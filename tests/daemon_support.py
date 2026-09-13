"""Test-only framed host/sandbox clients; production code is Rust."""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import uuid
from support import command

ROOT = Path(__file__).resolve().parents[1]

def frame(value):
    body = json.dumps(value).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode() + body


def receive(peer):
    header = bytearray()
    while not header.endswith(b"\r\n\r\n"):
        chunk = peer.recv(1)
        if not chunk:
            raise EOFError("missing RPC frame")
        header.extend(chunk)
        if len(header) > 256:
            raise ValueError("oversized header")
    length = int(header.decode().split(":", 1)[1].strip())
    body = bytearray()
    while len(body) < length:
        chunk = peer.recv(length - len(body))
        if not chunk:
            raise EOFError("incomplete RPC body")
        body.extend(chunk)
    return json.loads(body)


class RPC:
    def __init__(self, path):
        self.peer = socket.socket(socket.AF_UNIX)
        self.peer.settimeout(5)
        self.peer.connect(str(path))
        self.next = 0
        self.init = self.call("initialize", {"api": 1})

    def send(self, method, params):
        self.next += 1
        self.peer.sendall(frame({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}))
        return self.next

    def call(self, method, params):
        id = self.send(method, params)
        reply = receive(self.peer)
        assert reply["id"] == id, reply
        if "error" in reply:
            raise ValueError(reply["error"])
        return reply["result"]

    def close(self):
        self.peer.close()


def app():
    if "GOBLINS_APP" not in os.environ:
        os.environ["GOBLINS_APP"] = str(Path(command(["nix", "build", "--no-link", "--print-out-paths", "path:" + str(ROOT) + "#goblins"])) / "bin/goblins")
    return Path(os.environ["GOBLINS_APP"])


class Daemon:
    def __init__(self, application=None, env=None, workspace=None):
        self.temp = tempfile.TemporaryDirectory(prefix="gd-")
        self.state = Path(self.temp.name) / "state"
        self.app = application or app()
        self.binary, self.manifest = re.search(r'exec (\S+) --runtime (\S+)', self.app.read_text()).groups()
        argv = [self.binary, "--runtime", self.manifest, "--state-dir", str(self.state), "daemon"]
        if workspace:
            argv += ["--workspace", str(workspace)]
        self.process = subprocess.Popen(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        assert "daemon ready" in self.process.stdout.readline(), self.process.stderr.read()
        self.rpc = self.host()

    def host(self):
        return RPC(self.state / "host.sock")

    def wait(self, predicate, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            value = predicate()
            if value:
                return value
            time.sleep(.02)
        raise AssertionError("daemon condition timed out")

    def start(self, name="shell", manifest=None, key=None, wait=True, agent_name=None):
        result = self.rpc.call("sessions.start", {"key": key or uuid.uuid4().hex, "name": name, "agent_name": agent_name,
                                                  "configuration": str(manifest or self.manifest), "rows": 24, "cols": 100})
        if wait:
            record = self.wait(lambda: (r if (r := self.get(result["session"]))["state"] != "starting" else None))
            assert record["state"] == "running", record
        return result

    def get(self, id):
        return self.rpc.call("sessions.get", {"session": id})

    def permissions(self, **filters):
        return self.rpc.call("permissions.list", filters)

    def pending(self, session, package="hello"):
        peer = RPC(self.state / session / "resources/request.sock")
        peer.send("permissions.request", {"kind": "package", "package": package, "reason": "test"})
        record = self.wait(lambda: next(iter(self.permissions(session=session, state="pending")), None))
        return peer, record

    def decide(self, record, approved, rpc=None):
        return (rpc or self.rpc).call("permissions.decide", {"session": record["session"], "request": record["id"], "approval": record["approval"], "approved": approved})

    def terminal(self, launch):
        peer = socket.socket(socket.AF_UNIX)
        peer.settimeout(10)
        peer.connect(launch["terminal"])
        return peer

    def close(self):
        self.rpc.close()
        if self.process.poll() is None:
            self.process.terminate()
        self.process.wait(timeout=10)
        self.process.stdout.close()
        self.process.stderr.close()
        self.temp.cleanup()
