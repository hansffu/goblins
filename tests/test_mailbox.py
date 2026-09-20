"""Deterministic inbox/CLI tests using real daemon sockets and sandbox terminals.

No coding agents, model calls, or subscription credentials are used here.
"""
import json
import subprocess
import tempfile
from pathlib import Path
import unittest
import uuid

from daemon_support import Daemon, RPC
from terminal_support import Terminal


class MailboxTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def cli(self, *args):
        return subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), *args],
                              capture_output=True, text=True, timeout=10)

    def child(self, parent):
        launch = self.sandbox(parent, "sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "agent_name": "scout", "detached": True,
        })
        self.d.wait(lambda: self.d.get(launch["session"])["state"] == "running")
        return launch["session"]

    def test_host_send_reply_and_audited_retry(self):
        session = self.d.start(agent_name="chief")["session"]
        text = 'my message\n literal $(false) "quoted" 🐟'
        sent = self.cli("send", "chief", "--message", text)
        self.assertEqual(sent.returncode, 0, sent.stderr)
        message = json.loads(sent.stdout)
        self.assertEqual(message["sender"], "host")
        self.assertIn(message["key"], sent.stderr)
        retry = self.cli("send", "chief", "--message", text, "--key", message["key"])
        self.assertEqual(json.loads(retry.stdout), message)
        claim = self.sandbox(session, "inbox.next", {"key": "fetch"})
        self.assertEqual(claim["message"]["body"], text)
        reply = self.sandbox(session, "messages.reply", {
            "key": "reply", "message": message["id"],
            "claim_generation": claim["claim_generation"], "body": "done",
        })
        incoming = self.cli("inbox", "next", "--key", "host-fetch")
        self.assertEqual(incoming.returncode, 0, incoming.stderr)
        self.assertEqual(json.loads(incoming.stdout)["message"]["id"], reply["reply"]["id"])
        events = self.cli("communications-log", "--session", "chief", "--json")
        self.assertEqual(events.returncode, 0, events.stderr)
        records = [json.loads(line) for line in events.stdout.splitlines()]
        accepted = [r for r in records if r["kind"] == "message.accepted"]
        self.assertEqual(len(accepted), 1)
        self.assertEqual(accepted[0]["data"]["change"]["message"]["body"], text)
        self.assertEqual([r["sequence"] for r in records], sorted(r["sequence"] for r in records))
        logs = list(self.d.state.glob("communications-*.jsonl"))
        self.assertEqual(len(logs), 1)
        self.assertEqual(logs[0].stat().st_mode & 0o777, 0o600)
        self.assertEqual(len(logs[0].read_text().splitlines()), len(records))

    def test_sender_local_file_crosses_private_tmp_by_contents(self):
        parent = self.d.start(agent_name="chief")["session"]
        child = self.child(parent)
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", parent])
        self.addCleanup(terminal.close)
        with tempfile.NamedTemporaryFile(prefix="goblins-message-", dir="/tmp", delete=False) as host_file:
            host_file.write(b"host-only contents")
            path = Path(host_file.name)
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        # The same pathname refers to different files in the two namespaces.
        terminal.send(f"printf 'sandbox-only contents' > {path}; goblins send scout --file {path} --key local-file; rm {path}; printf 'MAILBOX_SENT\\n'\n")
        output = terminal.expect(r"(?s)(.*?)\nMAILBOX_SENT\n").group(1).decode(errors="replace")
        self.assertEqual(path.read_text(), "host-only contents")
        path.unlink()
        claim = self.sandbox(child, "inbox.next", {"key": "fetch"})
        self.assertIsNotNone(claim["message"], output)
        self.assertEqual(claim["message"]["body"], "sandbox-only contents")
        self.assertEqual(claim["message"]["sender"], parent)

    def test_orderly_shutdown_audits_pending_host_and_sandbox_items(self):
        session = self.d.start(agent_name="chief")["session"]
        sent = json.loads(self.cli("send", "chief", "--message", "first").stdout)
        claim = self.sandbox(session, "inbox.next", {"key": "fetch"})
        reply = self.sandbox(session, "messages.reply", {
            "key": "reply", "message": sent["id"],
            "claim_generation": claim["claim_generation"], "body": "done",
        })
        pending = json.loads(self.cli("send", "chief", "--message", "second").stdout)
        stopped = self.cli("server", "stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertEqual(self.d.process.wait(timeout=10), 0)
        records = [json.loads(line) for line in next(self.d.state.glob("communications-*.jsonl")).read_text().splitlines()]
        failures = {r["data"]["recipient"]: r["data"]["messages"]
                    for r in records if r["kind"] == "recipient.stopped"}
        self.assertEqual(failures[session], [pending["id"]])
        self.assertEqual(failures["host"], [reply["reply"]["id"]])
        log = next(self.d.state.glob("communications-*.jsonl"))
        offline = self.cli("communications-log", "--log", str(log), "--json")
        self.assertEqual(offline.returncode, 0, offline.stderr)
        self.assertEqual([json.loads(line) for line in offline.stdout.splitlines()], records)
        scoped = self.cli("communications-log", "--log", str(log), "--session", session, "--json")
        self.assertEqual(scoped.returncode, 0, scoped.stderr)
        self.assertTrue(scoped.stdout)


    def test_scope_body_validation_and_recipient_exit(self):
        parent = self.d.start(agent_name="chief")["session"]
        child = self.child(parent)
        other = self.d.start(agent_name="other")["session"]
        for target in [other, "absent"]:
            with self.assertRaises(ValueError) as error:
                self.sandbox(child, "messages.send", {"key": "denied", "to": target, "body": "x"})
            self.assertEqual(error.exception.args[0]["code"], -32004)
        with self.assertRaises(ValueError):
            self.sandbox(child, "messages.send", {"key": "spoof", "to": "parent", "body": "x", "sender": "host"})
        with self.assertRaises(ValueError) as error:
            self.sandbox(child, "communications.list", {})
        self.assertEqual(error.exception.args[0]["code"], -32601)
        # Worst-case JSON escaping must fit the new messaging frame bound.
        message = self.sandbox(parent, "messages.send", {"key": "large", "to": child, "body": "\0" * 8192})
        self.assertEqual(self.sandbox(child, "inbox.next", {"key": "large-fetch"})["message"]["body"], "\0" * 8192)
        self.d.rpc.call("sessions.stop", {"session": child})
        failed = self.d.wait(lambda: (m if (m := self.d.rpc.call("messages.get", {"message": message["id"]}))["state"] == "failed" else None))
        self.assertEqual(failed["failure"], "recipient stopped")
        invalid = self.cli("send", "chief", "--message", "x" * 8193)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertEqual(self.sandbox(parent, "inbox.status", {})["queued"], 0)


if __name__ == "__main__":
    unittest.main()
