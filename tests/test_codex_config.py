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
from support import command
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


# These run the real Codex, whose prompts change between versions.
@unittest.skipUnless(os.environ.get("GOBLINS_TEST_CODEX"), "set GOBLINS_TEST_CODEX=1 to test Codex")
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

    def test_mounts_attach_and_member_inheritance(self):
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

    def test_a_goblin_with_nix_gets_the_nix_skill_instead_of_packages(self):
        terminal = self.terminal("nix", "nixer")
        terminal.expect("codex-probe>")
        skills = "/etc/codex/skills"
        terminal.send(
            f"test -r {skills}/goblins-nix/SKILL.md && test -r {skills}/goblins-messaging/SKILL.md "
            f"&& test ! -e {skills}/goblins-packages && test ! -e {skills}/goblins-devshell "
            "&& command -v nix >/dev/null; printf 'NIX_SKILLS=%s\\n' \"$?\"\n"
        )
        terminal.expect(r"(?:^|\n)NIX_SKILLS=0\n")

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

    def thread(self, receiver):
        """The adapter's conversation, or None until it writes its first status."""
        import subprocess
        result = subprocess.run([str(self.app), "--state-dir", str(self.d.state), "exec", receiver, "--",
                                 "goblins-codex", "status"], capture_output=True, text=True)
        if result.returncode and "No such file or directory" in result.stderr:
            return None
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)["thread_id"]

    def native_status(self, receiver):
        import subprocess
        code = "import pathlib; print(pathlib.Path('/tmp/goblins-codex/native-status.json').read_text())"
        return json.loads(subprocess.check_output([str(self.app),"--state-dir",str(self.d.state),"exec",receiver,"--","python3","-c",code]))

    def test_notify_then_fetch_without_model_or_attachment(self):
        receiver = self.d.start("notifier", agent_name="receiver")["session"]
        self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
        self.local(receiver, "pause")
        self.d.rpc.call("messages.send", {"key":"wake", "to":receiver, "body":"private inbox content"})
        import time
        time.sleep(1)
        self.assertEqual(self.sandbox(receiver,"inbox.status",{})["queued"],1)
        self.local(receiver,"resume")
        self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 1)
        response=self.d.rpc.call("inbox.next",{"key":"reply"})["message"]
        self.assertEqual(response["body"],"processed: private inbox content")
        events=self.d.rpc.call("communications.list",{})["events"]
        self.assertFalse(any(e["kind"].startswith("integration.") for e in events))
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
        time.sleep(.7)
        self.assertEqual(self.sandbox(receiver,"inbox.status",{})["queued"],1)
        self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 2)
        self.d.wait(lambda: self.d.get(receiver)["agent_state"] == "idle")
        for text in ("failed","interrupted"):
            peer.send(text+"\r")
            self.d.rpc.call("messages.send",{"key":text,"to":receiver,"body":text})
        self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 4)
        # Quitting the terminal detaches; the goblin and its delivery go on.
        peer.send("exit\r")
        self.assertEqual(peer.wait(), 0)
        record = self.d.wait(lambda: (r if (r := self.d.get(receiver))["terminal_detached"] else None))
        self.assertEqual(record["state"], "running")
        self.d.rpc.call("messages.send",{"key":"detached","to":receiver,"body":"while detached"})
        self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 5)
        # Messages are handled by hand while delivery is paused.
        self.local(receiver, "pause")
        self.d.rpc.call("messages.send",{"key":"manual","to":receiver,"body":"manual fallback"})
        import subprocess
        command = [str(self.app),"--state-dir",str(self.d.state),"exec",receiver,"--","/run/goblins/bin/goblin"]
        item = json.loads(subprocess.check_output([*command,"inbox","next"]))
        subprocess.run([*command,"reply",item["message"]["id"],"--claim-generation",str(item["claim_generation"]),"--message","processed manually"],check=True,capture_output=True)
        self.d.wait(lambda: self.d.rpc.call("inbox.status",{})["queued"] == 6)
        # Without its adapter, the goblin has no conversation: it stops.
        self.local(receiver, "resume")
        peer = Terminal([str(self.app),"--state-dir",str(self.d.state),"attach",receiver])
        self.addCleanup(peer.close)
        peer.expect("› ")
        peer.send("notifier-fail\r")
        self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
        self.assertEqual(self.d.get(receiver)["agent_state"],"unknown")
        self.assertNotEqual(self.d.get(receiver)["exit_code"],0)

    def test_recreate_loads_the_recorded_thread_with_the_goblins_policy(self):
        self.assertEqual(self.manifest["goblins"]["codex"]["resume_command"],
                         ["-c", 'model="literal $(false) {{model}}"', "--thread", "{session_id}"])
        self.assertEqual(self.manifest["goblins"]["profile"]["resume_command"],
                         [*self.manifest["goblins"]["profile"]["args"], "--thread", "{session_id}"])
        receiver = self.d.start("notifier", agent_name="receiver")["session"]
        # The adapter names a new conversation after the goblin and records it.
        thread = self.d.wait(lambda: (self.d.record(receiver) or {}).get("variables", {}).get("session_id"))
        self.assertEqual(self.d.record(receiver)["variables"]["session_name"], "receiver")
        import subprocess
        def recorded(session, file):
            return json.loads(subprocess.check_output([str(self.app), "--state-dir", str(self.d.state),
                "exec", session, "--", "python3", "-c", "print(open('/tmp/goblins-codex/"+file+".json').read())"]))
        params = recorded(receiver, "start-params")
        self.assertEqual((params["sandbox"], params["approvalPolicy"]), ("danger-full-access", "on-request"))
        # The terminal joins it as it is, without permission flags.
        peer = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", receiver])
        self.addCleanup(peer.close)
        peer.expect("› ")
        args = recorded(receiver, "native-args")
        self.assertEqual(args[-2:], ["resume", thread])
        self.assertIn("--remote", args)
        self.assertNotIn("--sandbox", args)
        self.assertNotIn("--ask-for-approval", args)
        self.d.rpc.call("sessions.stop", {"session": receiver})
        self.d.wait(lambda: self.d.get(receiver)["state"] == "stopped")
        launch = self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": self.d.manifest,
                                                    "recreate": receiver, "rows": 24, "cols": 100, "detached": True})
        again = launch["session"]
        self.d.wait(lambda: self.d.record(receiver)["variables"] ==
                    {"session_id": thread, "session_name": "Resumed thread"}, timeout=60)
        self.assertEqual(self.d.record(receiver)["sessions"], [receiver, again])
        # The adapter loads it with this configuration's permissions, here
        # Codex's default policy, since it does not bypass approvals.
        params = recorded(again, "resume-params")
        self.assertEqual((params["threadId"], params["approvalPolicy"], params["sandbox"]),
                         (thread, "on-request", "danger-full-access"))
        # A resume command that passes what is not a server option stops the
        # goblin and says why in its server log.
        config = json.loads(Path(self.d.manifest).read_text())
        config["goblins"]["notifier"]["resume_command"] = ["--sandbox", "read-only", "--thread", "{session_id}"]
        path = Path(self.d.temp.name) / "sandbox-resume.json"
        path.write_text(json.dumps(config))
        self.d.rpc.call("sessions.stop", {"session": again})
        self.d.wait(lambda: self.d.get(again)["state"] == "stopped")
        launch = self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": str(path),
                                                    "recreate": receiver, "rows": 24, "cols": 100, "detached": True})
        refused = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] == "stopped" else None), timeout=60)
        self.assertEqual(refused["exit_code"], 2)
        self.assertIn("--sandbox is not a Codex server option", Path(refused["server_log"]).read_text())

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

    def test_profile_and_cli_overrides_reach_local_server(self):
        profile = '[model_providers.fixture]\nbase_url="http://localhost/fixture"\n[features]\nplugins=false\n'
        path = self.codex / 'fixture.config.toml'
        path.write_text(profile)
        receiver = self.d.start('profile', agent_name='profile')['session']
        self.d.wait(lambda: self.d.get(receiver)['agent_state'] == 'idle')
        import subprocess
        code = "import pathlib; print(pathlib.Path('/tmp/goblins-codex/server-args.json').read_text())"
        args = json.loads(subprocess.check_output([str(self.app), '--state-dir', str(self.d.state), 'exec', receiver, '--', 'python3', '-c', code]))
        settings = {}
        for i, arg in enumerate(args):
            if arg == '-c' or arg == '--config':
                key, value = args[i+1].split('=', 1)
                settings[key] = tomllib.loads('value=' + value)['value']
        self.assertEqual(settings['model_providers']['fixture']['base_url'], 'http://localhost/fixture')
        self.assertEqual(settings['model_provider'], 'literal $(false) provider')
        self.assertEqual(settings['features'], {'plugins': False})
        # Full access always; the approval policy is Codex's own without
        # bypassPermissions.
        self.assertEqual(settings['sandbox_mode'], 'danger-full-access')
        self.assertNotIn('approval_policy', settings)
        self.assertIn('plugins', args)
        self.assertNotIn('--profile', args)
        self.assertEqual(path.read_text(), profile)

    def test_owner_and_member_servers_have_independent_bindings(self):
        owner = self.d.start('notifier', agent_name='owner')['session']
        self.d.wait(lambda: self.d.get(owner)['agent_state'] == 'idle')
        owner_thread = self.d.wait(lambda: self.thread(owner))
        member = self.d.member(owner, 'notifier', agent_name='member', wait=False)['session']
        record = self.d.wait(lambda: (r if (r := self.d.get(member))['agent_state'] == 'idle' or r['state'] in ('stopped', 'failed') else None), timeout=60)
        self.assertEqual(record['agent_state'], 'idle', record)
        member_thread = self.d.wait(lambda: self.thread(member))
        self.assertNotEqual(owner_thread, member_thread)
        self.local(owner, 'pause')
        self.d.rpc.call('messages.send', {'key': 'member-message', 'to': member, 'body': 'member only'})
        self.d.wait(lambda: self.d.rpc.call('inbox.status', {})['queued'] == 1)
        reply = self.d.rpc.call('inbox.next', {'key': 'member-reply'})['message']
        self.assertEqual(reply['sender'], member)
        self.assertEqual(reply['body'], 'processed: member only')
        self.assertEqual(self.local(owner, 'status')['thread_id'], owner_thread)
        self.assertEqual(self.sandbox(owner, 'inbox.status', {})['queued'], 0)
        # Nothing cascades: each goblin is stopped on its own.
        for session in (member, owner):
            self.d.rpc.call('sessions.stop', {'session': session})
        self.d.wait(lambda: all(self.d.get(s)['state'] == 'stopped' for s in (owner, member)))
        self.assertTrue(all(self.d.get(s)['agent_state'] == 'unknown' for s in (owner, member)))



if __name__ == "__main__":
    unittest.main()
