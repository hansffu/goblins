#!/usr/bin/env python3
"""Does sharing a scope's network/PID namespaces let client-side Gradle builds share state?

Each variant starts a scope holder owning a user, network and PID namespace.
Client sandboxes always enter the holder's user namespace, then either join or
unshare its network and PID namespaces. Gradle runs inside the clients; there
is no execution service. Each client uses a private daemon registry.

Run: python3 prototypes/shared-namespaces/experiment.py --output /tmp/shared-ns
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import zipfile

HERE = Path(__file__).resolve().parent
PYTHON = str(Path(sys.executable).resolve())
BASH = str(Path(shutil.which('bash')).resolve())
GRADLE = str(Path(shutil.which('gradle')).resolve())
JAVA = str(Path(shutil.which('java')).resolve())
JAVA_HOME = str(Path(JAVA).parent.parent)
PATH = ':'.join(str(Path(x).parent) for x in (PYTHON, BASH, JAVA, GRADLE))

VARIANTS = {
    'isolated': {'net': False, 'pid': False},
    'shared-net': {'net': True, 'pid': False},
    'shared-net-pid': {'net': True, 'pid': True},
}


def wait_for(predicate, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(.1)
    return False


class Holder:
    """Owns the scope namespaces; killing it tears down the shared PID namespace."""
    def __init__(self):
        self.proc = subprocess.Popen(
            ['unshare', '--user', '--map-root-user', '--net', '--pid', '--fork', '--kill-child',
             '--mount', '--mount-proc', 'sh', '-c', 'ip link set lo up && exec sleep infinity'])

        def child():
            path = Path(f'/proc/{self.proc.pid}/task/{self.proc.pid}/children')
            return path.exists() and path.read_text().split()

        if not wait_for(child, 5):
            raise RuntimeError('scope holder did not start')
        self.pid = int(child()[0])
        time.sleep(.2)  # let the holder bring up loopback

    def close(self):
        self.proc.kill()
        self.proc.wait(timeout=10)


class Agent:
    def __init__(self, holder, variant, name, home, workspace, repo, port, log_dir):
        self.name, self.port = name, port
        share = VARIANTS[variant]
        enter = ['nsenter', '-t', str(holder.pid), '-U', '--preserve-credentials']
        enter += ['-n'] if share['net'] else []
        enter += ['-p'] if share['pid'] else []
        sandbox = [shutil.which('bwrap'), '--unshare-ipc', '--unshare-uts', '--die-with-parent',
                   '--new-session']
        sandbox += [] if share['net'] else ['--unshare-net']
        sandbox += [] if share['pid'] else ['--unshare-pid']
        sandbox += ['--ro-bind', '/nix/store', '/nix/store', '--proc', '/proc', '--dev', '/dev',
                    '--tmpfs', '/tmp', '--dir', '/bin', '--ro-bind', BASH, '/bin/sh',
                    '--tmpfs', '/home/agent', '--ro-bind', str(HERE), '/prototype',
                    '--bind', str(home), '/gradle-home', '--bind', str(workspace), '/workspace',
                    '--ro-bind', str(repo), '/repo', '--clearenv',
                    '--setenv', 'PATH', PATH, '--setenv', 'HOME', '/home/agent',
                    '--setenv', 'JAVA_HOME', JAVA_HOME, '--setenv', 'LANG', 'C.UTF-8',
                    '--setenv', 'GRADLE_USER_HOME', '/gradle-home', '--chdir', '/workspace']
        self.log = (log_dir / f'agent-{name}.log').open('w')
        self.proc = subprocess.Popen(enter + ['--'] + sandbox + ['--', PYTHON, '/prototype/agent.py', str(port)],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
                                     text=True, bufsize=1)
        self.lock = threading.Lock()
        self.info = json.loads(self.proc.stdout.readline())

    def run(self, argv, timeout=240):
        with self.lock:
            self.proc.stdin.write(json.dumps({'argv': argv, 'timeout': timeout}) + '\n')
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f'agent {self.name} exited')
            return json.loads(line)

    def gradle(self, *tasks, timeout=240):
        return self.run([GRADLE, '--daemon', '--console=plain', '--stacktrace',
                         '-Dorg.gradle.daemon.registry.base=/tmp/daemon-registry',
                         f'-PrepoPort={self.port}', f'-Pclient={self.name}', *tasks], timeout)

    def process_cmdline(self, pid):
        """What this client sees at a PID reported by Gradle (usually the lock owner)."""
        result = self.run([PYTHON, '-c', 'import sys\ntry:\n print(open(f"/proc/{sys.argv[1]}/cmdline","rb")'
                           '.read().replace(b"\\0",b" ").decode()[:160])\nexcept OSError as e: print("ERROR", e)',
                           str(pid)], 10)
        return result['stdout'].strip()

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.log.close()


def summarize(result, observer=None):
    output = result['stdout'] + result['stderr']
    summary = {
        'code': result['code'], 'seconds': result['seconds'],
        'timed_out': result.get('timed_out', False),
        'daemon_pid': next(iter(re.findall(r'DAEMON_PID (\d+)', output)), None),
        'lock_timeouts': sorted(set(re.findall(r'Timeout waiting to lock ([^\n]*)', output))),
        'owner_pids': sorted(set(re.findall(r'Owner PID: (\d+)', output))),
    }
    if observer:
        summary['owner_seen_as'] = {pid: observer.process_cmdline(pid) for pid in summary['owner_pids']}
    return summary


def build_workspace(path):
    path.mkdir(parents=True)
    (path / 'settings.gradle').write_text("rootProject.name = 'probe'\n")
    shutil.copyfile(HERE / 'build.gradle', path / 'build.gradle')


def build_repository(repo):
    artifact = repo / 'example/sample/1.0'
    artifact.mkdir(parents=True)
    (artifact / 'sample-1.0.pom').write_text(
        '<project><modelVersion>4.0.0</modelVersion><groupId>example</groupId>'
        '<artifactId>sample</artifactId><version>1.0</version></project>')
    with zipfile.ZipFile(artifact / 'sample-1.0.jar', 'w') as jar:
        jar.writestr('marker.txt', 'generated dependency fixture\n')


def scenario(root, variant, repo, shared_home, shared_workspace, steps):
    root.mkdir(parents=True)
    holder = Holder()
    agents = []
    try:
        for name, port in (('a', 18081), ('b', 18082)):
            home = root / ('home' if shared_home else f'home-{name}')
            workspace = root / ('workspace' if shared_workspace else f'workspace-{name}')
            home.mkdir(exist_ok=True)
            if not workspace.exists():
                build_workspace(workspace)
            agents.append(Agent(holder, variant, name, home, workspace, repo, port, root))
        a, b = agents
        record = {'namespaces': {agent.name: agent.info for agent in agents}}
        record.update(steps(a, b, root / 'workspace' if shared_workspace else None))
        return record
    finally:
        for agent in agents:
            agent.close()
        holder.close()


def idle_daemon_steps(a, b, _):
    """Shared Gradle home, separate checkouts: A's idle daemon vs B's first build."""
    first = summarize(a.gradle('resolve'))
    second = summarize(b.gradle('resolve'), observer=b)
    third = summarize(a.gradle('resolve'), observer=a)
    return {'a_first': first, 'b_while_a_idle': second, 'a_while_b_idle': third}


def concurrent_steps(a, b, workspace):
    """Same checkout, optionally shared home: B builds while A's build runs, then while A idles."""
    holding = {}
    thread = threading.Thread(target=lambda: holding.update(a.gradle('hold', '-Pduration=20')))
    thread.start()
    started = wait_for(lambda: (workspace / 'started-a').exists(), 120)
    during = summarize(b.gradle('resolve'), observer=b)
    thread.join()
    after = summarize(b.gradle('resolve'), observer=b)
    again = summarize(a.gradle('resolve'), observer=a)
    return {'a_hold_started': started, 'a_hold': summarize(holding),
            'b_during_a_build': during, 'b_while_a_idle': after, 'a_while_b_idle': again}


