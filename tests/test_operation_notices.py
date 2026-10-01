"""Goblins-owned operations resume the fake Codex and Claude agents that
requested them, through the ordinary inbox notification machinery."""
import os
from pathlib import Path
import tempfile
import time
import unittest

from daemon_support import Daemon, ROOT, RPC, receive
from support import command
from terminal_support import Terminal


def target(event):
    """The message an inbox mutation event acted on."""
    return event["data"].get("change", {}).get("message")


class Notices:
    check = None
    composer = None

    @classmethod
    def setUpClass(cls):
        cls.app = Path(command([
            "nix", "build", "--no-link", "--print-out-paths",
            "path:" + str(ROOT) + "#checks.x86_64-linux." + cls.check,
        ])) / "bin/goblins"

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="gnotice-")
        self.addCleanup(temp.cleanup)
        home = Path(temp.name) / "home"
        for directory in (".codex", ".claude"):
            (home / directory).mkdir(parents=True)
        self.d = Daemon(self.app, env={**os.environ, "HOME": str(home)})
        self.addCleanup(self.d.close)
        self.receiver = self.d.start("notifier", agent_name="receiver")["session"]
        self.d.wait(lambda: self.integration().get("state") in ("starting", "ready"))

    def integration(self):
        return self.d.rpc.call("integration.status", {"session": self.receiver})

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
            return (e["data"].get("request") == request or target(e) == notice["id"]
                    or e["kind"].startswith("integration.wake"))
        return [e["kind"] for e in events if ours(e)]

    def test_idle_agent_resumes_after_each_unsuccessful_outcome(self):
        # The agent yielded with an empty inbox while its request waited.
        for outcome in ("denied", "withdrawn"):
            peer, record = self.d.pending(self.receiver)
            self.assertEqual(self.sandbox_status()["queued"], 0)
            self.assertEqual(self.sandbox_status()["operations"][0]["request"], record["id"])
            if outcome == "denied":
                self.d.decide(record, False)
                # The waiting CLI still gets its own result.
                self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
            peer.close()
            notice, events = self.handled(record["id"])
            self.assertEqual(notice["operation"]["status"], outcome)
            self.assertEqual(notice["sender"], "goblins")
            kinds = self.kinds(events, record["id"], notice)
            expected = ["operation.requested"] + (["operation.decided"] if outcome == "denied" else []) + [
                "operation.completed", "integration.wake_attempt", "message.claimed", "message.completed"]
            self.assertEqual([k for k in kinds if k != "integration.wake_result"][-len(expected):], expected)
            self.assertEqual(self.sandbox_status()["operations"], [])
        self.assertEqual(self.sandbox_status()["queued"], 0)

    def test_working_agent_handles_the_outcome_before_its_turn_ends(self):
        peer = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", self.receiver])
        self.addCleanup(peer.close)
        peer.expect(self.composer)
        peer.send("long-busy\r")
        peer.expect("Working")
        self.d.wait(lambda: self.integration()["state"] == "working")
        request, record = self.d.pending(self.receiver)
        self.d.decide(record, False)
        request.close()
        notice, events = self.handled(record["id"])
        kinds = self.kinds(events, record["id"], notice)
        completed = kinds.index("operation.completed")
        # No wakeup was sent to the busy agent; it fetched the notice itself.
        self.assertNotIn("integration.wake_attempt", kinds[completed:])
        self.assertEqual(kinds[completed + 1:], ["message.claimed", "message.completed"])
        self.d.wait(lambda: self.integration()["state"] == "ready")
        self.assertEqual(self.sandbox_status()["queued"], 0)

    def test_paused_integration_holds_the_notice(self):
        self.d.rpc.call("integration.pause", {"session": self.receiver})
        peer, record = self.d.pending(self.receiver)
        self.d.decide(record, False)
        peer.close()
        self.d.wait(lambda: self.sandbox_status()["queued"] == 1)
        time.sleep(1.5)
        self.assertEqual(self.sandbox_status()["queued"], 1)
        self.assertFalse(any(e["kind"] == "integration.wake_attempt" for e in self.events()))
        self.d.rpc.call("integration.resume", {"session": self.receiver})
        self.handled(record["id"])

    def sandbox_status(self):
        peer = RPC(self.d.state / self.receiver / "resources/request.sock")
        try:
            return peer.call("inbox.status", {})
        finally:
            peer.close()


class CodexNotices(Notices, unittest.TestCase):
    check = "codex-goblins"
    composer = "› "


class ClaudeNotices(Notices, unittest.TestCase):
    check = "claude-goblins"
    composer = "❯ "


if __name__ == "__main__":
    unittest.main()
