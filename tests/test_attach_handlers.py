"""Attach handlers through a real daemon: a goblin with onAttach runs its main
command without a terminal and starts a client on a new PTY at each attach,
once the variables it needs were set during this run. Shell scripts stand in
for an agent's server and client."""
import fcntl
import json
from pathlib import Path
import socket
import struct
import subprocess
import sys
import termios
import time
import unittest
import uuid

from build_support import fixture_app, nix_bin
from daemon_support import Daemon, RPC, receive
from terminal_support import Terminal
from test_exec import host_processes

BASH = ["--noprofile", "--norc", "-c"]


def encode(value):
    body = json.dumps(value).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode() + body


class AttachHandlerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.application = fixture_app("exec-goblins")

    def setUp(self):
        self.d = Daemon(self.application)
        self.addCleanup(self.d.close)
        self.marker = "4" + str(time.time_ns() % 10**8)

    def manifest(self, client, server=None, on_detach=None, timeout=30, wait_for=(), resume=None):
        """The fixture's bash goblin with attach handlers."""
        config = json.loads(Path(self.d.manifest).read_text())
        goblin = config["goblins"]["shell"]
        goblin["args"] = BASH + [server or f"echo SERVER_UP; exec sleep {self.marker}0"]
        goblin["on_attach"] = {"command": ["bash"] + BASH + [client], "timeout": timeout,
                               "wait_for": list(wait_for)}
        goblin["on_detach"] = on_detach
        if resume is not None:
            goblin["resume_command"] = resume
        path = Path(self.d.temp.name) / f"attach-{uuid.uuid4().hex}.json"
        path.write_text(json.dumps(config))
        return str(path)

    def client(self, **options):
        """A client that reports its conversation and size, then waits."""
        return self.manifest(f"echo CLIENT={{session_id}},$(stty size | tr ' ' x); exec sleep {self.marker}1",
                             **options)

    def set_variables(self, session, **values):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call("sessions.set-variables", {"variables": values})
        finally:
            peer.close()

    def attach(self, session, rows=24, cols=100):
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", session])
        terminal.resize(rows, cols)
        self.addCleanup(terminal.close)
        return terminal

    def hello(self, session, rows=24, cols=100, version=1):
        """A raw frontend on attach.sock that has sent its hello."""
        peer = socket.socket(socket.AF_UNIX)
        peer.settimeout(10)
        peer.connect(self.d.get(session)["attach"])
        peer.sendall(encode({"jsonrpc": "2.0", "method": "attach.hello",
                             "params": {"version": version, "rows": rows, "cols": cols}}))
        self.addCleanup(peer.close)
        return peer

    def handshake(self, peer):
        """The reply after any waiting notifications, and how many there were."""
        notices = 0
        while (frame := receive(peer)).get("method") == "attach.waiting":
            notices += 1
        return frame, notices

    def client_size(self):
        """The rows and columns of the running client's terminal."""
        [pid] = host_processes(f"sleep {self.marker}1")
        with open(f"/proc/{pid}/fd/0", "rb", buffering=0) as tty:
            return struct.unpack("HHHH", fcntl.ioctl(tty, termios.TIOCGWINSZ, bytes(8)))[:2]

    def wait_gone(self, marker):
        self.d.wait(lambda: not host_processes(marker))

    def test_readiness_wait_reattach_size_and_one_attachment(self):
        launch = self.d.start(manifest=self.client())
        session = launch["session"]
        record = self.d.get(session)
        self.assertTrue(record["terminal_detached"])
        self.d.wait(lambda: "SERVER_UP" in Path(record["server_log"]).read_text())
        # Readiness taking longer than the CLI's 3 second handshake bound.
        cli = self.attach(session)
        cli.expect(r"Starting…")
        time.sleep(4)
        self.assertFalse(self.d.get(session)["terminal_attached"])
        self.set_variables(session, session_id="c819")
        cli.expect(r"CLIENT=c819,24x100\n")
        self.d.wait(lambda: self.d.get(session)["terminal_attached"])
        # One attachment: a second is refused and leaves the client's size.
        peer = self.hello(session, rows=10, cols=40)
        reply, _ = self.handshake(peer)
        self.assertEqual(reply["error"]["message"], "terminal is already attached")
        self.assertEqual(self.client_size(), (24, 100))
        # Detach stops the client; the server keeps running.
        self.d.cli("detach", session)
        self.assertEqual(cli.wait(), 0)
        self.wait_gone(f"sleep {self.marker}1")
        self.assertTrue(host_processes(f"sleep {self.marker}0"))
        self.assertTrue(self.d.get(session)["terminal_detached"])
        # Reattach from a differently sized terminal: a new client at that size.
        cli = self.attach(session, rows=30, cols=120)
        cli.expect(r"CLIENT=c819,30x120\n")

    def test_readiness_timeout_and_cleared_variables(self):
        launch = self.d.start(manifest=self.client(timeout=2, wait_for=["ready"]))
        session = launch["session"]
        # A seeded or cleared value is not ready.
        self.set_variables(session, session_id="c819", ready="yes")
        self.set_variables(session, ready="")
        peer = self.hello(session)
        reply, notices = self.handshake(peer)
        self.assertGreaterEqual(notices, 1)
        message = reply["error"]["message"]
        self.assertIn("ready", message)
        self.assertIn("server.log", message)
        self.assertIn("restart the goblin", message)
        self.assertFalse(host_processes(f"sleep {self.marker}1"))
        self.assertEqual(self.d.get(session)["state"], "running")
        # A frontend leaving during the wait starts nothing and frees the slot.
        peer = self.hello(session)
        self.assertEqual(receive(peer)["method"], "attach.waiting")
        peer.close()
        time.sleep(.5)
        self.set_variables(session, ready="yes")
        time.sleep(.5)
        self.assertFalse(host_processes(f"sleep {self.marker}1"))
        cli = self.attach(session)
        cli.expect(r"CLIENT=c819,24x100\n")

    def test_detach_cancels_a_stubborn_client_and_what_it_left(self):
        client = (f"set -m; env --ignore-signal=HUP --ignore-signal=TERM sleep {self.marker}2 & set +m; "
                  f"echo CLIENT_UP; exec env --ignore-signal=HUP --ignore-signal=TERM sleep {self.marker}1")
        launch = self.d.start(manifest=self.manifest(client, on_detach=[
            "sh", "-c", f"echo detach {{attach.pid}} > /tmp/on-detach-{self.marker}"]))
        session = launch["session"]
        self.set_variables(session, session_id="x")
        other = subprocess.Popen([str(self.d.app), "--state-dir", str(self.d.state), "exec", session, "--",
                                  "sleep", f"{self.marker}3"])
        self.addCleanup(other.kill)
        cli = self.attach(session)
        cli.expect(r"CLIENT_UP\n")
        self.d.wait(lambda: len(host_processes(f"sleep {self.marker}")) == 4)
        self.d.cli("detach", session)
        self.assertEqual(cli.wait(), 0)
        self.wait_gone(f"sleep {self.marker}1")
        self.wait_gone(f"sleep {self.marker}2")
        # onDetach ran with the client's PID; the main command and an
        # unrelated exec are untouched.
        written = subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), "exec", session, "--",
                                  "cat", f"/tmp/on-detach-{self.marker}"], capture_output=True, text=True).stdout
        self.assertRegex(written, r"^detach \d+\n$")
        self.assertTrue(host_processes(f"sleep {self.marker}0"))
        self.assertTrue(host_processes(f"sleep {self.marker}3"))
        self.assertTrue(self.d.get(session)["terminal_detached"])

    def test_client_exit_detaches_and_stop_cancels_the_client(self):
        launch = self.d.start(manifest=self.manifest(
            f"echo CLIENT_UP; read line; [ \"$line\" = quit ] && exit 0; exec sleep {self.marker}1"))
        session = launch["session"]
        self.set_variables(session, session_id="x")
        cli = self.attach(session)
        cli.expect(r"CLIENT_UP\n")
        cli.send("quit\n")
        self.assertEqual(cli.wait(), 0)
        record = self.d.wait(lambda: (r if (r := self.d.get(session))["terminal_detached"] else None))
        self.assertEqual(record["state"], "running")
        cli = self.attach(session)
        cli.expect(r"CLIENT_UP\n")
        cli.send("stay\n")
        self.d.wait(lambda: host_processes(f"sleep {self.marker}1"))
        self.d.cli("stop", session)
        cli.wait()
        self.wait_gone(f"sleep {self.marker}1")
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")

    def test_an_older_frontend_is_refused_and_single_terminal_goblins_take_both(self):
        launch = self.d.start(manifest=self.client())
        peer = socket.socket(socket.AF_UNIX)
        peer.settimeout(10)
        peer.connect(self.d.get(launch["session"])["terminal"])
        self.addCleanup(peer.close)
        self.assertIn("upgrade Goblins", receive(peer)["error"]["message"])
        # A goblin without onAttach attaches through terminal.sock as before
        # and through attach.sock with a hello.
        plain = self.d.start()
        old = self.d.terminal(dict(plain, terminal=self.d.get(plain["session"])["terminal"]))
        old.close()
        self.d.wait(lambda: not self.d.get(plain["session"])["terminal_attached"])
        cli = self.attach(plain["session"], rows=33, cols=90)
        cli.send("stty size\n")
        cli.expect(r"33 90\n")
        peer = self.hello(plain["session"])
        self.assertEqual(self.handshake(peer)[0]["error"]["message"], "terminal is already attached")
        self.assertEqual(self.hello(launch["session"], version=99).recv(4096).count(b"unsupported"), 1)

    def test_older_frontends_are_answered_during_a_slow_launch(self):
        """terminal.sock answers within an older frontend's 3 second bound
        although rooting the launch's manifest (store-backed for the plain
        goblin) and its session take longer."""
        tools = Path(self.d.temp.name) / "slow-bin"
        tools.mkdir()
        wrapper = tools / "nix-store"
        wrapper.write_text(f"""#!{sys.executable}
import os, sys, time
for kind in ("/manifests/", "/roots/"):
    once = os.path.join({str(tools)!r}, kind.strip("/"))
    if "--realise" in sys.argv and any(kind in a for a in sys.argv) and not os.path.exists(once):
        open(once, "w").close()
        time.sleep(5)
os.execv({str(nix_bin() / "nix-store")!r}, [{str(nix_bin() / "nix-store")!r}, *sys.argv[1:]])
""")
        wrapper.chmod(0o700)
        d = Daemon(self.application, tool_path=tools)
        self.addCleanup(d.close)
        self.d = d
        for manifest, answer in ((None, "attached"), (self.client(), "upgrade Goblins")):
            for kind in ("manifests", "roots"):
                (tools / kind).unlink(missing_ok=True)
            launch = d.start(manifest=manifest, wait=False)
            peer = socket.socket(socket.AF_UNIX)
            peer.settimeout(3)
            peer.connect(launch["terminal"])
            self.addCleanup(peer.close)
            reply = receive(peer)
            self.assertEqual(d.get(launch["session"])["state"], "starting")
            self.assertIn(answer, json.dumps(reply))
            d.wait(lambda: d.get(launch["session"])["state"] == "running", timeout=60)

    def test_recreate_uses_resume_command_and_waits_for_this_run(self):
        manifest = self.manifest(f"echo CLIENT={{session_id}}; exec sleep {self.marker}1",
                                 server=f"echo FRESH; exec sleep {self.marker}0",
                                 resume=BASH + [f"echo RESUMED={{session_id}}; exec sleep {self.marker}5"])
        launch = self.d.start(manifest=manifest)
        session = launch["session"]
        self.set_variables(session, session_id="c819")
        self.d.wait(lambda: self.d.record(session) and self.d.record(session)["variables"].get("session_id"))
        self.d.cli("stop", session)
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")
        record = self.d.record(session)
        again = self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": manifest,
                                                   "recreate": record["id"], "rows": 24, "cols": 100,
                                                   "detached": True})
        new = again["session"]
        self.d.wait(lambda: self.d.get(new)["state"] == "running", timeout=60)
        log = self.d.get(new)["server_log"]
        self.d.wait(lambda: "RESUMED=c819" in Path(log).read_text())
        # The recorded value is seeded but not ready until this run sets it.
        peer = self.hello(new)
        self.assertEqual(receive(peer)["method"], "attach.waiting")
        self.set_variables(new, session_id="c819")
        reply, _ = self.handshake(peer)
        self.assertTrue(reply["result"]["attached"])
        self.assertIn(b"CLIENT=c819", peer.recv(4096))


if __name__ == "__main__":
    unittest.main()
