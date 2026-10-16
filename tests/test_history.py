"""Recreate records through a real daemon: what a root goblin's record holds.

Each daemon gets a private XDG_STATE_HOME (daemon_support), never the user's.
"""
import json
import os
from pathlib import Path
import signal
import tempfile
import unittest

from daemon_support import Daemon, RPC, receive


class HistoryTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def sandbox(self, session, method, params, d=None):
        peer = RPC((d or self.d).state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def test_record_follows_grants_denials_and_variables(self):
        d = self.d
        launch = d.start()
        session = launch["session"]
        record = d.wait(lambda: d.record(session))
        self.assertEqual((record["version"], record["sessions"], record["configuration"], record["agent_name"]),
                         (1, [session], "shell", launch["agent_name"]))
        # A short random id of its own, unlike the goblin's.
        self.assertRegex(record["id"], r"^[0-9a-hjkmnp-tv-z]{8}$")
        self.assertEqual(record["packages"], {"approved": [], "denied": []})
        self.assertIsNone(record["stopped"])
        self.assertEqual(os.stat(d.history).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(d.history / f"{record['id']}.json").st_mode & 0o777, 0o600)
        # A host denial, then an approval: the latest final outcome wins.
        peer, request = d.pending(session)
        d.decide(request, False)
        self.assertEqual(receive(peer.peer)["result"]["status"], "denied"); peer.close()
        d.wait(lambda: d.record(session)["packages"]["denied"] == ["hello"])
        peer, request = d.pending(session)
        d.decide(request, True)
        peer.peer.settimeout(60)
        self.assertEqual(receive(peer.peer)["result"]["status"], "ready"); peer.close()
        record = d.wait(lambda: (r if (r := d.record(session))["packages"]["approved"] == ["hello"] else None))
        self.assertEqual(record["packages"]["denied"], [])
        # A withdrawn request changes nothing.
        peer, request = d.pending(session, "tree")
        peer.peer.sendall(b"x"); peer.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request": request["id"]})["state"] == "withdrawn")
        # Variables from the sandbox CLI, all keys in one call.
        result = d.cli("exec", session, "--", "/run/goblins/bin/goblins", "variable", "set",
                       "session_id=c8193c93-1", "session_name=Plan goblin session restore")
        self.assertEqual(json.loads(result.stdout), {"accepted": True, "recorded": True})
        record = d.wait(lambda: (r if (r := d.record(session))["variables"].get("session_id") else None))
        self.assertEqual(record["variables"], {"session_id": "c8193c93-1", "session_name": "Plan goblin session restore"})
        self.assertGreaterEqual(record["updated"], record["created"])
        refused = d.cli("exec", session, "--", "/run/goblins/bin/goblins", "variable", "set", "Bad=1", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("invalid variable name", refused.stderr)
        self.sandbox(session, "sessions.set-variables", {"variables": {"session_name": ""}})
        d.wait(lambda: d.record(session)["variables"] == {"session_id": "c8193c93-1"})
        # Children are neither recorded nor able to write their parent's record.
        child = self.sandbox(session, "sessions.start", {"key": "child", "name": "shell", "agent_name": "kid", "detached": True})
        d.wait(lambda: d.get(child["session"])["state"] == "running", timeout=60)
        self.assertEqual(self.sandbox(child["session"], "sessions.set-variables", {"variables": {"session_id": "other"}}),
                         {"accepted": True, "recorded": False})
        self.assertIsNone(d.record(child["session"]))
        self.assertEqual(d.record(session)["variables"], {"session_id": "c8193c93-1"})
        # The stop is recorded.
        d.rpc.call("sessions.stop", {"session": session, "kill_children": True})
        record = d.wait(lambda: (r if (r := d.record(session))["stopped"] else None))
        self.assertEqual(record["packages"]["approved"], ["hello"])
        self.assertEqual(sorted(p.name for p in d.history.iterdir()), [".lock", f"{record['id']}.json"])
        status = d.cli("server", "status").stdout
        self.assertIn(f"Session history in {d.history}", status)

    def test_record_survives_kill_and_a_second_daemon_runs_without_history(self):
        d = self.d
        session = d.start()["session"]
        d.wait(lambda: d.record(session))
        self.sandbox(session, "sessions.set-variables", {"variables": {"session_id": "survivor"}})
        d.wait(lambda: d.record(session)["variables"].get("session_id") == "survivor")
        # Another daemon on the same store runs, but keeps no history.
        other = Daemon(state_home=d.state_home); self.addCleanup(other.close)
        status = other.rpc.call("server.status", {})["history"]
        self.assertFalse(status["owner"])
        owner = f"goblins server (pid {d.process.pid}, --state-dir {d.state}) owns {d.history}"
        self.assertIn(owner, status["reason"])
        self.assertIn(f"No session history: {owner}", other.cli("server", "status").stdout)
        foreign = other.start()["session"]
        self.assertEqual(self.sandbox(foreign, "sessions.set-variables", {"variables": {"session_id": "x"}}, d=other),
                         {"accepted": True, "recorded": False})
        self.assertIsNone(d.record(foreign))
        other.close()
        # Nothing depends on a clean exit.
        d.process.send_signal(signal.SIGKILL); d.process.wait(timeout=10)
        record = d.record(session)
        self.assertEqual(record["variables"], {"session_id": "survivor"})
        self.assertIsNone(record["stopped"])
        # The store is free again for the next daemon.
        again = Daemon(state_home=d.state_home); self.addCleanup(again.close)
        self.assertTrue(again.rpc.call("server.status", {})["history"]["owner"])

    def test_a_server_that_takes_the_store_over_records_its_running_goblins(self):
        import uuid
        d = self.d
        other = Daemon(state_home=d.state_home); self.addCleanup(other.close)
        session = other.start()["session"]
        peer, request = other.pending(session)
        other.decide(request, True)
        peer.peer.settimeout(60)
        self.assertEqual(receive(peer.peer)["result"]["status"], "ready"); peer.close()
        self.assertEqual(self.sandbox(session, "sessions.set-variables", {"variables": {"session_id": "kept"}}, d=other),
                         {"accepted": True, "recorded": False})
        self.assertIsNone(d.record(session))
        d.process.terminate(); d.process.wait(timeout=10)
        other.wait(lambda: other.rpc.call("server.status", {})["history"]["owner"], timeout=15)
        record = other.wait(lambda: other.record(session))
        self.assertEqual(record["packages"]["approved"], ["hello"])
        self.assertEqual(record["variables"], {"session_id": "kept"})
        self.assertEqual(record["sessions"], [session])
        with self.assertRaises(ValueError) as refused:
            other.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": other.manifest,
                                              "recreate": session, "rows": 24, "cols": 100})
        self.assertIn("is live and runs it", refused.exception.args[0]["message"])

    def test_history_store_is_a_protected_path(self):
        d = self.d
        d.start()
        config = json.loads(Path(d.manifest).read_text())
        spec = json.loads(Path(config["goblins"]["shell"]["build_spec"]).read_text())
        for source in (d.history, d.state_home):
            spec["ro_dirs"] = [str(source)]
            spec_path = Path(d.temp.name) / "bind-spec.json"; spec_path.write_text(json.dumps(spec))
            config["goblins"]["shell"]["build_spec"] = str(spec_path)
            path = Path(d.temp.name) / "bound.json"; path.write_text(json.dumps(config))
            launch = d.start(manifest=path, wait=False)
            record = d.wait(lambda: (r if (r := d.get(launch["session"]))["state"] == "failed" else None))
            self.assertIn("protected path", record["detail"])
            self.assertIn(str(d.history), record["detail"])

if __name__ == "__main__": unittest.main(verbosity=2)
