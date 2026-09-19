"""Codex Nix setup and hooks in real sandboxes, with synthetic credentials only.

The native checks use `codex login status` and the local /hooks UI; no model
prompt or live game runs.
"""
import json
import os
from pathlib import Path
import re
import shlex
import tempfile
import tomllib
import unittest
import uuid

from daemon_support import Daemon, ROOT, RPC
from support import command
from terminal_support import Terminal, screen_wait


class CodexConfigTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                                "path:" + str(ROOT) + "#checks.x86_64-linux.codex-goblins"])) / "bin/goblins"

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="gcodex-")
        self.addCleanup(temp.cleanup)
        self.home = Path(temp.name) / "home"
        self.codex = self.home / ".codex"
        self.codex.mkdir(parents=True)
        # A fake local credential exercises sharing without any account access.
        (self.codex / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "goblins-test-not-a-real-key"}))
        self.original = b'model = "host-fixture-model"\n'
        (self.codex / "config.toml").write_bytes(self.original)
        (self.home / "secret").write_text("not mounted")
        self.d = Daemon(self.app, env={**os.environ, "HOME": str(self.home)})
        self.addCleanup(self.d.close)
        self.manifest = json.loads(Path(self.d.manifest).read_text())
        self.settings = tomllib.loads(Path(self.manifest["goblins"]["codex"]["sandbox_etc"]["codex/config.toml"]).read_text())

    def terminal(self, name="codex", agent="chief"):
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "run", name, "--name", agent])
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
        terminal.send(f"printf '%s' {shlex.quote(payload)} | {shlex.quote(handler)} > /tmp/hook-output; printf 'HOOK_RESULT=%s\\n' \"$(cat /tmp/hook-output)\"\n")
        return json.loads(terminal.expect(r"(?:^|\n)HOOK_RESULT=(.*)\n").group(1))

    def test_mounts_hooks_and_child_inheritance(self):
        terminal = self.terminal()
        terminal.expect("CODEX_DIR=" + re.escape(str(self.codex)))
        terminal.expect(r"CODEX_ARG=literal \$\(false\) argument")
        terminal.expect("codex-probe>")
        terminal.send("test -r /etc/codex/skills/goblins/SKILL.md && test -r /etc/codex/config.toml; printf 'SETUP=%s\\n' \"$?\"; echo changed >> /etc/codex/config.toml; printf 'READONLY=%s\\n' \"$?\"; test -e \"$HOME/secret\"; printf 'SECRET=%s\\n' \"$?\"; cat /etc/goblins-test.conf\n")
        terminal.expect(r"(?:^|\n)SETUP=0\n")
        terminal.expect(r"(?:^|\n)READONLY=1\n")
        terminal.expect(r"(?:^|\n)SECRET=1\n")
        terminal.expect(r"(?:^|\n)sandbox-only\n")
        context = self.hook(terminal, "SessionStart")
        self.assertEqual(context["hookSpecificOutput"]["hookEventName"], "SessionStart")
        self.assertEqual(self.hook(terminal, "Stop"), {})
        parent = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        self.d.rpc.call("messages.send", {"key": "task", "to": parent, "body": "private message body"})
        continuation = self.hook(terminal, "Stop")
        self.assertEqual(continuation["decision"], "block")
        self.assertNotIn("private message body", json.dumps(continuation))
        self.assertEqual(self.sandbox(parent, "inbox.status", {})["queued"], 1)
        self.assertIsNone(self.sandbox(parent, "inbox.status", {})["claim"])
        self.assertEqual(self.hook(terminal, "Stop", stop_hook_active=True), {})
        child = self.sandbox(parent, "sessions.start", {
            "key": uuid.uuid4().hex, "name": "codex", "agent_name": "scout", "detached": True,
        })["session"]
        self.d.wait(lambda: self.d.get(child)["state"] == "running")
        attached = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", child])
        self.addCleanup(attached.close)
        attached.expect("CODEX_DIR=" + re.escape(str(self.codex)))
        attached.expect("codex-probe>")
        # Rotation by replacing a file must be observed by parent and child.
        replacement = self.codex / "replacement"
        replacement.write_text('{"marker":"rotated"}')
        replacement.replace(self.codex / "auth.json")
        for peer in [terminal, attached]:
            peer.send("printf 'AUTH_MARKER=%s\\n' \"$(jq -r .marker \"$CODEX_HOME/auth.json\")\"; printf 'ENV_MARKER=%s\\n' \"$GOBLINS_TEST_MARKER\"\n")
            peer.expect(r"(?:^|\n)AUTH_MARKER=rotated\n")
            peer.expect(r"(?:^|\n)ENV_MARKER=inherited\n")
        self.assertEqual((self.codex / "config.toml").read_bytes(), self.original)
        self.assertFalse((self.codex / "hooks.json").exists())

    def test_custom_directory_and_native_auth_status(self):
        custom = self.home / "custom codex"
        custom.mkdir()
        terminal = self.terminal("custom")
        terminal.expect("CODEX_DIR=" + re.escape(str(custom)))
        terminal.expect("codex-probe>")
        native = self.terminal("native", "native")
        native.expect("Logged in using an API key")
        self.assertEqual(native.wait(), 0)
        self.assertEqual((self.codex / "config.toml").read_bytes(), self.original)

    def test_invalid_etc_mounts_fail_before_payload_launch(self):
        source = self.manifest["goblins"]["codex"]["sandbox_etc"]["codex/config.toml"]
        for mounts in [
            {"../escape": source},
            {"codex/config.toml": str(self.codex / "config.toml")},
            {"codex": source, "codex/config.toml": source},
        ]:
            manifest = json.loads(json.dumps(self.manifest))
            manifest["goblins"]["codex"]["sandbox_etc"] = mounts
            path = self.home / "invalid.json"
            path.write_text(json.dumps(manifest))
            launch = self.d.start("codex", manifest=path, wait=False)
            record = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] == "failed" else None))
            self.assertTrue(record["detail"])

    def test_native_ui_discovers_managed_hooks(self):
        # Trust only this test workspace in the synthetic native configuration.
        # /hooks is local UI, not a prompt sent to a model.
        (self.codex / "config.toml").write_text(
            'model = "gpt-5.3-codex"\n'
            f'[projects.{json.dumps(str(ROOT))}]\ntrust_level = "trusted"\n'
        )
        native = self.terminal("native-ui")
        screen_wait(native, lambda text: "OpenAI Codex" in text, timeout=60)
        native.send("/hooks\r")
        screen = screen_wait(native, lambda text: "SessionStart" in text and "Stop" in text, timeout=30)
        self.assertRegex(screen, r"SessionStart\s+1\s+1")
        self.assertRegex(screen, r"\bStop\s+1\s+1")
        native.send("\x1b[B" * 5 + "\r")
        screen = screen_wait(native, lambda text: "Managed" in text and "Admin config" in text, timeout=10)
        self.assertIn("goblins-codex-hook", screen)


if __name__ == "__main__":
    unittest.main()
