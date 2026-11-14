"""An agent learns the outcome of a request it made: Goblins-owned operations
resume the fake Claude and Codex agents through the ordinary inbox
notification machinery."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from build_support import fixture_app
from daemon_support import Daemon, RPC, receive
from support import step
from terminal_support import Terminal


PROMPT = "Check your Goblins inbox and process the next item."


def target(event):
    """The message an inbox mutation event acted on."""
    return event["data"].get("change", {}).get("message")


class Notices:
    check = None
    configuration = None

    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app(cls.check)

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="gnotice-")
        self.addCleanup(temp.cleanup)
        home = Path(temp.name) / "home"
        for directory in (".codex", ".claude", "shared"):
            (home / directory).mkdir(parents=True)
        self.shared = home / "shared"
        self.d = Daemon(self.app, env={**os.environ, "HOME": str(home)})
        self.addCleanup(self.d.close)
        self.receiver = self.d.start(self.configuration, agent_name="receiver")["session"]
        self.d.wait(lambda: self.integration().get("state") in self.started)

    def goblin(self, *argv, check=True):
        return subprocess.run([str(self.app), "--state-dir", str(self.d.state), "exec", self.receiver, "--", *argv],
                              check=check, capture_output=True, text=True)

    def events(self):
        events, after = [], 0
        while True:
            page = self.d.rpc.call("communications.list", {"after": after, "session": self.receiver})
            events += page["events"]
            after = page["cursor"]
            if after >= page["latest"]:
                return events

    def handled(self, request):
        """The notice for REQUEST, once the agent has completed it."""
        def find():
            events = self.events()
            notice = next((e["data"]["notice"] for e in events
                           if e["kind"] == "operation.completed" and e["data"]["request"] == request), None)
            done = notice and any(e["kind"] == "message.completed" and target(e) == notice["id"]
                                  for e in events)
            return (notice, events) if done else None
        return self.d.wait(find, timeout=60)

    def kinds(self, events, request, notice):
        def ours(e):
            return e["data"].get("request") == request or target(e) == notice["id"]
        return [e["kind"] for e in events if ours(e)]

    def inbox(self):
        peer = RPC(self.d.state / self.receiver / "resources/request.sock")
        try:
            return peer.call("inbox.status", {})
        finally:
            peer.close()

    def an_idle_agent_is_woken(self, outcome):
        """The agent yielded with an empty inbox while its request waited."""
        peer, record = self.d.pending(self.receiver)
        self.assertEqual(self.inbox()["queued"], 0)
        self.assertEqual(self.inbox()["operations"][0]["request"], record["id"])
        if outcome == "denied":
            self.d.decide(record, False)
            # The waiting CLI still gets its own result.
            self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        peer.close()
        notice, events = self.handled(record["id"])
        self.assertEqual((notice["operation"]["status"], notice["sender"]), (outcome, "goblins"))
        expected = ["operation.requested"] + (["operation.decided"] if outcome == "denied" else []) + [
            "operation.completed", "message.claimed", "message.completed"]
        kinds = self.kinds(events, record["id"], notice)
        self.assertEqual(kinds[-len(expected):], expected)
        self.assertEqual(self.inbox()["operations"], [])
        self.assertEqual(self.inbox()["queued"], 0)
        return notice


class ClaudeNoticesWorkflow(Notices, unittest.TestCase):
    check = "claude-goblins"
    configuration = "host-notifier"
    started = ("starting", "ready")

    def integration(self):
        result = self.goblin("goblins-claude", "status", check=False)
        return json.loads(result.stdout) if result.returncode == 0 else {}

    def wakes(self):
        """The lines Claude's monitor printed (none yet: no file)."""
        return self.goblin("cat", "/tmp/fake-claude-wakes", check=False).stdout.splitlines()

    def test_a_claude_agent_learns_the_outcome_of_its_request(self):
        for outcome in ("denied", "withdrawn"):
            with step(f"an idle agent is woken when its request is {outcome}"):
                notice = self.an_idle_agent_is_woken(outcome)
                self.assertIn(f"{PROMPT} [goblins-inbox-wake {notice['id']}]", self.wakes())

        with step("a working agent handles the outcome before its turn ends, without a wakeup"):
            wakes = self.wakes()
            peer = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", self.receiver])
            self.addCleanup(peer.close)
            peer.expect("❯ ")
            peer.send("hold\r")
            peer.expect("Working")
            self.d.wait(lambda: self.integration().get("state") == "working")
            request, record = self.d.pending(self.receiver)
            self.d.decide(record, False)
            request.close()
            # The notice is queued while the turn still runs; then the turn ends.
            self.d.wait(lambda: self.inbox()["queued"] == 1)
            self.goblin("touch", "/tmp/fake-claude-release")
            notice, events = self.handled(record["id"])
            kinds = self.kinds(events, record["id"], notice)
            # The agent fetched the notice itself.
            self.assertEqual(kinds[kinds.index("operation.completed") + 1:], ["message.claimed", "message.completed"])
            self.assertEqual(self.wakes(), wakes)
            self.d.wait(lambda: self.integration().get("state") == "ready")
            self.assertEqual(self.inbox()["queued"], 0)

        with step("an attached host command's result reaches the agent, a detached one's becomes a notice"):
            def request(**host):
                return {"kind": "host-command", "reason": "notice test",
                        "host": {"argv": ["true"], "cwd": str(self.shared), **host}}
            attached = RPC(self.d.state / self.receiver / "resources/request.sock")
            attached.send("permissions.request", request())
            waited = receive(attached.peer)["params"]["request"]
            self.d.decide(self.d.wait(lambda: next(iter(self.d.permissions(state="pending")), None)), True)
            self.assertEqual(receive(attached.peer)["result"]["status"], "finished")
            attached.close()
            detached = RPC(self.d.state / self.receiver / "resources/request.sock")
            other = detached.call("permissions.request", request(detach=True))["request"]
            detached.close()
            self.d.decide(self.d.wait(lambda: next(iter(self.d.permissions(state="pending")), None)), True)
            notice, events = self.handled(other)
            self.assertEqual(notice["operation"]["status"], "finished")
            completed = next(e for e in events if e["kind"] == "operation.completed" and e["data"]["request"] == waited)
            self.assertIsNone(completed["data"].get("notice"))


class CodexNoticesWorkflow(Notices, unittest.TestCase):
    check = "codex-goblins"
    configuration = "notifier"
    started = ("ready",)

    def integration(self):
        state = self.d.get(self.receiver)["agent_state"]
        return {"state": {"idle": "ready", "unknown": "starting"}.get(state, state)}

    def test_an_idle_codex_agent_learns_a_denial(self):
        self.an_idle_agent_is_woken("denied")


if __name__ == "__main__":
    unittest.main()
