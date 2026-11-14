"""A server agent with attach handlers, through a real daemon: a goblin with
onAttach runs its main command without a terminal and starts a client on a new
PTY at each attach, once the variables it needs were set during this run.
Shell scripts stand in for an agent's server and client. The handshake's
bounds, the readiness timeout and refused hellos are Rust unit tests; older
frontends on terminal.sock are not supported (no backward compatibility)."""
import fcntl
import json
from pathlib import Path
import socket
import struct
import subprocess
import termios
import time
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC, receive
from support import step
from terminal_support import Terminal
from test_exec import host_processes
from api_contract import validate_completion, validate_notification

BASH = ["--noprofile", "--norc", "-c"]


def encode(value):
    body = json.dumps(value).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode() + body


class ServerAgentWorkflow(unittest.TestCase):
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
            validate_notification("attach", "server-to-client", frame)
            notices += 1
        validate_completion("attach.reply", frame)
        return frame, notices

    def client_size(self):
        """The rows and columns of the running client's terminal."""
        [pid] = host_processes(f"sleep {self.marker}1")
        with open(f"/proc/{pid}/fd/0", "rb", buffering=0) as tty:
            return struct.unpack("HHHH", fcntl.ioctl(tty, termios.TIOCGWINSZ, bytes(8)))[:2]

    def wait_gone(self, marker):
        self.d.wait(lambda: not host_processes(marker))

    def test_a_server_agent_with_attach_handlers(self):
        d, marker = self.d, self.marker
        # The client reports its conversation and size; "quit" ends it,
        # anything else leaves it running.
        manifest = self.manifest(
            f"echo CLIENT={{session_id}},$(stty size | tr ' ' x); read line; "
            f"[ \"$line\" = quit ] && exit 0; exec sleep {marker}1",
            server=f"echo SERVER_UP; exec sleep {marker}0",
            resume=BASH + [f"echo RESUMED={{session_id}}; exec sleep {marker}5"])
        with step("the server starts without a terminal"):
            session = d.start(manifest=manifest)["session"]
            record = d.get(session)
            self.assertTrue(record["terminal_detached"])
            d.wait(lambda: "SERVER_UP" in Path(record["server_log"]).read_text())

        with step("attaching says it is starting until the agent's variables are set"):
            cli = self.attach(session)
            cli.expect(r"Starting…")
            self.assertFalse(d.get(session)["terminal_attached"])
            self.set_variables(session, session_id="c819")
            cli.expect(r"CLIENT=c819,24x100\n")
            d.wait(lambda: d.get(session)["terminal_attached"])
            cli.send("stay\n")
            d.wait(lambda: host_processes(f"sleep {marker}1"))

        with step("a second attachment is refused and leaves the client's size"):
            reply, _ = self.handshake(self.hello(session, rows=10, cols=40))
            self.assertEqual(reply["error"]["message"], "terminal is already attached")
            self.assertEqual(self.client_size(), (24, 100))

        with step("detach ends the client; the server keeps running"):
            d.cli("detach", session)
            self.assertEqual(cli.wait(), 0)
            self.wait_gone(f"sleep {marker}1")
            self.assertTrue(host_processes(f"sleep {marker}0"))
            self.assertTrue(d.get(session)["terminal_detached"])

        with step("reattaching from a larger terminal starts a client at that size"):
            cli = self.attach(session, rows=30, cols=120)
            cli.expect(r"CLIENT=c819,30x120\n")

        with step("the client quitting detaches"):
            cli.send("quit\n")
            self.assertEqual(cli.wait(), 0)
            record = d.wait(lambda: (r if (r := d.get(session))["terminal_detached"] else None))
            self.assertEqual(record["state"], "running")

        with step("stopping the goblin ends its client"):
            d.wait(lambda: d.record(session) and d.record(session)["variables"].get("session_id"))
            cli = self.attach(session)
            cli.expect(r"CLIENT=c819")
            cli.send("stay\n")
            d.wait(lambda: host_processes(f"sleep {marker}1"))
            d.cli("stop", session)
            cli.wait()
            self.wait_gone(f"sleep {marker}1")
            d.wait(lambda: d.get(session)["state"] == "stopped")

        with step("resume runs the resume command and waits for this run's variables"):
            record = d.record(session)
            d.wait_record_released(record["id"])
            again = d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": manifest,
                                                  "recreate": record["id"], "rows": 24, "cols": 100,
                                                  "detached": True})["session"]
            d.wait(lambda: d.get(again)["state"] == "running", timeout=60)
            d.wait(lambda: "RESUMED=c819" in Path(d.get(again)["server_log"]).read_text())
            # The recorded value is seeded, but not ready until this run sets it.
            peer = self.hello(again)
            self.assertEqual(receive(peer)["method"], "attach.waiting")
            self.set_variables(again, session_id="c819")
            reply, _ = self.handshake(peer)
            self.assertTrue(reply["result"]["attached"])
            self.assertIn(b"CLIENT=c819", peer.recv(4096))

    def test_detach_runs_on_detach_and_ends_a_stubborn_client(self):
        d, marker = self.d, self.marker
        client = (f"set -m; env --ignore-signal=HUP --ignore-signal=TERM sleep {marker}2 & set +m; "
                  f"echo CLIENT_UP; exec env --ignore-signal=HUP --ignore-signal=TERM sleep {marker}1")
        session = d.start(manifest=self.manifest(client, on_detach=[
            "sh", "-c", f"echo detach {{attach.pid}} > /tmp/on-detach-{marker}"]))["session"]
        self.set_variables(session, session_id="x")
        other = subprocess.Popen([str(d.app), "--state-dir", str(d.state), "exec", session, "--",
                                  "sleep", f"{marker}3"])
        self.addCleanup(other.kill)
        cli = self.attach(session)
        cli.expect(r"CLIENT_UP\n")
        d.wait(lambda: len(host_processes(f"sleep {marker}")) == 4)
        d.cli("detach", session)
        self.assertEqual(cli.wait(), 0)
        # The client and what it left in its own process group are gone.
        self.wait_gone(f"sleep {marker}1")
        self.wait_gone(f"sleep {marker}2")
        # onDetach ran with the client's PID; the main command and an
        # unrelated exec are untouched.
        written = subprocess.run([str(d.app), "--state-dir", str(d.state), "exec", session, "--",
                                  "cat", f"/tmp/on-detach-{marker}"], capture_output=True, text=True).stdout
        self.assertRegex(written, r"^detach \d+\n$")
        self.assertTrue(host_processes(f"sleep {marker}0"))
        self.assertTrue(host_processes(f"sleep {marker}3"))
        self.assertTrue(d.get(session)["terminal_detached"])


if __name__ == "__main__":
    unittest.main()
