"""Background terminals as a person and an agent use them: programs on PTYs the
goblin's supervisor owns, which keep running with nobody attached, are
attached and reattached with their screen, read, typed into and waited on,
and report their exit to the agent that asked. Names, limits, keys, terminal
queries and the sandbox endpoint's validation are Rust unit tests; fish's
startup query is a step of the interactive goblin workflow."""
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC
from support import step
from terminal_support import Terminal


def host_processes(marker):
    """PIDs of host-visible `sleep ARG` processes whose ARG starts with MARKER."""
    found = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            try:
                argv = (entry / "cmdline").read_bytes().split(b"\0")[:-1]
            except OSError:
                continue
            if (len(argv) == 2 and os.path.basename(argv[0]) == b"sleep"
                    and argv[1].decode(errors="replace").startswith(marker)):
                found.append(int(entry.name))
    return found


class BackgroundTerminalsWorkflow(unittest.TestCase):
    def setUp(self):
        self.d = Daemon(fixture_app("exec-goblins"))
        self.addCleanup(self.d.close)
        self.agent = self.d.start(agent_name="gribble")
        self.marker = str(time.time_ns() % 10**9)

    def command(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), "terminal", *args]

    def terminal(self, *args, **kwargs):
        return subprocess.run(self.command(*args), capture_output=True, text=True, timeout=30, **kwargs)

    def create(self, *args, goblin="gribble"):
        r = self.terminal("create", goblin, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip()

    def listed(self, goblin="gribble"):
        return self.d.rpc.call("terminals.list", {"session": goblin})["terminals"]

    def named(self, name):
        return next((t for t in self.listed() if t["name"] == name), None)

    def attach(self, terminal, goblin="gribble"):
        t = Terminal(self.command("attach", goblin, terminal))
        self.addCleanup(t.close)
        return t

    def inside(self, *args, goblin="gribble"):
        """`goblin terminal` in the goblin, as its agent runs it."""
        return subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), "exec", goblin, "--",
                               "goblin", "terminal", *args], capture_output=True, text=True, timeout=60)

    def next_notice(self):
        """The agent's next inbox message, claimed and completed, or None."""
        peer = RPC(self.d.state / self.agent["session"] / "resources/request.sock")
        try:
            claim = peer.call("inbox.next", {"key": uuid.uuid4().hex})
        finally:
            peer.close()
        if claim["message"] is None:
            return None
        peer = RPC(self.d.state / self.agent["session"] / "resources/request.sock")
        try:
            peer.call("inbox.complete", {"message": claim["message"]["id"], "key": uuid.uuid4().hex,
                                         "claim_generation": claim["claim_generation"]})
        finally:
            peer.close()
        return claim["message"]

    def test_background_terminals(self):
        d, marker = self.d, self.marker
        with step("a person's program keeps running with nobody attached and comes back with its screen"):
            # The goblin has no attach handlers: a terminal attaches all the same.
            self.assertEqual(self.create("--name", "app", "--", "sh", "-c",
                                         'echo FIRST; while read line; do echo "<$line>"; done'), "t1")
            record = self.named("app")
            self.assertEqual((record["id"], record["label"], record["origin"], record["attached"]),
                             ("t1", "app", "host", False))
            self.assertEqual((record["rows"], record["cols"]), (40, 120))
            t = self.attach("app")
            t.expect("FIRST")
            t.send("hello\n")
            t.expect("<hello>")
            d.wait(lambda: self.named("app")["attached"])
            second = subprocess.run(self.command("attach", "gribble", "t1"), capture_output=True, text=True,
                                    timeout=30, stdin=subprocess.DEVNULL)
            self.assertNotEqual(second.returncode, 0)
            # Detaching from elsewhere ends the frontend; the program runs on.
            self.assertEqual(self.terminal("detach", "gribble", "app").returncode, 0)
            t.expect("detached from t1")
            self.assertEqual(t.wait(), 0)
            d.wait(lambda: not self.named("app")["attached"])
            self.assertEqual(self.terminal("detach", "gribble", "app").returncode, 1)
            self.assertEqual(self.terminal("send", "gribble", "app", "--text", "while-away",
                                           "--keys", "enter").returncode, 0)
            d.wait(lambda: "<while-away>" in self.terminal("read", "gribble", "app").stdout)
            again = self.attach("t1")
            again.expect(r"FIRST\nhello\n<hello>\nwhile-away\n<while-away>")
            again.send("live\n")
            again.expect("<live>")
            # A killed frontend leaves the terminal and frees it.
            os.kill(again.pid, signal.SIGKILL)
            again.wait()
            d.wait(lambda: not self.named("app")["attached"])

        with step("a full-screen program is redrawn for the next frontend"):
            self.create("--name", "full", "--", "sh", "-c",
                        r'printf "\033[?1049h\033[2J\033[3;5HFULLSCREEN"; exec sleep 600')
            d.wait(lambda: d.rpc.call("terminals.read", {"session": "gribble", "terminal": "full"})["alternate_screen"])
            t = self.attach("full")
            t.expect("FULLSCREEN")
            os.kill(t.pid, signal.SIGKILL)
            t.wait()
            d.wait(lambda: not self.named("full")["attached"])
            self.attach("full").expect("FULLSCREEN")

        with step("run, wait, read and send from the host"):
            self.create("--name", "sh", "--size", "30x100")
            record = self.named("sh")
            self.assertEqual((record["rows"], record["cols"]), (30, 100))
            self.assertEqual(self.terminal("run", "gribble", "sh", "echo $((6*7))").returncode, 0)
            r = self.terminal("wait", "gribble", "sh", "--regex", "^42$")
            self.assertEqual((r.returncode, r.stdout), (0, "42\n"), r.stderr)
            # Output already on screen matches at once.
            self.assertEqual(self.terminal("wait", "gribble", "sh", "--match", "42", "--timeout", "50").returncode, 0)
            self.assertIn("42", self.terminal("read", "gribble", "sh").stdout.splitlines())
            self.terminal("send", "gribble", "sh", "--text", "echo kept", "--keys", "enter")
            self.terminal("wait", "gribble", "sh", "--regex", "^kept$")
            recent = self.terminal("read", "gribble", "sh", "--source", "recent", "--lines", "5").stdout
            self.assertIn("kept", recent.splitlines())
            self.terminal("send", "gribble", "sh", "--text", f"sleep {marker}0", "--keys", "enter")
            d.wait(lambda: host_processes(f"{marker}0"))
            self.terminal("send", "gribble", "sh", "--keys", "ctrl+c")
            self.terminal("run", "gribble", "sh", "echo after-interrupt")
            r = self.terminal("wait", "gribble", "sh", "--regex", "^after-interrupt$")
            self.assertEqual(r.returncode, 0, r.stderr)
            r = self.terminal("wait", "gribble", "sh", "--match", "NEVER-PRINTED", "--timeout", "300")
            self.assertEqual(r.returncode, 1)
            self.assertIn("timed out", r.stderr)
            self.assertNotEqual(self.terminal("read", "gribble", "t9").returncode, 0)
            r = self.terminal("send", "gribble", "sh", "--keys", "enter", "no-such-key")
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("unknown key", r.stderr)

        with step("a terminal and an exec run in a UTF-8 locale"):
            self.create("--name", "locale", "--", "bash", "-c",
                        'echo "L=$LC_ALL M=$(printf \'\\303\\251\' | wc -m)"; sleep 600')
            r = self.terminal("wait", "gribble", "locale", "--match", "L=C.UTF-8 M=1")
            self.assertEqual(r.returncode, 0, r.stderr + self.terminal("read", "gribble", "locale").stdout)

        with step("a wait for the exit gets the program's last output and code"):
            self.create("--name", "quick", "--", "sh", "-c", "read go; echo MARKER; exit 3")
            waiting = subprocess.Popen(self.command("wait", "gribble", "quick", "--exit"),
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(waiting.kill)
            self.terminal("send", "gribble", "quick", "--text", "go", "--keys", "enter")
            out, err = waiting.communicate(timeout=30)
            self.assertEqual(waiting.returncode, 0, err)
            self.assertIn("MARKER", out)
            self.assertIn("exited with code 3", err)
            d.wait(lambda: self.named("quick") is None)

        with step("closing a terminal ends its program and everything it started"):
            self.create("--name", "long", "--", "sh", "-c", f"sleep {marker}1 & exec sleep {marker}2")
            d.wait(lambda: len(host_processes(f"{marker}1") + host_processes(f"{marker}2")) == 2)
            self.assertEqual(self.terminal("close", "gribble", "long").returncode, 0)
            d.wait(lambda: self.named("long") is None)
            d.wait(lambda: not host_processes(f"{marker}1") and not host_processes(f"{marker}2"))
            self.create("--name", "killed", "--", "sh", "-c", "kill -9 $$")
            d.wait(lambda: self.named("killed") is None)

        with step("the agent creates and drives its own terminals"):
            r = self.inside("create", "--name", "srv", "--", "sh", "-c", "echo UP; read x; echo GOT-$x; sleep 600")
            self.assertEqual(r.returncode, 0, r.stderr)
            srv = r.stdout.strip()
            self.assertEqual((self.named("srv")["id"], self.named("srv")["origin"]), (srv, "agent"))
            listing = [line.split("\t")[:3] for line in self.inside("list").stdout.splitlines()]
            self.assertIn([srv, "srv", "agent"], listing)
            r = self.inside("wait", "srv", "--regex", "^UP$")
            self.assertEqual((r.returncode, r.stdout), (0, "UP\n"), r.stderr)
            self.assertEqual(self.inside("send", "srv", "--text", "hi", "--keys", "enter").returncode, 0)
            self.assertEqual(self.inside("wait", "srv", "--match", "GOT-hi").returncode, 0)
            self.assertIn("GOT-hi", self.inside("read", "srv").stdout.splitlines())
            r = self.inside("wait", "srv", "--match", "NEVER-PRINTED", "--timeout", "300")
            self.assertEqual(r.returncode, 1)
            self.assertIn("timed out", r.stderr)

        with step("the agent reads and types into the person's terminals but cannot close them"):
            self.assertEqual(self.inside("run", "sh", "echo $((6*8))").returncode, 0)
            self.assertEqual(self.inside("wait", "sh", "--regex", "^48$").returncode, 0)
            r = self.inside("close", "sh")
            self.assertEqual(r.returncode, 2)
            self.assertIn("only the human can close it", r.stderr)
            self.assertIsNotNone(self.named("sh"))
            self.assertEqual(self.inside("close", "srv").returncode, 0)
            d.wait(lambda: self.named("srv") is None)
            self.assertEqual(self.inside("read", "t99").returncode, 2)
            r = self.inside("send", "sh", "--keys", "no-such-key")
            self.assertEqual(r.returncode, 2)
            self.assertIn("unknown key", r.stderr)

        with step("another goblin's terminals are out of reach"):
            d.start(agent_name="snaga")
            self.assertEqual(self.create("--name", "theirs", "--", "sleep", "600", goblin="snaga"), "t1")
            r = self.inside("read", "theirs")
            self.assertEqual(r.returncode, 2)
            self.assertIn("no terminal theirs", r.stderr)

        with step("the agent gets one notice when a terminal it asked about exits"):
            # A terminal without --notify and a closed one send nothing, so
            # the next notice is the notified program's.
            self.assertEqual(self.inside("create", "--name", "quiet", "--", "sh", "-c", "exit 0").returncode, 0)
            self.assertEqual(self.inside("create", "--name", "closed", "--notify", "--", "sleep", "600").returncode, 0)
            self.assertEqual(self.inside("close", "closed").returncode, 0)
            d.wait(lambda: self.named("quiet") is None and self.named("closed") is None)
            self.assertEqual(self.inside("create", "--name", "job", "--notify", "--", "sh", "-c",
                                         "echo working; echo done-line; exit 3").returncode, 0)
            message = d.wait(self.next_notice)
            self.assertEqual(message["sender_name"], "goblins")
            operation = message["operation"]
            self.assertEqual((operation["kind"], operation["subject"], operation["status"]),
                             ("terminal", "job", "exited"))
            self.assertTrue(operation["message"].startswith("exit code 3\n"), operation)
            self.assertIn("done-line", operation["message"])
            self.assertIn("terminal job exited: exit code 3", message["body"])
            self.assertIsNone(self.next_notice())

        with step("the goblin stopping ends every terminal and the waits on them"):
            self.create("--name", "stays", "--", "sleep", f"{marker}3")
            d.wait(lambda: host_processes(f"{marker}3"))
            pending = subprocess.Popen(self.command("wait", "gribble", "stays", "--exit"),
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(pending.kill)
            d.rpc.call("sessions.stop", {"session": self.agent["session"]})
            pending.communicate(timeout=30)
            self.assertIsNotNone(pending.returncode)
            d.wait(lambda: d.get(self.agent["session"])["state"] == "stopped")
            self.assertEqual(d.get(self.agent["session"])["terminals"], [])
            d.wait(lambda: not host_processes(marker))
            self.assertNotEqual(self.terminal("read", "gribble", "stays").returncode, 0)


if __name__ == "__main__":
    unittest.main()
