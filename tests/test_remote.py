"""A person manages a goblin on another machine from the CLI: `--host` runs
the same command there through ssh. `fake-ssh` stands in for ssh, so the other
machine is this one; quoting and option handling are Rust unit tests."""
import json
import os
from pathlib import Path
import select
import subprocess
import time
import unittest

from daemon_support import Daemon, frame
from support import step
from terminal_support import Terminal

FAKE_SSH = Path(__file__).resolve().parent / "fake-ssh"
# Quotes, a backslash and a space: what the remote login shell could mangle.
WORD = "a'b\\c d"


class Frames:
    """JSON-RPC frames from a pipe, with a deadline on each."""
    def __init__(self, pipe):
        self.fd = pipe.fileno()
        self.buffer = b""

    def next(self, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            header, separator, rest = self.buffer.partition(b"\r\n\r\n")
            if separator:
                length = int(header.decode().split(":", 1)[1])
                if len(rest) >= length:
                    self.buffer = rest[length:]
                    return json.loads(rest[:length])
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.fd], [], [], remaining)[0]:
                raise AssertionError(f"no frame arrived: {self.buffer[-2000:]!r}")
            chunk = os.read(self.fd, 65536)
            if not chunk:
                raise AssertionError(f"the connection ended: {self.buffer[-2000:]!r}")
            self.buffer += chunk


class RemoteGoblinWorkflow(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)
        self.env = {**os.environ, "GOBLINS_SSH": str(FAKE_SSH)}

    def remote(self, *args):
        app = str(self.d.app)
        return [app, "--host", "fake", "--remote-program", app, "--state-dir", str(self.d.state), *args]

    def cli(self, *args):
        return subprocess.run(self.remote(*args), env=self.env, capture_output=True, text=True, timeout=60)

    def test_manage_a_goblin_on_another_machine_from_the_cli(self):
        d = self.d
        d.start(agent_name="grub")

        with step("the command line goes to the other machine without this machine's options"):
            # echo as the remote program shows what the other machine is asked
            # to run; the wrapper's own --runtime is not part of it.
            sent = subprocess.run([str(d.app), "--host", "fake", "--remote-program", "echo",
                                   "--state-dir", str(d.state), "list"],
                                  env=self.env, capture_output=True, text=True, timeout=60)
            self.assertEqual((sent.returncode, sent.stdout), (0, f"--state-dir {d.state} list\n"), sent.stderr)

        with step("the goblins on the other machine are listed"):
            listed = self.cli("list")
            self.assertEqual(listed.returncode, 0, listed.stderr)
            self.assertIn("grub", listed.stdout)

        with step("a command's arguments and exit status cross unchanged"):
            echoed = self.cli("exec", "grub", "--", "sh", "-c", 'printf %s "$1"', "sh", WORD)
            self.assertEqual((echoed.returncode, echoed.stdout), (0, WORD), echoed.stderr)
            self.assertEqual(self.cli("exec", "grub", "--", "sh", "-c", "exit 7").returncode, 7)

        with step("a program follows the other machine's state"):
            client = subprocess.Popen(self.remote("rpc", "--stdio"), env=self.env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.addCleanup(client.kill)
            frames = Frames(client.stdout)

            def call(id, method, params):
                client.stdin.write(frame({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                client.stdin.flush()
                reply = frames.next()
                self.assertEqual(reply["id"], id, reply)
                return reply["result"]

            self.assertEqual(call(1, "initialize", {"api": 1})["role"], "host")
            subscription = call(2, "state.subscribe", {})["subscription"]
            d.start(agent_name="snikk")
            # A launch publishes more than once; one of them names the goblin.
            deadline = time.monotonic() + 60
            while True:
                change = frames.next()
                self.assertEqual(change["method"], "state.changed", change)
                self.assertEqual(change["params"]["subscription"], subscription)
                if "snikk" in json.dumps(change["params"]["snapshot"]):
                    break
                self.assertLess(time.monotonic(), deadline, "the new goblin never appeared")
            client.stdin.close()
            self.assertEqual(client.wait(timeout=30), 0, client.stderr.read())
            client.stdout.close()
            client.stderr.close()

        with step("the goblin's terminal opens through the other machine"):
            terminal = Terminal(self.remote("attach", "grub"), env=self.env)
            self.addCleanup(terminal.close)
            terminal.send("echo REMOTE_READY\n")
            terminal.expect(r"(?:^|\n)REMOTE_READY\n")
            terminal.send("exit 7\n")
            self.assertEqual(terminal.wait(), 7)
