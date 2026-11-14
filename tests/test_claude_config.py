"""Claude Code Nix setup and lifecycle hooks in real sandboxes."""
import json
import os
from pathlib import Path
import re
import shlex
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon
from support import step
from terminal_support import Terminal


class ClaudeConfigTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("claude-goblins")

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="gclaude-")
        self.addCleanup(temp.cleanup)
        self.home = Path(temp.name) / "home"
        self.claude = self.home / ".claude"
        self.claude.mkdir(parents=True)
        (self.claude / ".credentials.json").write_text('{"fixture":"credential"}')
        (self.claude / "settings.json").write_text('{"model":"host-model"}')
        (self.home / "secret").write_text("not mounted")
        self.d = Daemon(self.app, env={**os.environ, "HOME": str(self.home)})
        self.addCleanup(self.d.close)
        self.manifest = json.loads(Path(self.d.manifest).read_text())
        self.assertEqual(
            self.manifest["goblins"]["claude"]["description"],
            "Claude Code running in a Goblins sandbox",
        )
        source = self.manifest["goblins"]["claude"]["sandbox_etc"][
            "claude-code/managed-settings.json"
        ]
        self.settings = json.loads(Path(source).read_text())
        for skill in [
            "goblins-spawn", "goblins-messaging", "goblins-packages", "goblins-docker",
            "goblins-raids", "goblins-devshell", "goblins-worktrees", "goblins-terminals",
        ]:
            self.assertEqual(self.settings["skillOverrides"][skill], "on")

    def terminal(self, name="claude", agent="chief"):
        terminal = Terminal([
            str(self.app), "--state-dir", str(self.d.state), "run", name,
            "--name", agent,
        ])
        self.addCleanup(terminal.close)
        return terminal

    def integration(self, session):
        """The goblin's own wakeup ledger, as `goblins-claude status` reports it (none before launch)."""
        result = self.d.cli("exec", session, "--", "goblins-claude", "status", check=False)
        return json.loads(result.stdout) if result.returncode == 0 else {}

    def test_a_claude_goblin_shares_credentials_and_keeps_host_configuration_private(self):
        with step("the configured plugin and read-only settings run in an isolated home"):
            terminal = self.terminal()
            terminal.expect("CLAUDE_DIR=" + re.escape(str(self.claude)))
            terminal.expect(r"CLAUDE_ARG=--plugin-dir")
            plugin = terminal.expect(
                r"CLAUDE_ARG=(/nix/store/[^\n]+-goblins-claude-plugin)"
            ).group(1).decode()
            # Next, with no --dangerously-skip-permissions before it: without
            # bypassPermissions, Claude Code's own settings choose the mode.
            terminal.expect(r"\A\s*CLAUDE_ARG=literal \$\(false\) argument")
            terminal.expect("claude-probe>")
            terminal.send(
                "printf 'UID=%s\\n' \"$(id -u)\"; "
                f"test -r {shlex.quote(plugin)}/.claude-plugin/plugin.json "
                f"&& test -r {shlex.quote(plugin)}/monitors/monitors.json && "
                "test -r /etc/claude-code/managed-settings.json "
                "&& test -r /etc/claude-code/.claude/skills/goblins-spawn/SKILL.md "
                "&& test -r /etc/claude-code/.claude/skills/goblins-messaging/SKILL.md "
                "&& test -r /etc/claude-code/.claude/skills/goblins-packages/SKILL.md "
                "&& test -r /etc/claude-code/.claude/skills/goblins-docker/SKILL.md "
                "&& test -r /etc/claude-code/.claude/skills/goblins-raids/SKILL.md "
                "&& test ! -e /etc/claude-code/.claude/skills/goblins-devshell; "
                "printf 'SETUP=%s\\n' \"$?\"; "
                "echo changed >> /etc/claude-code/managed-settings.json; "
                "printf 'READONLY=%s\\n' \"$?\"; "
                "test -e \"$HOME/secret\"; printf 'SECRET=%s\\n' \"$?\"\n"
            )
            terminal.expect(r"(?:^|\n)UID=1000\n")
            terminal.expect(r"(?:^|\n)SETUP=0\n")
            terminal.expect(r"(?:^|\n)READONLY=1\n")
            terminal.expect(r"(?:^|\n)SECRET=1\n")
            terminal.send("goblin status\n")
            terminal.expect("Agent permission bypass: not requested")
        with step("owner and member both observe credential rotation"):
            owner = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
            member = self.d.member(owner, "claude", agent_name="scout")["session"]
            attached = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", member])
            self.addCleanup(attached.close)
            attached.expect("CLAUDE_DIR=" + re.escape(str(self.claude)))
            replacement = self.claude / "replacement"
            replacement.write_text('{"fixture":"rotated"}')
            replacement.replace(self.claude / ".credentials.json")
            for peer in [terminal, attached]:
                peer.send("printf 'AUTH=%s\\n' \"$(jq -r .fixture \"$CLAUDE_CONFIG_DIR/.credentials.json\")\"\n")
                peer.expect(r"(?:^|\n)AUTH=rotated\n")
            self.assertEqual((self.claude / "settings.json").read_text(), '{"model":"host-model"}')
        with step("a custom configuration directory and permission bypass"):
            custom = self.home / "custom claude"
            custom.mkdir()
            terminal = self.terminal("custom", "custom")
            terminal.expect("CLAUDE_DIR=" + re.escape(str(custom)), timeout=60)
            # This configuration sets bypassPermissions.
            terminal.expect(r"CLAUDE_ARG=--dangerously-skip-permissions")
            terminal.expect("claude-probe>")
            terminal.send("goblin status\n")
            terminal.expect("Agent permission bypass: requested")

    def test_a_claude_goblin_handles_its_inbox_and_resumes_its_conversation(self):
        with step("a detached agent wakes for its first message and goes idle"):
            launch = self.d.start("notifier", agent_name="receiver")
            receiver = launch["session"]
            self.d.wait(lambda: self.integration(receiver).get("state") == "starting")
            sent = self.d.rpc.call(
                "messages.send",
                {"key": "startup", "to": receiver, "body": "startup message"},
            )
            self.d.wait(lambda: self.d.rpc.call("inbox.status", {})["queued"] == 1)
            response = self.d.rpc.call("inbox.next", {"key": "startup-reply"})
            reply = response["message"]
            self.assertEqual(reply["body"], "processed: startup message")
            self.d.rpc.call("inbox.complete", {"key":"read-startup", "message":reply["id"],
                "claim_generation":response["claim_generation"]})
            # The monitor's line woke it, naming the message; the daemon took no part.
            wakes = self.d.cli("exec", receiver, "--", "cat", "/tmp/fake-claude-wakes").stdout
            self.assertEqual(
                wakes,
                f"Check your Goblins inbox and process the next item. [goblins-inbox-wake {sent['id']}]\n",
            )
            events = self.d.rpc.call("communications.list", {})["events"]
            self.assertFalse(any(e["kind"].startswith("integration.") for e in events))
            self.d.wait(lambda: self.integration(receiver)["state"] == "ready")
            # Heartbeat refresh and expiry are covered with an injected clock in
            # controller::tests::expired_activity_reaches_subscribers_and_heartbeats_do_not.
            # This workflow waits for the user-visible idle transition.
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            # Its hooks recorded the conversation; recreating resumes it.
            recorded = self.d.record(receiver)["variables"]
            self.assertEqual(recorded["session_name"], "Fake task")
        with step("attach, handle another message, then exit the conversation"):
            terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", receiver])
            self.addCleanup(terminal.close)
            terminal.expect("  ❯ ")
            self.d.rpc.call("messages.send", {"key":"attached","to":receiver,"body":"while attached"})
            self.d.wait(lambda: self.d.rpc.call("inbox.status", {})["queued"] == 1)
            answer = self.d.rpc.call("inbox.next", {"key":"attached-reply"})["message"]
            self.assertEqual(answer["body"], "processed: while attached")
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            terminal.send("exit\r")
            self.assertEqual(terminal.wait(), 0)
        with step("resume continues the recorded conversation"):
            self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
            self.d.wait_record_released(self.d.record(receiver)["id"])
            again = self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": self.d.manifest,
                                                       "recreate": receiver, "rows": 24, "cols": 100, "detached": True})
            self.assertEqual(again["agent_name"], "receiver")
            # The same record continues, with the resumed conversation.
            self.d.wait(lambda: self.d.record(receiver)["variables"].get("session_name") == "Resumed task", timeout=60)
            self.assertEqual(self.d.record(receiver)["variables"]["session_id"], recorded["session_id"])
            self.assertEqual(self.d.record(receiver)["sessions"], [receiver, again["session"]])
            self.assertFalse((self.d.history / f"{again['session']}.json").exists())


if __name__ == "__main__":
    unittest.main()
