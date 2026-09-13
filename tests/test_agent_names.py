"""Daemon-owned launch names, concurrent allocation and immutable identity."""
from concurrent.futures import ThreadPoolExecutor
import json
import subprocess
import threading
import unittest
from daemon_support import Daemon, ROOT, receive
from terminal_support import Terminal


class AgentNameTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def params(self, key, **extra):
        return dict(key=key, configuration=self.d.manifest, name="shell", rows=24, cols=100, **extra)

    def concurrent(self, launches):
        barrier = threading.Barrier(len(launches))
        def start(params):
            client = self.d.host()
            try:
                barrier.wait(timeout=10)
                try:
                    return client.call("sessions.start", params)
                except ValueError as error:
                    return error.args[0]
            finally:
                client.close()
        with ThreadPoolExecutor(max_workers=len(launches)) as pool:
            return list(pool.map(start, launches))

    def test_concurrent_names_exhaustion_collisions_and_retry_at_capacity(self):
        d = self.d
        self.assertIn("agent-names", d.rpc.init["features"])
        same = self.params("same-launch", agent_name="snikk")
        same_results = self.concurrent([same, same])
        self.assertEqual(same_results[0], same_results[1])
        self.assertIn("session", same_results[0])
        attempts = [self.params("explicit-" + str(i), agent_name="grib") for i in range(2)]
        results = self.concurrent(attempts)
        winner = next(i for i, r in enumerate(results) if "session" in r)
        self.assertEqual(results[1 - winner]["code"], -32009)
        self.assertIn("already in use", results[1 - winner]["message"])
        automatic = [self.params("auto-" + str(i)) for i in range(14)]
        allocated = self.concurrent(automatic)
        self.assertTrue(all("session" in r for r in allocated), allocated)
        names = {r["agent_name"] for r in allocated} | {"snikk", "grib"}
        expected = set((ROOT / "crates/controller/src/goblin-names.txt").read_text().splitlines())
        self.assertEqual(names, expected)
        with self.assertRaisesRegex(ValueError, "automatic agent name pool exhausted"):
            d.rpc.call("sessions.start", self.params("exhausted"))
        repeated = self.concurrent([attempts[winner], attempts[winner], automatic[0], automatic[0]])
        self.assertEqual(repeated[:2], [results[winner]] * 2)
        self.assertEqual(repeated[2:], [allocated[0]] * 2)
        changed = dict(automatic[0], agent_name="custom")
        with self.assertRaisesRegex(ValueError, "conflicting"):
            d.rpc.call("sessions.start", changed)
        self.assertEqual(len(d.rpc.call("sessions.list", {})), 16)
        print("EVIDENCE concurrent explicit collision, 16 unique live names, bounded pool exhaustion and concurrent retries at capacity", flush=True)

    def test_name_reuse_retains_permission_history_and_endpoint_identity(self):
        d = self.d
        first = d.start(key="first", agent_name="snikk")
        peer, permission = d.pending(first["session"])
        self.addCleanup(peer.close)
        self.assertEqual(permission["agent_name"], "snikk")
        sub = d.host(); self.addCleanup(sub.close)
        snapshot = sub.call("state.subscribe", {})["snapshot"]
        self.assertEqual(snapshot["sessions"][0]["agent_name"], "snikk")
        self.assertEqual(snapshot["permissions"][0]["agent_name"], "snikk")
        # A display name is never an approval's session identity.
        with self.assertRaises(ValueError):
            d.decide(dict(permission, session="snikk"), True)
        d.decide(permission, False)
        self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        event = receive(sub.peer)["params"]["snapshot"]
        self.assertEqual(event["permissions"][0]["agent_name"], "snikk")
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "serve", "--plain"])
        self.addCleanup(ui.close)
        ui.expect(r'"agent_name":"snikk"')
        ui.send("status\rquit\r"); self.assertEqual(ui.wait(), 0)
        d.rpc.call("sessions.stop", {"session": "snikk"})
        def released():
            try:
                d.rpc.call("sessions.get", {"session": "snikk"})
            except ValueError:
                return True
        d.wait(released)
        second = d.start(agent_name="snikk")
        self.assertNotEqual(first["session"], second["session"])
        self.assertEqual(d.rpc.call("sessions.get", {"session": "snikk"})["id"], second["session"])
        self.assertEqual(d.get(first["session"])["agent_name"], "snikk")
        self.assertEqual(d.start(key="first", agent_name="snikk", wait=False), first)
        history = d.rpc.call("permissions.get", {"request": permission["id"]})
        self.assertEqual(history["session"], first["session"])
        self.assertEqual(history["agent_name"], "snikk")
        self.assertEqual(history["state"], "denied")
        fresh, current = d.pending(second["session"]); self.addCleanup(fresh.close)
        with self.assertRaises(ValueError):
            d.decide(permission, True)
        self.assertEqual(d.rpc.call("permissions.get", {"request": current["id"]})["state"], "pending")

    def test_cli_run_list_and_stop_by_name_or_id(self):
        d = self.d
        for args, requested in [(["run", "shell", "--name", "scout"], "scout"),
                                (["run", "shell", "--name", "builder-2"], "builder-2"),
                                (["run", "shell"], None)]:
            ui = Terminal([str(d.app), "--state-dir", str(d.state), *args]); self.addCleanup(ui.close)
            ui.expect("workspace[>#]")
            listing = subprocess.check_output([str(d.app), "--state-dir", str(d.state), "list"], text=True)
            record = next(r for r in json.loads(listing) if r["state"] == "running")
            self.assertEqual(record["name"], "shell")
            self.assertEqual(record["configuration"], d.manifest)
            if requested:
                self.assertEqual(record["agent_name"], requested)
            else:
                self.assertIn(record["agent_name"], (ROOT / "crates/controller/src/goblin-names.txt").read_text().splitlines())
            target = requested or record["id"]
            stopped = subprocess.check_output([str(d.app), "--state-dir", str(d.state), "stop", target], text=True)
            self.assertEqual(json.loads(stopped), {"accepted": True})
            d.wait(lambda: d.get(record["id"])["state"] == "stopped")
        for args in [["list", "--name", "bad"], ["run", "shell", "--name"], ["run", "shell", "--name", "a", "--name", "b"]]:
            result = subprocess.run([str(d.app), "--state-dir", str(d.state), *args], capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
