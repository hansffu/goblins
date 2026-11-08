"""Terminals in a sandbox against a real daemon: programs on PTYs the goblin's
supervisor owns, which keep running with nobody attached, are attached and
re-attached with their screen, and are read, typed into and waited on from the
host CLI."""
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest
import uuid

from daemon_support import Daemon, RPC
from terminal_support import Terminal
from build_support import fixture_app


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


class TerminalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.application = fixture_app("exec-goblins")
        cls.d = Daemon(cls.application)
        cls.addClassCleanup(cls.d.close)

    def setUp(self):
        self.addCleanup(self.stop_sessions)
        self.agent = self.d.start(agent_name="gribble")

    def stop_sessions(self):
        sessions = self.d.rpc.call("sessions.list", {})
        for session in sessions:
            if session["state"] in ("starting", "running"):
                self.d.rpc.call("sessions.stop", {"session": session["id"]})
        self.d.wait(lambda: all(not (self.d.state / s["id"] / "resources").exists() for s in sessions))
        for session in sessions:
            record = self.d.wait(lambda: self.d.record(session["id"]))
            self.d.wait_record_released(record["id"])

    def command(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), "terminal", *args]

    def terminal(self, *args, **kw):
        return subprocess.run(self.command(*args), capture_output=True, text=True, timeout=30, **kw)

    def create(self, *args, goblin="gribble"):
        r = self.terminal("create", goblin, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip()

    def listed(self, goblin="gribble"):
        return self.d.rpc.call("terminals.list", {"session": goblin})["terminals"]

    def attach(self, terminal, goblin="gribble"):
        t = Terminal(self.command("attach", goblin, terminal))
        self.addCleanup(t.close)
        return t

    def test_a_terminal_outlives_its_frontend_and_comes_back_with_its_screen(self):
        # The goblin has no attach handlers: a terminal attaches all the same.
        self.assertIsNone(self.d.get(self.agent["session"])["server_log"])
        self.assertEqual(self.create("--name", "app", "--", "sh", "-c",
                                     'echo FIRST; while read line; do echo "<$line>"; done'), "t1")
        (record,) = self.listed()
        self.assertEqual((record["id"], record["name"], record["label"], record["origin"], record["attached"]),
                         ("t1", "app", "app", "host", False))
        self.assertEqual((record["rows"], record["cols"]), (40, 120))
        t = self.attach("app")
        t.expect("FIRST")
        t.send("hello\n")
        t.expect("<hello>")
        self.d.wait(lambda: self.listed()[0]["attached"])
        # One frontend at a time.
        second = subprocess.run(self.command("attach", "gribble", "t1"), capture_output=True, text=True,
                                timeout=30, stdin=open("/dev/null"))
        self.assertNotEqual(second.returncode, 0)
        # Detaching from elsewhere ends the stream; the terminal runs on.
        self.assertEqual(self.terminal("detach", "gribble", "app").returncode, 0)
        t.expect("detached from t1")
        self.assertEqual(t.wait(), 0)
        self.d.wait(lambda: not self.listed()[0]["attached"])
        self.assertEqual(self.terminal("detach", "gribble", "app").returncode, 1)
        self.assertEqual(self.terminal("send", "gribble", "app", "--text", "while-away",
                                       "--keys", "enter").returncode, 0)
        self.d.wait(lambda: "<while-away>" in self.terminal("read", "gribble", "app").stdout)
        # The next frontend gets the whole screen, old and new.
        again = self.attach("t1")
        again.expect(r"FIRST\nhello\n<hello>\nwhile-away\n<while-away>")
        again.send("live\n")
        again.expect("<live>")
        # Killing the frontend leaves the terminal and frees it.
        os.kill(again.pid, signal.SIGKILL)
        again.wait()
        self.d.wait(lambda: not self.listed()[0]["attached"])
        self.assertEqual(len(self.listed()), 1)

    def test_a_full_screen_program_is_redrawn_on_attach(self):
        self.create("--name", "full", "--", "sh", "-c",
                    r'printf "\033[?1049h\033[2J\033[3;5HFULLSCREEN\033[?2004h"; exec sleep 600')
        self.d.wait(lambda: self.d.rpc.call("terminals.read", {"session": "gribble", "terminal": "full"})["alternate_screen"])
        read = self.d.rpc.call("terminals.read", {"session": "gribble", "terminal": "full"})
        self.assertIn("FULLSCREEN", read["text"])
        t = self.attach("full")
        t.expect("FULLSCREEN")
        os.kill(t.pid, signal.SIGKILL)
        t.wait()
        self.d.wait(lambda: not self.listed()[0]["attached"])
        again = self.attach("full")
        again.expect("FULLSCREEN")

    def test_read_send_run_and_wait_from_the_cli(self):
        self.assertEqual(self.create("--name", "sh", "--size", "30x100"), "t1")
        (record,) = self.listed()
        self.assertEqual((record["rows"], record["cols"]), (30, 100))
        # The sandbox's own shell: run a command line, wait for its output.
        self.assertEqual(self.terminal("run", "gribble", "sh", "echo $((6*7))").returncode, 0)
        r = self.terminal("wait", "gribble", "sh", "--regex", "^42$")
        self.assertEqual((r.returncode, r.stdout), (0, "42\n"), r.stderr)
        # Output already on screen matches at once.
        r = self.terminal("wait", "gribble", "t1", "--match", "42", "--timeout", "50")
        self.assertEqual(r.returncode, 0, r.stderr)
        read = self.terminal("read", "gribble", "t1")
        self.assertIn("42", read.stdout.splitlines())
        # Typed text and keys; recent lines.
        self.terminal("send", "gribble", "t1", "--text", "echo kept", "--keys", "enter")
        self.terminal("wait", "gribble", "t1", "--regex", "^kept$")
        recent = self.terminal("read", "gribble", "t1", "--source", "recent", "--lines", "5").stdout
        self.assertIn("kept", recent.splitlines())
        self.terminal("send", "gribble", "t1", "--text", "sleep 600", "--keys", "enter")
        time.sleep(.3)
        self.terminal("send", "gribble", "t1", "--keys", "ctrl+c")
        self.terminal("run", "gribble", "t1", "echo after-interrupt")
        r = self.terminal("wait", "gribble", "t1", "--regex", "^after-interrupt$")
        self.assertEqual(r.returncode, 0, r.stderr)
        # A wait that nothing satisfies times out with 1.
        r = self.terminal("wait", "gribble", "t1", "--match", "NEVER-PRINTED", "--timeout", "300")
        self.assertEqual(r.returncode, 1)
        self.assertIn("timed out", r.stderr)
        # Unknown terminals, keys and conditions are refused.
        self.assertNotEqual(self.terminal("read", "gribble", "t9").returncode, 0)
        r = self.terminal("send", "gribble", "t1", "--keys", "enter", "no-such-key")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("unknown key", r.stderr)
        self.assertNotEqual(self.terminal("wait", "gribble", "t1").returncode, 0)

    def test_names_sizes_and_the_number_of_terminals_are_limited(self):
        for name in ("t1", "T", "9a", "a b"):
            r = self.terminal("create", "gribble", "--name", name)
            self.assertNotEqual(r.returncode, 0, name)
        self.assertNotEqual(self.terminal("create", "gribble", "--size", "501x80").returncode, 0)
        self.assertEqual(self.listed(), [])
        self.create("--name", "one", "--", "sleep", "600")
        r = self.terminal("create", "gribble", "--name", "one", "--", "sleep", "600")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("exists", r.stderr)
        for _ in range(15):
            self.create("--", "sleep", "600")
        r = self.terminal("create", "gribble", "--", "sleep", "600")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("16", r.stderr)
        # IDs count up for the goblin's run and are never reused.
        self.terminal("close", "gribble", "one")
        self.d.wait(lambda: len(self.listed()) == 15)
        self.assertEqual(self.create("--", "sleep", "600"), "t17")

    def test_a_terminal_ends_when_its_program_exits_is_closed_or_the_goblin_stops(self):
        # Exits: a wait for the exit gets the output the program left.
        self.create("--name", "quick", "--", "sh", "-c", "read go; echo MARKER; exit 3")
        waiting = subprocess.Popen(self.command("wait", "gribble", "quick", "--exit"),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        time.sleep(.5)
        self.terminal("send", "gribble", "quick", "--text", "go", "--keys", "enter")
        out, err = waiting.communicate(timeout=30)
        self.assertEqual(waiting.returncode, 0, err)
        self.assertIn("MARKER", out)
        self.assertIn("exited with code 3", err)
        self.d.wait(lambda: self.listed() == [])
        # Closing ends the program and everything it started.
        marker = "5" + str(time.time_ns() % 10**8)
        self.create("--name", "long", "--", "sh", "-c", f"sleep {marker}1 & exec sleep {marker}2")
        self.d.wait(lambda: len(host_processes(f"{marker}")) == 2)
        self.assertEqual(self.terminal("close", "gribble", "long").returncode, 0)
        self.d.wait(lambda: self.listed() == [])
        self.d.wait(lambda: not host_processes(marker))
        # A signal ends it with no code.
        self.create("--name", "killed", "--", "sh", "-c", "kill -9 $$")
        self.d.wait(lambda: self.listed() == [])
        # The goblin stopping ends every terminal and the waits on them.
        marker = "6" + str(time.time_ns() % 10**8)
        self.create("--name", "stays", "--", "sleep", f"{marker}")
        pending = subprocess.Popen(self.command("wait", "gribble", "stays", "--exit"),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        time.sleep(.5)
        self.d.rpc.call("sessions.stop", {"session": self.agent["session"]})
        pending.communicate(timeout=30)
        self.assertNotEqual(pending.returncode, None)
        self.d.wait(lambda: self.d.get(self.agent["session"])["state"] == "stopped")
        self.assertEqual(self.d.get(self.agent["session"])["terminals"], [])
        self.d.wait(lambda: not host_processes(marker))
        self.assertNotEqual(self.terminal("read", "gribble", "stays").returncode, 0)

    def test_every_goblins_terminals_are_its_own(self):
        other = self.d.start(agent_name="snaga")
        self.create("--name", "mine", "--", "sleep", "600")
        self.assertEqual(self.d.rpc.call("terminals.list", {"session": "snaga"})["terminals"], [])
        r = self.terminal("read", "snaga", "mine")
        self.assertNotEqual(r.returncode, 0)
        # The same name may exist in each.
        self.assertEqual(self.create("--name", "mine", "--", "sleep", "600", goblin="snaga"), "t1")
        self.assertEqual(other["agent_name"], "snaga")

    # What a goblin does with `goblin terminal` and the sandbox endpoint.

    def inside(self, *args, goblin="gribble"):
        return subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), "exec", goblin, "--",
                               "goblin", "terminal", *args], capture_output=True, text=True, timeout=60)

    def sandbox(self, method, params, session=None):
        # The request socket answers one call per connection.
        peer = RPC(self.d.state / (session or self.agent["session"]) / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def notice(self, timeout=30):
        """The next inbox message of gribble, claimed and completed."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            claim = self.sandbox("inbox.next", {"key": uuid.uuid4().hex})
            if claim["message"] is not None:
                self.sandbox("inbox.complete", {"message": claim["message"]["id"], "key": uuid.uuid4().hex,
                                                "claim_generation": claim["claim_generation"]})
                return claim["message"]
            time.sleep(.2)
        self.fail("no notice arrived")

    def test_a_goblin_creates_and_drives_its_own_terminals_and_closes_only_its_own(self):
        r = self.inside("create", "--name", "srv", "--", "sh", "-c", "echo UP; read x; echo GOT-$x; sleep 600")
        self.assertEqual((r.returncode, r.stdout), (0, "t1\n"), r.stderr)
        (record,) = self.listed()
        self.assertEqual((record["id"], record["name"], record["origin"]), ("t1", "srv", "agent"))
        r = self.inside("list")
        self.assertEqual(r.stdout.split("\t")[:3], ["t1", "srv", "agent"], r.stderr)
        r = self.inside("wait", "srv", "--regex", "^UP$")
        self.assertEqual((r.returncode, r.stdout), (0, "UP\n"), r.stderr)
        self.assertEqual(self.inside("send", "srv", "--text", "hi", "--keys", "enter").returncode, 0)
        r = self.inside("wait", "srv", "--match", "GOT-hi")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("GOT-hi", self.inside("read", "srv").stdout.splitlines())
        r = self.inside("wait", "srv", "--match", "NEVER-PRINTED", "--timeout", "300")
        self.assertEqual(r.returncode, 1)
        self.assertIn("timed out", r.stderr)
        # The human's terminals are the goblin's to read and type into, not to close.
        human = self.d.rpc.call("terminals.create", {"session": "gribble", "name": "human"})
        self.assertEqual(human["origin"], "host")
        self.assertEqual(self.inside("run", "human", "echo $((6*7))").returncode, 0)
        r = self.inside("wait", "human", "--regex", "^42$")
        self.assertEqual(r.returncode, 0, r.stderr)
        r = self.inside("close", "human")
        self.assertEqual(r.returncode, 2)
        self.assertIn("only the human can close it", r.stderr)
        self.assertEqual(len(self.listed()), 2)
        self.assertEqual(self.inside("close", "srv").returncode, 0)
        self.d.wait(lambda: [t["name"] for t in self.listed()] == ["human"])
        # An unknown terminal and an unknown key are refused.
        self.assertEqual(self.inside("read", "t9").returncode, 2)
        r = self.inside("send", "human", "--keys", "no-such-key")
        self.assertEqual(r.returncode, 2)
        self.assertIn("unknown key", r.stderr)

    def test_a_goblin_reaches_only_its_own_terminals_and_has_no_attachment_methods(self):
        self.d.start(agent_name="snaga")
        self.d.rpc.call("terminals.create", {"session": "snaga", "name": "theirs"})
        self.d.rpc.call("terminals.create", {"session": "gribble", "name": "mine"})
        self.assertEqual([t["name"] for t in self.sandbox("terminals.list", {})["terminals"]], ["mine"])
        r = self.inside("read", "theirs")
        self.assertEqual(r.returncode, 2)
        self.assertIn("no terminal theirs", r.stderr)
        # No call names a goblin, and attaching, detaching and resizing do not exist.
        for method, params, code in [
            ("terminals.list", {"session": "snaga"}, -32602),
            ("terminals.read", {"session": "snaga", "terminal": "theirs"}, -32602),
            ("terminals.attach", {"terminal": "mine"}, -32601),
            ("terminals.detach", {"terminal": "mine"}, -32601),
            ("terminals.resize", {"terminal": "mine", "rows": 20, "cols": 80}, -32601),
        ]:
            with self.assertRaises(ValueError, msg=method) as caught:
                self.sandbox(method, params)
            self.assertEqual(caught.exception.args[0]["code"], code, method)
        # The notice is the goblin's alone to ask for.
        with self.assertRaises(ValueError) as caught:
            self.d.rpc.call("terminals.create", {"session": "gribble", "notify": True})
        self.assertEqual(caught.exception.args[0]["code"], -32602)

    def test_notify_puts_one_notice_in_the_inbox_when_the_program_exits(self):
        r = self.inside("create", "--name", "job", "--notify", "--", "sh", "-c",
                        "echo working; echo done-line; exit 3")
        self.assertEqual(r.returncode, 0, r.stderr)
        message = self.notice()
        self.assertEqual(message["sender_name"], "goblins")
        operation = message["operation"]
        self.assertEqual((operation["kind"], operation["subject"], operation["status"]),
                         ("terminal", "job", "exited"))
        self.assertTrue(operation["message"].startswith("exit code 3\n"), operation)
        self.assertIn("done-line", operation["message"])
        self.assertIn("terminal job exited: exit code 3", message["body"])
        # Closing a terminal sends none: the next notice is the next program's.
        self.assertEqual(self.inside("create", "--name", "closed", "--notify", "--", "sleep", "600").returncode, 0)
        self.assertEqual(self.inside("close", "closed").returncode, 0)
        self.d.wait(lambda: self.listed() == [])
        self.assertEqual(self.inside("create", "--name", "second", "--notify", "--", "sh", "-c",
                                     "echo bye").returncode, 0)
        message = self.notice()
        self.assertEqual(message["operation"]["subject"], "second")
        self.assertEqual(self.sandbox("inbox.next", {"key": uuid.uuid4().hex})["message"], None)
        # Without notify nothing is sent.
        self.assertEqual(self.inside("create", "--name", "quiet", "--", "sh", "-c", "exit 0").returncode, 0)
        self.d.wait(lambda: self.listed() == [])
        time.sleep(1)
        self.assertEqual(self.sandbox("inbox.next", {"key": uuid.uuid4().hex})["message"], None)

    def test_a_goblin_stopping_with_a_notified_terminal_ends_cleanly(self):
        self.assertEqual(self.inside("create", "--name", "stays", "--notify", "--", "sleep", "600").returncode, 0)
        self.d.rpc.call("sessions.stop", {"session": self.agent["session"]})
        self.d.wait(lambda: self.d.get(self.agent["session"])["state"] == "stopped")
        self.assertEqual(self.d.get(self.agent["session"])["terminals"], [])


if __name__ == "__main__":
    unittest.main()
