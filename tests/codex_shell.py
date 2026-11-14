"""Exercise generated launchers and the MCP filter without an agent engine."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile


def record(name, value):
    Path(os.environ['TRACE'], name + '.json').write_text(json.dumps(value))


def codex_mock():
    args = sys.argv[1:]
    if args == ['--version']:
        print(os.environ.get('VERSION', 'codex 0.163.0'))
        return
    record('codex', {'args': args, 'home': os.environ.get('CODEX_HOME'), 'cwd': os.getcwd()})
    if args[:1] == ['app-server']:
        record('server', {'args': args, 'pid': os.getpid()})
        endpoint = socket.socket(socket.AF_UNIX)
        endpoint.bind(args[2].removeprefix('unix://'))
        signal.pause()
    elif args[-3:] == ['mcp', 'list', '--json']:
        if os.environ.get('MCP_FAIL'):
            sys.exit(1)
        print(Path(os.environ['MCP_LIST']).read_text())


def adapter_mock():
    args = sys.argv[1:]
    if args[:1] == ['profile-args']:
        assert args[1:] == ['literal profile $(false)']
        sys.stdout.buffer.write(b'model_providers={fixture={base_url="http://local"}}\0features={plugins=false}\0')
    else:
        record('adapter', args)
        sys.exit(int(os.environ.get('ADAPTER_EXIT', '0')))


def main():
    own, bypass, mcp, mock = map(Path, sys.argv[1:])
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        env = {**os.environ, 'TRACE': temp, 'HOME': str(root / 'home')}
        runtime = Path('/tmp/goblins-codex')
        # This check runs in a Nix build's private /tmp. Refuse existing data.
        assert not runtime.exists(), runtime
        def run(path, *args, code=0, extra=None):
            result = subprocess.run([str(path), *args], env=env | (extra or {}), capture_output=True, timeout=12)
            assert result.returncode == code, (args, result.returncode, result.stderr)
            return result
        for launcher, permissions in [(own, False), (bypass, True)]:
            for invalid in [('--sandbox', 'read-only'), ('--thread',), ('--config',), ('--profile',), ('--',)]:
                run(launcher / 'bin/goblins-codex-server', *invalid, code=2)
                assert not runtime.exists()
            run(launcher / 'bin/goblins-codex-server', code=1, extra={'VERSION': '0.162.9'})
            assert not runtime.exists()
            literal = 'model="literal $(touch /tmp/should-not-execute) {model}"'
            result = run(launcher / 'bin/goblins-codex-server', '--profile', 'literal profile $(false)',
                         '--config', literal, '--enable=plugins', '--disable', 'other', '--thread', 'saved thread', code=1)
            assert b'adapter stopped (0)' in result.stderr
            server = json.loads((root / 'server.json').read_text())
            args = server['args']
            assert args[:3] == ['app-server', '--listen', 'unix:///tmp/goblins-codex/app-server.sock']
            assert args[3:7] == ['-c', 'model_providers={fixture={base_url="http://local"}}', '-c', 'features={plugins=false}']
            assert args[7:12] == ['--config', literal, '--enable=plugins', '--disable', 'other']
            assert args[12:] == ['-c', 'sandbox_mode="danger-full-access"'] + (['-c', 'approval_policy="never"'] if permissions else []) + ['--enable', 'hooks']
            assert json.loads((root / 'adapter.json').read_text()) == ['run', '--socket', '/tmp/goblins-codex/app-server.sock', '--thread', 'saved thread'] + (['--bypass-permissions'] if permissions else [])
            assert not Path('/proc', str(server['pid'])).exists(), 'launcher did not reap server'
            assert not Path('/tmp/should-not-execute').exists()
            shutil.rmtree(runtime)
            run(launcher / 'bin/goblins-codex-attach', 'saved thread')
            client = json.loads((root / 'codex.json').read_text())
            assert client['args'] == ['--remote', 'unix:///tmp/goblins-codex/app-server.sock', 'resume', 'saved thread']
            assert client['home'] == str(root / 'home' / '.codex')
            run(launcher / 'bin/codex', 'exec', 'literal $(false)')
            assert json.loads((root / 'codex.json').read_text())['args'] == ['--sandbox', 'danger-full-access'] + (['--ask-for-approval', 'never'] if permissions else []) + ['--enable', 'hooks', 'exec', 'literal $(false)']
        # The actual filter consumes effective native configuration, including
        # plugin entries, and emits only safe command/enabled overrides.
        project = root / 'project with spaces'
        project.mkdir()
        bindir = project / 'bin'
        bindir.mkdir()
        available = bindir / 'available'
        available.write_text('#!/bin/sh\nexit 0\n')
        available.chmod(0o700)
        def entry(name, command=None, **transport):
            return {'name': name, 'enabled': True, 'transport': {'type': 'stdio', 'command': command, **transport}}
        missing = '/missing/node "quoted" \\backslash'
        listing = [entry('missing', missing, env={'SECRET':'do not copy'}),
                   entry('absolute', str(available)), entry('relative', './available', cwd='bin'),
                   entry('path', 'available', env={'PATH':str(bindir)}),
                   entry('dotted.name', '/missing/dotted'),
                   {**entry('disabled','/missing/disabled'), 'enabled':False},
                   {'name':'http', 'enabled':True, 'transport':{'type':'streamableHttp','url':'https://fixture.invalid'}}]
        path = root / 'mcp.json'
        path.write_text(json.dumps(listing))
        harness = root / 'filter.sh'
        harness.write_text('codex_executable=$1\nshift\nsource "$MCP_SCRIPT"\nprintf "%s\\0" "${mcp_overrides[@]}"\n')
        result = run(Path(shutil.which('bash')), str(harness), str(mock / 'bin/codex'), '-p', 'native profile',
                     '--config=model="literal $(false)"', '--cd', str(project), 'exec', code=0,
                     extra={'MCP_LIST':str(path),'MCP_SCRIPT':str(mcp)})
        fields = result.stdout.split(b'\0')[:-1]
        assert fields == [b'-c', ('mcp_servers.missing.command=' + json.dumps(missing, separators=(',',':'))).encode(), b'-c', b'mcp_servers.missing.enabled=false'], fields
        assert b'dotted name cannot be overridden' in result.stderr
        assert b'SECRET' not in result.stdout
        probe = json.loads((root / 'codex.json').read_text())
        assert probe['cwd'] == str(project)
        assert probe['args'] == ['-p','native profile','--config=model="literal $(false)"','mcp','list','--json']
        failed = run(Path(shutil.which('bash')), str(harness), str(mock / 'bin/codex'), code=0,
                     extra={'MCP_LIST':str(path),'MCP_SCRIPT':str(mcp),'MCP_FAIL':'1'})
        assert b'continuing with native settings' in failed.stderr
        assert failed.stdout in (b'', b'\0')


if __name__ == 'codex_mock':
    codex_mock()
elif __name__ == 'adapter_mock':
    adapter_mock()
elif __name__ == '__main__':
    main()
