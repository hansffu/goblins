"""Claude Code Nix setup and lifecycle hooks in real sandboxes."""
import json
import os
from pathlib import Path
import re
import shlex
import tempfile
import unittest
import uuid

from daemon_support import Daemon, ROOT, RPC
from support import command
from terminal_support import Terminal


class ClaudeConfigTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = Path(command([
            "nix", "build", "--no-link", "--print-out-paths",
            "path:" + str(ROOT) + "#checks.x86_64-linux.claude-goblins",
        ])) / "bin/goblins"

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
        for skill in ["goblins-spawn", "goblins-messaging", "goblins-packages"]:
            self.assertEqual(self.settings["skillOverrides"][skill], "on")

    def terminal(self, name="claude", agent="chief"):
        terminal = Terminal([
            str(self.app), "--state-dir", str(self.d.state), "run", name,
            "--name", agent,
        ])
        self.addCleanup(terminal.close)
        return terminal

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def hook(self, terminal, event, **extra):
        handler = self.settings["hooks"][event][0]["hooks"][0]["command"]
        payload = json.dumps({"hook_event_name": event, **extra})
        terminal.send(
            f"printf '%s' {shlex.quote(payload)} | {shlex.quote(handler)} "
            '> /tmp/hook-output; printf \'HOOK_RESULT=%s\\n\' "$(cat /tmp/hook-output)"\n'
        )
        return json.loads(terminal.expect(r"(?:^|\n)HOOK_RESULT=(.*)\n").group(1))

    def test_mounts_hooks_skills_and_inherits_state(self):
        terminal = self.terminal()
        terminal.expect("CLAUDE_DIR=" + re.escape(str(self.claude)))
        terminal.expect(r"CLAUDE_ARG=--dangerously-skip-permissions")
        terminal.expect(r"CLAUDE_ARG=literal \$\(false\) argument")
        terminal.expect("claude-probe>")
        terminal.send(
            "printf 'UID=%s\\n' \"$(id -u)\"; "
            "test -r /etc/claude-code/managed-settings.json "
            "&& test -r /etc/claude-code/.claude/skills/goblins-spawn/SKILL.md "
            "&& test -r /etc/claude-code/.claude/skills/goblins-messaging/SKILL.md "
            "&& test -r /etc/claude-code/.claude/skills/goblins-packages/SKILL.md; "
            "printf 'SETUP=%s\\n' \"$?\"; "
            "echo changed >> /etc/claude-code/managed-settings.json; "
            "printf 'READONLY=%s\\n' \"$?\"; "
            "test -e \"$HOME/secret\"; printf 'SECRET=%s\\n' \"$?\"\n"
        )
        terminal.expect(r"(?:^|\n)UID=1000\n")
        terminal.expect(r"(?:^|\n)SETUP=0\n")
        terminal.expect(r"(?:^|\n)READONLY=1\n")
        terminal.expect(r"(?:^|\n)SECRET=1\n")
        parent = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        integration = self.d.rpc.call("integration.status", {"session": parent})
        self.assertEqual(integration["driver"], "claude")
        self.assertEqual(integration["state"], "starting")
        self.assertEqual(self.hook(terminal, "SessionStart"), {})
        self.assertEqual(
            self.hook(terminal, "UserPromptSubmit", prompt="private task"),
            {},
        )
        self.assertEqual(self.hook(terminal, "Stop"), {})
        self.d.rpc.call("messages.send", {"key": "task", "to": parent, "body": "private body"})
        continuation = self.hook(terminal, "Stop")
        self.assertEqual(continuation["decision"], "block")
        self.assertNotIn("private body", json.dumps(continuation))
        self.assertEqual(self.hook(terminal, "Stop", stop_hook_active=True), {})
        child = self.sandbox(parent, "sessions.start", {
            "key": uuid.uuid4().hex,
            "name": "claude",
            "agent_name": "scout",
            "detached": True,
        })["session"]
        self.d.wait(lambda: self.d.get(child)["state"] == "running")
        attached = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", child])
        self.addCleanup(attached.close)
        attached.expect("CLAUDE_DIR=" + re.escape(str(self.claude)))
        replacement = self.claude / "replacement"
        replacement.write_text('{"fixture":"rotated"}')
        replacement.replace(self.claude / ".credentials.json")
        for peer in [terminal, attached]:
            peer.send("printf 'AUTH=%s\\n' \"$(jq -r .fixture \"$CLAUDE_CONFIG_DIR/.credentials.json\")\"\n")
            peer.expect(r"(?:^|\n)AUTH=rotated\n")
        self.assertEqual((self.claude / "settings.json").read_text(), '{"model":"host-model"}')

    def test_custom_directory(self):
        custom = self.home / "custom claude"
        custom.mkdir()
        terminal = self.terminal("custom")
        terminal.expect("CLAUDE_DIR=" + re.escape(str(custom)), timeout=60)
        terminal.expect("claude-probe>")

    def test_spawn_message_wakes_indented_claude_composer(self):
        launch = self.d.start("notifier", agent_name="receiver")
        receiver = launch["session"]
        self.d.wait(
            lambda: self.d.rpc.call(
                "integration.status", {"session": receiver}
            ).get("state")
            == "starting"
        )
        self.d.rpc.call(
            "messages.send",
            {"key": "startup", "to": receiver, "body": "startup message"},
        )
        self.d.wait(lambda: self.d.rpc.call("inbox.status", {})["queued"] == 1)
        reply = self.d.rpc.call("inbox.next", {"key": "startup-reply"})["message"]
        self.assertEqual(reply["body"], "processed: startup message")
        events = self.d.rpc.call("communications.list", {})["events"]
        self.assertTrue(
            any(
                event["kind"] == "integration.wake_attempt"
                and event["actor"] == receiver
                for event in events
            )
        )


if __name__ == "__main__":
    unittest.main()
