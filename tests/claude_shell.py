"""Run the actual hook script with controlled daemon and adapter endpoints."""
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def mock():
    root = Path(os.environ['TRACE'])
    args = sys.argv[1:]
    if ENDPOINT == 'claude':
        with (root / 'claude.log').open('a') as out:
            out.write(json.dumps({'args':args,'session':os.environ.get('GOBLINS_CLAUDE_SESSION'),
                                 'config':os.environ.get('CLAUDE_CONFIG_DIR')})+'\n')
        return
    if args == ['start']:
        with (root / 'starts.log').open('a') as out:
            out.write('{}\n')
        return
    if ENDPOINT == 'goblin':
        with (root / 'variables.log').open('a') as out:
            out.write(json.dumps(args) + '\n')
        sys.exit(1 if (root / 'fail').exists() else 0)
    raw = sys.stdin.read()
    with (root / 'hooks.log').open('a') as out:
        out.write(json.dumps({'args': args, 'raw': json.loads(raw)}) + '\n')
    if (root / 'hook-fail').exists():
        sys.exit(1)
    print('{"decision":"block"}')


def main():
    hook, mock_bin, own, bypass = map(Path, sys.argv[1:])
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        runtime = root / 'runtime'
        env = {**os.environ, 'TRACE':temp, 'HOOK_RUNTIME':str(runtime),
               'HOME':temp, 'PATH':str(mock_bin / 'bin') + os.pathsep + os.environ['PATH']}
        def run(*args, payload=None, extra=None):
            result = subprocess.run([str(hook),*args], input=json.dumps(payload or {}).encode(),
                                    env=env | (extra or {}), capture_output=True, timeout=8)
            assert result.returncode == 0, result.stderr
            return result
        def rows(name):
            path = root / name
            return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        def cache():
            return json.loads((runtime / 'variables.json').read_text())
        def latest(value):
            (runtime / 'latest.json').write_text(json.dumps(value))
        def report(value):
            latest(value)
            run('--report-variables')
        for launcher, permissions in [(own,False),(bypass,True)]:
            for nested in [False,True]:
                settings = env | {'GOBLINS_WORKTREES':'/work trees', 'GOBLINS_CHECKOUT':'/main checkout'}
                if nested: settings['GOBLINS_CLAUDE_SESSION'] = 'main'
                starts = len(rows('starts.log'))
                result = subprocess.run([str(launcher / 'bin/claude'),'literal $(false) argument'],
                                        env=settings,capture_output=True,timeout=3)
                assert result.returncode == 0, result.stderr
                actual = rows('claude.log')[-1]
                expected = [] if nested else ['--add-dir','/work trees','--add-dir','/main checkout',
                                              '--plugin-dir','/fixture plugin']
                expected += (['--dangerously-skip-permissions'] if permissions else []) + ['literal $(false) argument']
                assert actual['args'] == expected
                assert actual['session'] == ('nested' if nested else 'main')
                assert actual['config'] == str(root / '.claude')
                assert len(rows('starts.log')) == starts + (0 if nested else 1)
        # Nested hooks leave activity, inbox and conversation metadata alone.
        for event in ['Notification','Stop','SessionStart']:
            assert json.loads(run(payload={'hook_event_name':event},extra={'GOBLINS_CLAUDE_SESSION':'nested'}).stdout) == {}
        assert not runtime.exists() and not rows('hooks.log')
        runtime.mkdir(mode=0o700)
        transcript = root / 'transcript.jsonl'
        transcript.write_text('\n'.join(json.dumps(item) for item in [
            {'type':'ai-title','aiTitle':'AI one'}, {'type':'custom-title','customTitle':'Mine'},
            {'type':'ai-title','aiTitle':'AI two'}])+'\nmalformed\n')
        report({'session_id':'one','transcript_path':str(transcript)})
        assert cache() == {'session_id':'one','session_name':'Mine'}
        before = len(rows('variables.log'))
        report({'session_id':'one','transcript_path':str(root / 'missing')})
        assert len(rows('variables.log')) == before
        report({'session_id':'one','session_title':'Renamed\tline\n'+'🐟'*80})
        assert cache()['session_name'] == ('Renamed line '+'🐟'*80)[:64]
        report({'session_id':'two','transcript_path':str(root / 'missing')})
        assert cache() == {'session_id':'two','session_name':''}
        (root / 'fail').touch()
        report({'session_id':'three','session_title':'retry'})
        assert cache()['session_id'] == 'two'
        (root / 'fail').unlink()
        report({'session_id':'three','session_title':'retry'})
        assert cache() == {'session_id':'three','session_name':'retry'}
        assert rows('variables.log')[-2:] == [['variable','set','session_id=three','session_name=retry']]*2
        # Waiters read the latest input after acquiring the lock and update once.
        with (runtime / 'lock').open('w') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)
            latest({'session_id':'obsolete','session_title':'old'})
            first = subprocess.Popen([str(hook),'--report-variables'],env=env)
            latest({'session_id':'latest','session_title':'new'})
            second = subprocess.Popen([str(hook),'--report-variables'],env=env)
            fcntl.flock(lock,fcntl.LOCK_UN)
            assert first.wait(timeout=8) == second.wait(timeout=8) == 0
        assert cache() == {'session_id':'latest','session_name':'new'}
        assert sum('session_id=latest' in row for row in rows('variables.log')) == 1
        # A blocked transcript reader cannot hold up an inbox decision. Feed
        # its FIFO after observing the real reader, rather than waiting 7 s.
        fifo = root / 'slow.jsonl'
        os.mkfifo(fifo)
        started = time.monotonic()
        result = run(payload={'hook_event_name':'Stop','session_id':'fifo','transcript_path':str(fifo)})
        assert json.loads(result.stdout) == {'decision':'block'}
        assert time.monotonic()-started < 2.5
        deadline = time.monotonic()+5
        while True:
            try:
                writer = os.open(fifo,os.O_WRONLY|os.O_NONBLOCK)
                break
            except OSError:
                assert time.monotonic()<deadline, 'background transcript reader did not start'
                time.sleep(.01)
        os.write(writer,b'{"type":"ai-title","aiTitle":"FIFO title"}\n')
        os.close(writer)
        while not (runtime / 'variables.json').exists() or cache().get('session_id') != 'fifo':
            assert time.monotonic()<deadline, 'background report did not finish'
            time.sleep(.01)
        assert cache()['session_name'] == 'FIFO title'
        # The monitor PID must identify the monitor, not a stale or unrelated process.
        monitor_file = root / 'goblins-claude-monitor'
        monitor_file.write_text('#!/bin/sh\nexec sleep 300\n')
        monitor_file.chmod(0o700)
        # A shell retaining the script in argv is visible in /proc/cmdline.
        monitor_file.write_text('#!/bin/sh\nsleep 300 &\nwait\n')
        monitor = subprocess.Popen([str(monitor_file)], start_new_session=True)
        try:
            for pid, expected in [(str(monitor.pid),['hook','--monitor']), ('not-a-pid',['hook']),
                                  (str(os.getpid()),['hook']),('999999999',['hook'])]:
                (runtime / 'monitor.pid').write_text(pid)
                run(payload={'hook_event_name':'Stop'})
                assert rows('hooks.log')[-1]['args'] == expected
        finally:
            os.killpg(monitor.pid,15)
            monitor.wait(timeout=3)
        (root / 'hook-fail').touch()
        assert json.loads(run(payload={'hook_event_name':'SessionStart'}).stdout) == {}
        assert 'no inbox item was consumed' in json.loads(run(payload={'hook_event_name':'Stop'}).stdout)['systemMessage']
        assert json.loads(run(payload={'hook_event_name':'PostToolUse'}).stdout) == {}
        # Synchronize with any final detached reporter before deleting its files.
        with (runtime / 'lock').open('w') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)


if __name__ == 'claude_mock':
    mock()
elif __name__ == '__main__':
    main()
