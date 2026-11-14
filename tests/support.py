"""Test-only bridge to the Rust Session library; no Python controller logic.

The ignored Rust test driver supplies payload pipes and the request listener by
SCM_RIGHTS. Original payload probes still execute in the actual namespace.
"""
import array
import contextlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parents[1]
# The repository as a flake: tracked files only (untracked ones need git add),
# so builds never copy target/ or other ignored files into the store again.
FLAKE = "git+file://" + str(HERE)


@contextlib.contextmanager
def step(name):
    """Name the workflow step in a failure, so one long test still says where it broke."""
    try:
        yield
    except AssertionError as error:
        raise AssertionError(f"step '{name}': {error}") from error


def cases(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from cases(test)
        else:
            yield test


def cargo_artifacts(*args):
    # Keep machine-readable artifact paths on stdout and compiler diagnostics
    # on inherited stderr, including when check_output raises on a failed build.
    output = subprocess.check_output([
        os.environ.get("CARGO", "cargo"), *args, "--message-format=json-render-diagnostics",
    ], cwd=HERE, text=True)
    return [json.loads(line) for line in output.splitlines()]


def rust_driver():
    if "GOBLINS_TEST_DRIVER" not in os.environ:
        # Match test-rust's workspace feature selection so Cargo reuses artifacts.
        for artifact in cargo_artifacts("test", "--locked", "--workspace", "--no-run"):
            if (artifact.get("reason") == "compiler-artifact" and artifact.get("executable")
                    and artifact["target"]["name"] == "goblins_supervisor"):
                os.environ["GOBLINS_TEST_DRIVER"] = artifact["executable"]
        if "GOBLINS_TEST_DRIVER" not in os.environ:
            raise RuntimeError("Cargo did not produce the Rust host test driver")
    return os.environ["GOBLINS_TEST_DRIVER"]


def command(args):
    result = subprocess.run([str(x) for x in args], text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {args}\n{result.stderr[-12000:]}")
    return result.stdout.strip()



def pinned_nixpkgs():
    """This repository's locked nixpkgs: the flake URL naming it and its lock node."""
    lock = json.loads((HERE / "flake.lock").read_text())
    node = lock["nodes"][lock["nodes"][lock["root"]]["inputs"]["nixpkgs"]]
    original = node["original"]
    return f"github:{original['owner']}/{original['repo']}/{original['rev']}", node


def lock_flake(directory):
    """Lock a test flake whose input `nixpkgs` is pinned_nixpkgs()'s URL.

    Its lock node is copied from this repository's flake.lock, so Nix finds
    the pinned source in the store by its hash, offline and without hashing
    the tree on every command as it does for a path: input. Nix locks any
    other inputs."""
    _, node = pinned_nixpkgs()
    (Path(directory) / "flake.lock").write_text(json.dumps({
        "nodes": {"nixpkgs": {"locked": node["locked"], "original": node["original"]},
                  "root": {"inputs": {"nixpkgs": "nixpkgs"}}},
        "root": "root", "version": 7}, indent=2) + "\n")
    command(["nix", "flake", "lock", "--offline", f"path:{directory}"])

def request(**updates):
    return {"id": "r1", "package": "jq", "reason": "test", **updates}


class Process:
    def __init__(self, session):
        self.session = session

    @property
    def pid(self):
        return self.session.state["pid"]

    def poll(self):
        self.session.rpc("state")
        return None if self.session.state["alive"] else 1


class Session:
    def __init__(self, workspace=None, **config):
        self.python = Path(config["python"])
        self.driver = subprocess.Popen(
            [rust_driver(), "session::tests::host_driver", "--ignored", "--exact", "--nocapture"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
        )
        self.closed = False
        self.input = self.output = self.socket = None
        self.fault = None
        self.proc = Process(self)
        self.rpc("new", config=config, workspace=str(workspace) if workspace else None)

    def rpc(self, op, **args):
        self.driver.stdin.write(json.dumps({"op": op, **args}) + "\n")
        self.driver.stdin.flush()
        while True:
            line = self.driver.stdout.readline()
            if not line:
                raise RuntimeError("Rust test driver exited")
            if line.startswith("DRIVER "):
                break
        result = json.loads(line[7:])
        self.state = result["state"]
        if self.state:
            self.directory = Path(self.state["directory"])
            self.identity = self.state["identity"]
            self.mounted = set(map(Path, self.state["mounted"]))
            self.packages = self.state["packages"]
        if "error" in result:
            error = result["error"]
            raise (ValueError if "collision" in error or op == "parse" else RuntimeError)(error)
        return result["value"]

    def start(self):
        with tempfile.TemporaryDirectory(prefix="goblins-test-fds-") as temp, socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as listener:
            endpoint = str(Path(temp) / "fds.sock")
            listener.bind(endpoint)
            listener.listen()
            self.rpc("start", socket=endpoint)
            peer, _ = listener.accept()
            received = []
            with peer:
                for _ in range(3):
                    _, ancillary, flags, _ = peer.recvmsg(4096, socket.CMSG_SPACE(4))
                    assert not flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC)
                    fds = array.array("i")
                    fds.frombytes(ancillary[0][2])
                    received.append(fds[0])
            self.input = os.fdopen(received[0], "w", buffering=1)
            self.output = os.fdopen(received[1], "r", buffering=1)
            self.socket = socket.socket(fileno=received[2])
            self.socket.settimeout(15)
        return self

    def closure(self, paths):
        return set(map(Path, self.rpc("closure", paths=list(map(str, paths)))))

    def grant(self, name, path=None, fail_after=None):
        return self.rpc("grant", name=name, path=str(path) if path else None, fail_after=fail_after, fault=self.fault)

    def decide(self, req, approved):
        return self.rpc("decide", request=req, approved=approved)

    @property
    def flake(self):
        return None

    @flake.setter
    def flake(self, value):
        self.rpc("flake", value=value)

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.rpc("close")
        for stream in (self.input, self.output, self.socket, self.driver.stdin):
            if stream:
                stream.close()
        self.driver.stdout.read()
        self.driver.stdout.close()
        self.driver.wait(timeout=5)


def receive(conn):
    """Test-only handshake with the real restricted request executable."""
    from daemon_support import receive as framed, frame
    conn.settimeout(3)
    init = framed(conn)
    assert init["method"] == "initialize"
    fixture = json.loads((HERE / "libs/api/tests/fixtures.json").read_text())["sandbox_initialize"]
    conn.sendall(frame({"jsonrpc":"2.0", "id":init["id"], "result":fixture}))
    req = framed(conn)
    assert req["method"] == "permissions.request"
    return request(id=req["id"], package=req["params"]["package"], reason=req["params"]["reason"])
