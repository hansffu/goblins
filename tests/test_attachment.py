"""A person works in an interactive goblin: runs it in a project, detaches and
reattaches, interrupts and resizes it, and kills it, while another goblin
carries on. Implementation checks (pinned attachments, signals during the
handshake, detach validation) are Rust unit tests."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import termios
import time
import unittest

from daemon_support import Daemon
from support import step
from terminal_support import Terminal


class InteractiveGoblinWorkflow(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)
        temporary = tempfile.TemporaryDirectory(prefix="goblin-project-")
        self.addCleanup(temporary.cleanup)
        self.project = Path(temporary.name) / "first project"
        (self.project / "deep/nested").mkdir(parents=True)

    def host(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), *args]

    def cli(self, *args):
        return subprocess.run(self.host(*args), capture_output=True, text=True, timeout=60)

    def terminal(self, *args, cwd=None):
        terminal = Terminal(self.host(*args), cwd=cwd)
        self.addCleanup(terminal.close)
        return terminal

    def raw(self, terminal):
        """Wait until the attached client made its terminal raw: from then on
        Ctrl-C is relayed to the goblin instead of interrupting the client."""
        self.d.wait(lambda: not termios.tcgetattr(terminal.fd)[3] & termios.ISIG)

    def test_an_interactive_goblin(self):
        d = self.d
        with step("the configurations to choose from"):
            options = json.loads(subprocess.check_output([str(d.app), "configurations"], text=True))
            self.assertEqual(options["configuration"], d.manifest)
            self.assertEqual(options["descriptions"]["shell"], "fish running in a Goblins sandbox")

        with step("another goblin runs beside it"):
            grub = d.start(agent_name="grub")["session"]
            other = self.terminal("attach", "grub")
            other.send("echo OTHER_READY\n")
            other.expect(r"(?:^|\n)OTHER_READY\n")

        with step("a goblin runs in the project and edits go both ways"):
            (self.project / "deep/nested/input").write_text("initial\n")
            client = self.terminal("run", "shell", "--name", "snikk", cwd=self.project)
            client.send("printf 'CWD=%s\\n' $PWD; cat deep/nested/input; echo written > deep/nested/output\n")
            self.assertEqual(client.expect(r"(?:^|\n)CWD=([^\n]+)\n").group(1).decode(), str(self.project))
            client.expect(r"(?:^|\n)initial\n")
            d.wait(lambda: (self.project / "deep/nested/output").exists())
            self.assertEqual((self.project / "deep/nested/output").read_text(), "written\n")
            (self.project / "deep/nested/input").write_text("host-update\n")
            client.send("cat deep/nested/input\n")
            client.expect(r"(?:^|\n)host-update\n")
            snikk = next(s["id"] for s in d.rpc.call("sessions.list", {}) if s["agent_name"] == "snikk")
            identity = d.get(snikk)["identity"]

        with step("goblin detach leaves the shell running, also writing output, and reattach finds its state"):
            client.send("set -g saved_value retained; cd /tmp; printf 'PID=%s\\n' $fish_pid\n")
            pid = client.expect(r"(?:^|\n)PID=(\d+)\n").group(1).decode()
            client.send("goblin detach; echo $status > /tmp/detached-code; string repeat -n 200000 x; "
                        "touch /tmp/output-finished\n")
            self.assertEqual(client.wait(), 0)
            # /proc shows the sandbox's root without giving the goblin anything.
            root = Path(f"/proc/{identity['pid']}/root/tmp")
            d.wait(lambda: (root / "output-finished").exists())
            self.assertEqual((root / "detached-code").read_text(), "0\n")
            record = d.get(snikk)
            self.assertEqual((record["state"], record["identity"]), ("running", identity))
            self.assertTrue(record["terminal_detached"])
            client = self.terminal("attach", "snikk")
            client.send("printf 'STATE=%s,%s,%s\\n' $fish_pid $saved_value $PWD\n")
            client.expect(rf"(?:^|\n)STATE={pid},retained,/tmp\n")

        with step("the host detaches a busy goblin, by name and by id"):
            self.assertEqual(self.cli("detach", "missing-agent").returncode, 1)
            for target in ("snikk", snikk):
                client.send("echo BUSY; sleep 60\n")
                client.expect(r"(?:^|\n)BUSY\n")
                self.assertTrue(json.loads(self.cli("detach", target).stdout)["accepted"])
                self.assertEqual(client.wait(), 0)
                record = d.get(snikk)
                self.assertEqual((record["state"], record["identity"]), ("running", identity))
                self.assertTrue(d.get(grub)["terminal_attached"])
                again = self.cli("detach", target)
                self.assertEqual(again.returncode, 1)
                self.assertIn("terminal is not attached", again.stderr)
                client = self.terminal("attach", "snikk")
                self.raw(client)
                # Ctrl-C reaches the busy foreground job, not the client.
                client.send("\x03")
                client.send("printf 'STATE=%s,%s\\n' $fish_pid $saved_value\n")
                client.expect(rf"(?:^|\n)STATE={pid},retained\n")

        with step("a second attachment is refused"):
            second = self.terminal("attach", "snikk")
            second.expect("terminal is already attached")
            self.assertEqual(second.wait(), 1)

        with step("the goblin follows the terminal's size"):
            client.resize(33, 91)
            deadline = time.monotonic() + 10
            while True:
                client.send("stty size\n")
                if client.expect(r"(?:^|\n)(\d+ \d+)\n").group(1) == b"33 91":
                    break
                self.assertLess(time.monotonic(), deadline, "the resize never reached the goblin")

        with step("fish in a background terminal nobody attached to is not left waiting for a terminal answer"):
            # Fish asks its terminal for the device attributes at startup and
            # reads no input until it is answered or ten seconds passed.
            started = time.monotonic()
            created = self.cli("terminal", "create", "grub", "--name", "fish", "--", "fish")
            self.assertEqual(created.returncode, 0, created.stderr)
            self.cli("terminal", "send", "grub", "fish", "--text", "echo ready", "--keys", "enter")
            waited = self.cli("terminal", "wait", "grub", "fish", "--regex", "^ready$", "--timeout", "8000")
            self.assertEqual(waited.returncode, 0, waited.stderr)
            self.assertLess(time.monotonic() - started, 8)
            screen = self.cli("terminal", "read", "grub", "fish", "--source", "recent").stdout
            self.assertNotIn("could not read response", screen)
            self.assertNotIn("warning", screen)

        with step("killing an attached goblin ends its client and leaves the other running"):
            killed = json.loads(self.cli("kill", "snikk").stdout)
            self.assertTrue(killed["accepted"])
            d.wait(lambda: d.get(snikk)["state"] == "stopped")
            # A killed payload has no normal exit status; its client still ends.
            self.assertNotEqual(client.wait(), 0)
            d.wait(lambda: not (d.state / snikk / "resources").exists())
            self.assertEqual(d.get(grub)["state"], "running")

        with step("the other goblin's exit code reaches its client"):
            other.send("echo OTHER_ALIVE; exit 7\n")
            other.expect(r"(?:^|\n)OTHER_ALIVE\n")
            self.assertEqual(other.wait(), 7)


if __name__ == "__main__":
    unittest.main()
