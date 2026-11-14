"""Codex Nix setup in real sandboxes, with synthetic credentials only.

A Codex goblin runs the App Server and its adapter in the background, and each
attach starts the terminal as a client. The native checks use `codex login
status`, `codex mcp list` and the terminal's own screen; no model prompt or
live game runs.
"""
import json
import os
from pathlib import Path
import re
import tempfile
import tomllib
import time
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, ROOT, RPC
from support import step
from terminal_support import Terminal, screen_wait

PROVIDER = r"""
# A scripted Responses API on loopback: the first request runs one command,
# every later one answers with a message.
import json, sys, uuid
from http.server import BaseHTTPRequestHandler, HTTPServer
calls = []
class Provider(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        calls.append(1)
        id = str(uuid.uuid4())
        if len(calls) == 1:
            item = {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'echo ran > ' + sys.argv[2]})}
        else:
            item = {'type': 'message', 'role': 'assistant', 'id': id, 'content': [{'type': 'output_text', 'text': 'done'}]}
        events = [{'type': 'response.created', 'response': {'id': id}},
                  {'type': 'response.output_item.done', 'item': item},
                  {'type': 'response.completed', 'response': {'id': id, 'usage': {'input_tokens': 0, 'output_tokens': 0, 'total_tokens': 0}}}]
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        for event in events:
            self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
HTTPServer(('127.0.0.1', int(sys.argv[1])), Provider).serve_forever()
"""

# The native configuration uses real Codex; setup and inbox use fakes.
real_codex = unittest.skipUnless(os.environ.get("GOBLINS_TEST_CODEX") == "1",
                                "opt in with just test-feature real-agents")

class CodexConfigTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("codex-goblins")

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
        self.assertEqual(
            self.manifest["goblins"]["codex"]["description"],
            "Codex running in a Goblins sandbox",
        )
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

    def exec(self, session, *argv):
        import subprocess
        return subprocess.run([str(self.app), "--state-dir", str(self.d.state), "exec", session, "--", *argv],
                              capture_output=True, text=True, timeout=60)

    def test_a_codex_goblin_shares_credentials_and_keeps_host_configuration_private(self):
        terminal = self.terminal()
        # The terminal is a client of the goblin's server, on its conversation.
        terminal.expect("CODEX_DIR=" + re.escape(str(self.codex)))
        terminal.expect(r"CODEX_ARG=--remote\n")
        terminal.expect(r"CODEX_ARG=unix:///tmp/goblins-codex/app-server.sock\n")
        thread = terminal.expect(r"CODEX_ARG=resume\nCODEX_ARG=(\S+)\n").group(1).decode()
        terminal.expect("codex-probe>")
        terminal.send("test -r /etc/codex/skills/goblins-spawn/SKILL.md && test -r /etc/codex/skills/goblins-messaging/SKILL.md && test -r /etc/codex/skills/goblins-packages/SKILL.md && test -r /etc/codex/skills/goblins-docker/SKILL.md && test -r /etc/codex/skills/goblins-raids/SKILL.md && test ! -e /etc/codex/skills/goblins-devshell && test -r /etc/codex/config.toml; printf 'SETUP=%s\\n' \"$?\"; echo changed >> /etc/codex/config.toml; printf 'READONLY=%s\\n' \"$?\"; test -e \"$HOME/secret\"; printf 'SECRET=%s\\n' \"$?\"; cat /etc/goblins-test.conf\n")
        terminal.expect(r"(?:^|\n)SETUP=0\n")
        terminal.expect(r"(?:^|\n)READONLY=1\n")
        terminal.expect(r"(?:^|\n)SECRET=1\n")
        terminal.expect(r"(?:^|\n)sandbox-only\n")
        owner = self.d.rpc.call("sessions.get", {"session": "chief"})["id"]
        self.assertEqual(self.d.record(owner)["variables"], {"session_id": thread, "session_name": "chief"})
        self.d.wait(lambda: self.d.get(owner)["agent_state"] == "idle")
        self.assertEqual(self.d.wait(lambda: self.thread(owner)), thread)
        # Goblins installs no hooks of its own.
        self.assertNotIn("hooks", self.settings)
        self.assertEqual(self.manifest["goblins"]["codex"]["args"], ["-c", 'model="literal $(false) {model}"'])
        self.d.rpc.call("messages.send", {"key":"task", "to":owner, "body":"private message body"})
        self.assertEqual(self.sandbox(owner, "inbox.status", {})["queued"],1)
        self.assertIsNone(self.sandbox(owner, "inbox.status", {})["claim"])
        member = self.d.member(owner, "codex", agent_name="scout")["session"]
        attached = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", member])
        self.addCleanup(attached.close)
        attached.expect("CODEX_DIR=" + re.escape(str(self.codex)))
        attached.expect("codex-probe>")
        # Rotation by replacing a file must be observed by owner and member.
        replacement = self.codex / "replacement"
        replacement.write_text('{"marker":"rotated"}')
        replacement.replace(self.codex / "auth.json")
        for peer in [terminal, attached]:
            peer.send("printf 'AUTH_MARKER=%s\\n' \"$(jq -r .marker \"$CODEX_HOME/auth.json\")\"; printf 'ENV_MARKER=%s\\n' \"$GOBLINS_TEST_MARKER\"\n")
            peer.expect(r"(?:^|\n)AUTH_MARKER=rotated\n")
            peer.expect(r"(?:^|\n)ENV_MARKER=inherited\n")
        self.assertEqual((self.codex / "config.toml").read_bytes(), self.original)
        self.assertFalse((self.codex / "hooks.json").exists())
        with step("a configured credential directory is used literally"):
            custom = self.home / "custom codex"
            custom.mkdir()
            custom_terminal = self.terminal("custom", "custom")
            custom_terminal.expect("CODEX_DIR=" + re.escape(str(custom)))
            custom_terminal.expect("codex-probe>")

    @real_codex
    def test_custom_directory_and_codex_on_path(self):
        custom = self.home / "custom codex"
        custom.mkdir()
        terminal = self.terminal("custom")
        terminal.expect("CODEX_DIR=" + re.escape(str(custom)), timeout=60)
        terminal.expect("codex-probe>")
        config = self.original + b'[mcp_servers.missing_fixture]\ncommand = "/missing/goblins-test-mcp"\n'
        (self.codex / "config.toml").write_bytes(config)
        native = self.d.start("native", agent_name="native")["session"]
        thread = self.d.wait(lambda: self.thread(native), timeout=60)
        # `codex` in the sandbox runs its own engine with the goblin's
        # settings, and does not move the goblin's conversation.
        status = self.exec(native, "codex", "login", "status")
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertIn("skipping MCP server missing_fixture", status.stderr)
        self.assertIn("Logged in using an API key", status.stdout + status.stderr)
        self.assertEqual(self.local(native, "status")["thread_id"], thread)
        self.assertEqual((self.codex / "config.toml").read_bytes(), config)

    @real_codex
    def test_codex_exec_on_path_runs_its_own_engine_with_the_goblins_policy(self):
        import subprocess
        native = self.d.start("native", agent_name="native")["session"]
        thread = self.d.wait(lambda: self.thread(native), timeout=60)
        # A scripted model on the sandbox's own loopback asks to run one command.
        provider = subprocess.Popen([str(self.app), "--state-dir", str(self.d.state), "exec", native, "--",
                                     "python3", "-c", PROVIDER, "18765", "/tmp/exec-marker"])
        self.addCleanup(provider.wait)
        self.addCleanup(provider.kill)
        time.sleep(1)
        result = self.exec(native, "sh", "-c",
            'codex exec --skip-git-repo-check -m fixture-model -c model_provider=\\"scripted\\" '
            '-c "model_providers.scripted={name=\\"scripted\\",base_url=\\"http://127.0.0.1:18765/v1\\",'
            'wire_api=\\"responses\\",requires_openai_auth=false,supports_websockets=false}" "run it" </dev/null')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # Codex's own sandbox cannot run in a goblin: the command ran with the
        # goblin's full access, and the goblin's conversation did not move.
        self.assertEqual(self.exec(native, "sh", "-c", "test -e /tmp/exec-marker").returncode, 0, result.stdout)
        self.assertEqual(self.local(native, "status")["thread_id"], thread)

    @real_codex
    def test_plugin_mcp_filter_keeps_valid_transports(self):
        plugin = self.codex / "plugins/cache/test/fixture/local"
        (plugin / ".codex-plugin").mkdir(parents=True)
        manifest = '{"name":"fixture"}'
        (plugin / ".codex-plugin/plugin.json").write_text(manifest)
        missing_command = '/missing/node "quoted" \\backslash'
        plugin_config = json.dumps({"mcpServers": {
            "cua_repl": {"command": missing_command},
        }})
        (plugin / ".mcp.json").write_text(plugin_config)
        config = (
            '[features]\nplugins=true\n'
            '[plugins."fixture@test"]\nenabled=true\n'
            '[mcp_servers.node_repl]\ncommand="/missing/node_repl"\n'
            '[mcp_servers.available_fixture]\ncommand="/bin/sh"\n'
            '[mcp_servers.http_fixture]\nurl="https://mcp.fixture.invalid/mcp"\n'
        )
        (self.codex / "config.toml").write_text(config)
        native = self.d.start("native", agent_name="native")["session"]
        result = self.exec(native, "codex", "mcp", "list", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping MCP server cua_repl", result.stderr)
        self.assertIn("skipping MCP server node_repl", result.stderr)
        listing = json.loads(result.stdout)
        servers = {entry["name"]: entry for entry in listing}
        for name in ["cua_repl", "node_repl"]:
            self.assertFalse(servers[name]["enabled"])
            self.assertEqual(servers[name]["transport"]["type"], "stdio")
        self.assertEqual(servers["cua_repl"]["transport"]["command"], missing_command)
        self.assertTrue(servers["available_fixture"]["enabled"])
        self.assertTrue(servers["http_fixture"]["enabled"])
        self.assertEqual((self.codex / "config.toml").read_text(), config)
        self.assertEqual((plugin / ".mcp.json").read_text(), plugin_config)
        self.assertEqual((plugin / ".codex-plugin/plugin.json").read_text(), manifest)

    def local(self, receiver, command):
        return json.loads(__import__("subprocess").check_output([
            str(self.app), "--state-dir", str(self.d.state), "exec", receiver, "--",
            "goblins-codex", command,
        ]))

    def adapter_status(self, receiver):
        """The adapter snapshot, or None until it writes its first status."""
        import subprocess
        result = subprocess.run([str(self.app), "--state-dir", str(self.d.state), "exec", receiver, "--",
                                 "goblins-codex", "status"], capture_output=True, text=True)
        if result.returncode and "No such file or directory" in result.stderr:
            return None
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def thread(self, receiver):
        """The adapter's conversation, or None until it writes its first status."""
        status = self.adapter_status(receiver)
        return status["thread_id"] if status is not None else None

    def native_status(self, receiver):
        import subprocess
        code = "import pathlib; print(pathlib.Path('/tmp/goblins-codex/native-status.json').read_text())"
        return json.loads(subprocess.check_output([str(self.app),"--state-dir",str(self.d.state),"exec",receiver,"--","python3","-c",code]))

    def test_a_codex_goblin_handles_its_inbox_and_resumes_its_conversation(self):
        receiver = self.d.start("notifier", agent_name="receiver")["session"]
        def paused_and_quiescent():
            # Pause writes controls; only a later observer snapshot applies it.
            status = self.adapter_status(receiver)
            if (status is None or status.get("paused") is not True or status.get("agent_state") != "idle"
                    or "diagnostic" not in status or status["diagnostic"] is not None):
                return None
            thread, threads = status.get("thread_id"), status.get("threads")
            if not isinstance(thread, str) or not thread or not isinstance(threads, dict) or thread not in threads:
                return None
            delivery = threads[thread]
            if (not isinstance(delivery, dict) or "pending" not in delivery
                    or delivery["pending"] is not None or delivery.get("uncertain") is not False):
                return None
            return status
        with step('pause delivery, queue a message, and resume while detached'):
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            self.local(receiver, "pause")
            self.d.wait(paused_and_quiescent)
            self.d.rpc.call("messages.send", {"key":"wake", "to":receiver, "body":"private inbox content"})
            self.assertEqual(self.sandbox(receiver,"inbox.status",{})["queued"],1)
            self.local(receiver,"resume")
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 1)
            response=self.d.rpc.call("inbox.next",{"key":"reply"})["message"]
            self.assertEqual(response["body"],"processed: private inbox content")
            events=self.d.rpc.call("communications.list",{})["events"]
            self.assertFalse(any(e["kind"].startswith("integration.") for e in events))
        with step('attach while inbox turns preserve the human draft'):
            peer=Terminal([str(self.app),"--state-dir",str(self.d.state),"attach",receiver])
            self.addCleanup(peer.close)
            peer.expect("› ")
            peer.send("busy")
            self.d.rpc.call("messages.send",{"key":"draft", "to":receiver,"body":"during human input"})
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 1)
            # The original human draft survives a separately queued inbox turn.
            self.d.wait(lambda: self.native_status(receiver)["draft"] == "busy")
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            peer.send("\r")
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "working")
            self.d.rpc.call("messages.send",{"key":"working", "to":receiver,"body":"during work"})
            self.assertEqual(self.sandbox(receiver,"inbox.status",{})["queued"],1)
            self.d.cli("exec", receiver, "--", "python3", "-c",
                       "from pathlib import Path; Path('/tmp/fake-codex-release').touch()")
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 2)
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            for text in ("failed","interrupted"):
                peer.send(text+"\r")
                self.d.rpc.call("messages.send",{"key":text,"to":receiver,"body":text})
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 4)
        with step('quitting detaches and delivery continues'):
            # Quitting the terminal detaches; the goblin and its delivery go on.
            peer.send("exit\r")
            self.assertEqual(peer.wait(), 0)
            record = self.d.wait(lambda: (r if (r := self.d.get(receiver))["terminal_detached"] else None))
            self.assertEqual(record["state"], "running")
            self.d.rpc.call("messages.send",{"key":"detached","to":receiver,"body":"while detached"})
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 5)
        with step('handle a message by hand while paused'):
            # Messages are handled by hand while delivery is paused.
            self.local(receiver, "pause")
            self.d.wait(paused_and_quiescent)
            inbox = self.sandbox(receiver, "inbox.status", {})
            self.assertEqual((inbox["queued"], inbox["claim"], inbox["head"]), (0, None, None))
            manual = self.d.rpc.call("messages.send",{"key":"manual","to":receiver,"body":"manual fallback"})
            inbox = self.sandbox(receiver, "inbox.status", {})
            self.assertEqual((inbox["queued"], inbox["claim"]), (1, None))
            self.assertEqual(inbox["head"][0], manual["id"])
            import subprocess
            command = [str(self.app),"--state-dir",str(self.d.state),"exec",receiver,"--","/run/goblins/bin/goblin"]
            item = json.loads(subprocess.check_output([*command,"inbox","next"]))
            self.assertIsNotNone(item["message"])
            self.assertEqual((item["message"]["id"], item["message"]["body"]),
                             (manual["id"], "manual fallback"))
            subprocess.run([*command,"reply",item["message"]["id"],"--claim-generation",str(item["claim_generation"]),"--message","processed manually"],check=True,capture_output=True)
            self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 6)
        with step("a member replies on its own conversation"):
            owner_thread = self.thread(receiver)
            member = self.d.member(receiver, "notifier", agent_name="member", wait=False)["session"]
            self.d.wait(lambda: self.d.get(member)["agent_state"] == "idle")
            member_thread = self.d.wait(lambda: self.thread(member))
            self.assertNotEqual(owner_thread, member_thread)
            # Complete the earlier replies before reading this member's reply.
            for _ in range(8):
                if not self.d.rpc.call("inbox.status", {})["queued"]:
                    break
                item = self.d.rpc.call("inbox.next", {"key": uuid.uuid4().hex})
                self.d.rpc.call("inbox.complete", {"key": uuid.uuid4().hex,
                    "message": item["message"]["id"], "claim_generation": item["claim_generation"]})
            self.d.rpc.call("messages.send", {"key":"member-message", "to":member,"body":"member only"})
            self.d.wait(lambda: self.d.rpc.call("inbox.status", {})["queued"] == 1)
            reply = self.d.rpc.call("inbox.next", {"key":"member-reply"})["message"]
            self.assertEqual((reply["sender"], reply["body"]), (member, "processed: member only"))
            self.assertEqual(self.thread(receiver), owner_thread)
            self.assertEqual(self.sandbox(receiver,"inbox.status",{})["queued"],0)
            self.d.rpc.call("sessions.stop", {"session":member})
            self.d.wait(lambda: self.d.get(member)["state"] == "stopped")
            self.assertEqual(self.d.get(member)["agent_state"], "unknown")
            self.assertEqual(self.d.get(receiver)["state"], "running")
        with step("stop and resume continue the recorded conversation"):
            self.d.rpc.call("sessions.stop", {"session":receiver})
            self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
            self.d.wait_record_released(self.d.record(receiver)["id"])
            launch = self.d.rpc.call("sessions.start", {"key":uuid.uuid4().hex,
                "configuration":self.d.manifest,"recreate":receiver,"rows":24,"cols":100,"detached":True})
            previous, receiver = receiver, launch["session"]
            self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
            self.d.wait(lambda: self.thread(receiver) == owner_thread)
            self.d.wait(lambda: self.d.record(previous)["variables"] ==
                {"session_id":owner_thread,"session_name":"Resumed thread"})
            self.assertEqual(self.d.record(previous)["sessions"], [previous,receiver])
        with step("losing the adapter stops the goblin"):
            # Without its adapter, the goblin has no conversation: it stops.
            self.local(receiver, "resume")
            peer = Terminal([str(self.app),"--state-dir",str(self.d.state),"attach",receiver])
            self.addCleanup(peer.close)
            peer.expect("› ")
            peer.send("notifier-fail\r")
            self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
            self.assertEqual(self.d.get(receiver)["agent_state"],"unknown")
            self.assertNotEqual(self.d.get(receiver)["exit_code"],0)

    @real_codex
    def test_native_terminal_attaches_quits_to_detach_and_reattaches(self):
        # Trust only this test workspace in the synthetic native configuration.
        (self.codex / "config.toml").write_text(
            'model = "gpt-5.3-codex"\n'
            f'[projects.{json.dumps(str(ROOT))}]\ntrust_level = "trusted"\n'
        )
        native = self.terminal("native", "native")
        screen_wait(native, lambda text: "gpt-5.3-codex" in text, timeout=60)
        session = self.d.rpc.call("sessions.get", {"session": "native"})["id"]
        thread = self.d.wait(lambda: self.thread(session))
        self.assertEqual(self.d.record(session)["variables"]["session_id"], thread)
        native.send("/quit")
        time.sleep(.5)
        native.send("\r")
        self.assertEqual(native.wait(), 0)
        record = self.d.wait(lambda: (r if (r := self.d.get(session))["terminal_detached"] else None))
        self.assertEqual(record["state"], "running")
        again = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", "native"])
        self.addCleanup(again.close)
        screen_wait(again, lambda text: "gpt-5.3-codex" in text, timeout=60)
        self.assertEqual(self.local(session, "status")["thread_id"], thread)

if __name__ == "__main__":
    unittest.main()
