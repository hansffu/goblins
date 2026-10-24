"""Claude Code Nix setup and lifecycle hooks in real sandboxes."""
import json
import os
from pathlib import Path
import re
import shlex
import tempfile
import time
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon
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
            "goblins-raids", "goblins-devshell",
        ]:
            self.assertEqual(self.settings["skillOverrides"][skill], "on")

    def terminal(self, name="claude", agent="chief"):
        terminal = Terminal([
            str(self.app), "--state-dir", str(self.d.state), "run", name,
            "--name", agent,
        ])
        self.addCleanup(terminal.close)
        return terminal

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
        owner = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        integration = self.d.rpc.call("integration.status", {"session": owner})
        self.assertEqual(integration["driver"], "claude")
        self.assertEqual(integration["state"], "starting")
        activity = lambda: self.d.get(owner)["agent_state"]
        self.assertEqual(self.hook(terminal, "SessionStart"), {})
        self.assertEqual(activity(), "idle")
        self.assertEqual(
            self.hook(terminal, "UserPromptSubmit", prompt="private task"),
            {},
        )
        self.assertEqual(activity(), "working")
        # Activity-only hooks never answer the model or decide for a tool.
        self.assertEqual(
            self.settings["hooks"]["PreToolUse"][0]["matcher"],
            "AskUserQuestion|ExitPlanMode",
        )
        self.assertEqual(
            self.hook(terminal, "PreToolUse", tool_name="AskUserQuestion", tool_input={}),
            {},
        )
        self.assertEqual(activity(), "waiting-input")
        self.assertEqual(self.hook(terminal, "PostToolUse", tool_name="AskUserQuestion"), {})
        self.assertEqual(activity(), "working")
        self.assertEqual(
            self.hook(terminal, "Notification", notification_type="idle_prompt", message="idle"),
            {},
        )
        self.assertEqual(activity(), "idle")
        self.assertEqual(self.hook(terminal, "UserPromptSubmit", prompt="again"), {})
        self.assertEqual(self.hook(terminal, "Stop"), {})
        self.assertEqual(activity(), "idle")
        self.assertEqual(
            self.d.rpc.call("integration.status", {"session": owner})["activity"], "idle"
        )
        self.d.rpc.call("messages.send", {"key": "task", "to": owner, "body": "private body"})
        continuation = self.hook(terminal, "Stop")
        self.assertEqual(continuation["decision"], "block")
        self.assertNotIn("private body", json.dumps(continuation))
        self.assertEqual(self.hook(terminal, "Stop", stop_hook_active=True), {})
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

    def test_stop_leaves_pending_messages_to_a_running_monitor(self):
        terminal = self.terminal()
        plugin = terminal.expect(
            r"CLAUDE_ARG=(/nix/store/[^\n]+-goblins-claude-plugin)", timeout=60
        ).group(1).decode()
        terminal.expect("claude-probe>")
        owner = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        state = lambda: self.d.rpc.call("integration.status", {"session": owner})["state"]
        self.assertEqual(self.hook(terminal, "SessionStart"), {})
        self.assertEqual(self.hook(terminal, "UserPromptSubmit", prompt="task"), {})
        body = "private body: héllo ✉\nsecond line"
        sent = self.d.rpc.call("messages.send", {"key": "pending", "to": owner, "body": body})
        # The mod's read-only helper reads it whole, without claiming it.
        helper = shlex.quote(plugin + "/bin/goblins-claude-message")
        terminal.send(
            f"{helper} {sent['id']} | jq -e --arg body {shlex.quote(body)} "
            "'.body == $body and .state == \"queued\"' >/dev/null; printf 'HELPER=%s\\n' \"$?\"; "
            f"{helper} unknown-id 2>/dev/null; printf 'UNKNOWN=%s\\n' \"$?\"\n"
        )
        terminal.expect(r"(?:^|\n)HELPER=0\n")
        terminal.expect(r"(?:^|\n)UNKNOWN=1\n")
        # A script under the monitor's name stands in for the plugin monitor
        # (not exec -a: a multi-call coreutils reads its applet from argv[0]).
        terminal.send(
            "printf '#!%s\\nsleep 300 &\\nwait\\n' \"$(command -v bash)\" > /tmp/goblins-claude-monitor && "
            "chmod +x /tmp/goblins-claude-monitor && { /tmp/goblins-claude-monitor & "
            "printf '%s\\n' \"$!\" > /tmp/goblins-claude/monitor.pid; }; printf 'MONITOR=%s\\n' \"$?\"\n"
        )
        terminal.expect(r"(?:^|\n)MONITOR=0\n")
        # The turn ends without a Stop-hook continuation; the daemon settles
        # ready, so its notifier (the monitor) delivers the message.
        self.assertEqual(self.hook(terminal, "Stop"), {})
        self.assertEqual(state(), "ready")
        self.assertEqual(self.d.get(owner)["agent_state"], "idle")
        # A stale PID, or another process under it, is no monitor.
        terminal.send("kill \"$(cat /tmp/goblins-claude/monitor.pid)\"; printf 'KILLED=%s\\n' \"$?\"\n")
        terminal.expect(r"(?:^|\n)KILLED=0\n")
        self.assertEqual(self.hook(terminal, "UserPromptSubmit", prompt="again"), {})
        continuation = self.hook(terminal, "Stop")
        self.assertEqual(continuation["decision"], "block")
        self.assertNotIn("private body", json.dumps(continuation))
        self.assertEqual(state(), "working")
        terminal.send("printf '%s\\n' $$ > /tmp/goblins-claude/monitor.pid; printf 'SELF=%s\\n' \"$?\"\n")
        terminal.expect(r"(?:^|\n)SELF=0\n")
        self.assertEqual(self.hook(terminal, "UserPromptSubmit", prompt="third"), {})
        self.assertEqual(self.hook(terminal, "Stop")["decision"], "block")

    def test_hooks_record_the_conversation_and_recreate_resumes_it(self):
        self.assertEqual(self.manifest["goblins"]["claude"]["resume_command"],
                         ["literal $(false) argument", "--resume", "{session_id}"])
        self.assertEqual(self.manifest["goblins"]["custom"]["resume_command"], ["--resume", "{session_id}"])
        terminal = self.terminal()
        terminal.expect("claude-probe>", timeout=60)
        session = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        variables = lambda: self.d.record(session)["variables"]
        def transcript(path, *entries):
            for entry in entries:
                terminal.send(f"printf '%s\\n' {shlex.quote(json.dumps(entry))} >> {path}\n")
            terminal.send(f"printf 'LINES=%s\\n' \"$(wc -l < {path})\"\n")
            terminal.expect(r"(?:^|\n)LINES=\d+\n")
        first, second = str(uuid.uuid4()), str(uuid.uuid4())
        transcript("/tmp/a.jsonl", {"type": "ai-title", "aiTitle": "AI one", "sessionId": first})
        self.assertEqual(self.hook(terminal, "SessionStart", session_id=first, transcript_path="/tmp/a.jsonl"), {})
        self.d.wait(lambda: variables() == {"session_id": first, "session_name": "AI one"})
        # A custom title wins over a later generated one.
        transcript("/tmp/a.jsonl", {"type": "custom-title", "customTitle": "Mine", "sessionId": first},
                   {"type": "ai-title", "aiTitle": "AI two", "sessionId": first})
        self.assertEqual(self.hook(terminal, "UserPromptSubmit", prompt="x", session_id=first,
                                   transcript_path="/tmp/a.jsonl"), {})
        self.d.wait(lambda: variables()["session_name"] == "Mine")
        # session_title, when Claude sends it, wins over the transcript.
        self.assertEqual(self.hook(terminal, "Stop", session_id=first, session_title="Renamed\tline",
                                   transcript_path="/tmp/a.jsonl"), {})
        self.d.wait(lambda: variables()["session_name"] == "Renamed line")
        # An id switch resets the name; one that cannot be read yet is empty.
        self.hook(terminal, "SessionStart", session_id=second, transcript_path="/tmp/missing.jsonl")
        self.d.wait(lambda: variables() == {"session_id": second})
        transcript("/tmp/b.jsonl", {"type": "ai-title", "aiTitle": "B title", "sessionId": second})
        self.hook(terminal, "UserPromptSubmit", prompt="x", session_id=second, transcript_path="/tmp/b.jsonl")
        self.d.wait(lambda: variables() == {"session_id": second, "session_name": "B title"})
        # The same conversation keeps its last known name on a read failure.
        self.hook(terminal, "Stop", session_id=second, transcript_path="/tmp/missing.jsonl")
        # Recording never delays the inbox decision, even when the transcript
        # cannot be read in time (a FIFO without a writer blocks its reader).
        self.d.rpc.call("messages.send", {"key": "wake", "to": session, "body": "private body"})
        terminal.send("mkfifo /tmp/slow.jsonl; printf 'FIFO=%s\\n' \"$?\"\n")
        terminal.expect(r"(?:^|\n)FIFO=0\n")
        started = time.monotonic()
        decision = self.hook(terminal, "Stop", session_id=second, transcript_path="/tmp/slow.jsonl")
        self.assertEqual(decision["decision"], "block")
        self.assertLess(time.monotonic() - started, 2.5)
        # A failed call changes nothing, not even the hook's own answer.
        decision = self.hook(terminal, "Stop", session_id="bad\tid", transcript_path="/tmp/b.jsonl")
        self.assertEqual(decision["decision"], "block")
        time.sleep(7)
        self.assertEqual(variables(), {"session_id": second, "session_name": "B title"})
        # The next hook is recorded once the slow read gave up.
        third = str(uuid.uuid4())
        self.hook(terminal, "UserPromptSubmit", prompt="x", session_id=third, transcript_path="/tmp/b.jsonl")
        self.d.wait(lambda: variables() == {"session_id": third, "session_name": "B title"})
        second = third
        # Recreate resumes the recorded conversation with its own arguments.
        terminal.send("exit\n")
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")
        launch = self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": self.d.manifest,
                                                    "recreate": session, "rows": 24, "cols": 100})
        resumed = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", launch["session"]])
        self.addCleanup(resumed.close)
        resumed.expect(r"CLAUDE_ARG=literal \$\(false\) argument", timeout=60)
        resumed.expect(r"CLAUDE_ARG=--resume\r?\n")
        resumed.expect("CLAUDE_ARG=" + second)
        resumed.expect("claude-probe>")

    def test_a_goblin_with_nix_gets_the_nix_skill_instead_of_packages(self):
        terminal = self.terminal("nix", "nixer")
        terminal.expect("claude-probe>")
        skills = "/etc/claude-code/.claude/skills"
        terminal.send(
            f"test -r {skills}/goblins-nix/SKILL.md && test -r {skills}/goblins-messaging/SKILL.md "
            f"&& test ! -e {skills}/goblins-packages && test ! -e {skills}/goblins-devshell "
            "&& command -v nix >/dev/null; printf 'NIX_SKILLS=%s\\n' \"$?\"\n"
        )
        terminal.expect(r"(?:^|\n)NIX_SKILLS=0\n")

    def test_custom_directory_and_permission_bypass(self):
        custom = self.home / "custom claude"
        custom.mkdir()
        terminal = self.terminal("custom")
        terminal.expect("CLAUDE_DIR=" + re.escape(str(custom)), timeout=60)
        # This configuration sets bypassPermissions.
        terminal.expect(r"CLAUDE_ARG=--dangerously-skip-permissions")
        terminal.expect("claude-probe>")
        terminal.send("goblin status\n")
        terminal.expect("Agent permission bypass: requested")

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
        # The fake's Stop settles idle; monitor ticks keep it fresh past expiry.
        self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
        time.sleep(32)
        self.assertEqual(self.d.get(receiver)["agent_state"], "idle")
        # Its hooks recorded the conversation; recreating resumes it.
        recorded = self.d.record(receiver)["variables"]
        self.assertEqual(recorded["session_name"], "Fake task")
        self.d.rpc.call("sessions.stop", {"session": receiver})
        self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
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