SCENARIOS = {
    # name: (shared home, shared workspace, steps)
    'shared-home': (True, False, idle_daemon_steps),
    'same-checkout': (False, True, concurrent_steps),
    'shared-home-same-checkout': (True, True, concurrent_steps),
}


def verdict(step):
    if step['timed_out']:
        return 'HUNG'
    if step['code'] != 0:
        return 'FAIL' + (' (lock timeout)' if step['lock_timeouts'] else '')
    return 'ok'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--variant', action='append', choices=list(VARIANTS))
    parser.add_argument('--scenario', action='append', choices=list(SCENARIOS))
    args = parser.parse_args()
    args.output.mkdir()
    repo = args.output / 'repository'
    build_repository(repo)
    evidence = {'gradle': subprocess.run([GRADLE, '--version'], capture_output=True, text=True).stdout,
                'results': {}}
    for variant in args.variant or VARIANTS:
        for name in args.scenario or SCENARIOS:
            shared_home, shared_workspace, steps = SCENARIOS[name]
            print(f'== {variant} / {name}', flush=True)
            try:
                record = scenario(args.output / variant / name, variant, repo,
                                  shared_home, shared_workspace, steps)
            except Exception as error:  # record and continue with other variants
                record = {'error': repr(error)}
            evidence['results'].setdefault(variant, {})[name] = record
            for step, value in record.items():
                if isinstance(value, dict) and 'code' in value:
                    print(f'   {step:18} {verdict(value):22} {value["seconds"]:>6}s '
                          f'locks={value["lock_timeouts"]} owners={value.get("owner_seen_as", {})}', flush=True)
                elif step == 'error':
                    print(f'   ERROR {value}', flush=True)
            (args.output / 'results.json').write_text(json.dumps(evidence, indent=2))


if __name__ == '__main__':
    main()
