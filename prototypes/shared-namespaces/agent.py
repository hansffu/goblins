"""Long-lived command runner inside one client sandbox.

Keeps the sandbox (and any Gradle daemons it starts) alive between commands,
like an agent's shell. Serves the fixture repository on this client's port.
"""
import json
import os
import socket
import subprocess
import sys
import time

port = int(sys.argv[1])
subprocess.Popen([sys.executable, '-m', 'http.server', str(port), '--bind', '127.0.0.1',
                  '--directory', '/repo'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
deadline = time.monotonic() + 10
while True:
    try:
        socket.create_connection(('127.0.0.1', port), timeout=1).close()
        break
    except OSError:
        if time.monotonic() > deadline:
            raise
        time.sleep(.05)


def text(value):
    return value.decode(errors='replace') if isinstance(value, bytes) else (value or '')


print(json.dumps({'event': 'ready', 'pidns': os.readlink('/proc/self/ns/pid'),
                  'netns': os.readlink('/proc/self/ns/net')}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    start = time.monotonic()
    try:
        result = subprocess.run(request['argv'], cwd='/workspace', capture_output=True,
                                text=True, timeout=request.get('timeout', 240))
        reply = {'code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
    except subprocess.TimeoutExpired as error:
        reply = {'code': None, 'timed_out': True, 'stdout': text(error.stdout), 'stderr': text(error.stderr)}
    reply['seconds'] = round(time.monotonic() - start, 1)
    print(json.dumps(reply), flush=True)
