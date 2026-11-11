"""Schema and real-boundary checks for C01, including legacy parameter forms."""
import copy
import json
import os
import socket
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

from jsonschema import ValidationError
import api_contract as contract
from daemon_support import Daemon, RPC, app, frame, receive


class ContractTests(unittest.TestCase):
    def test_offline_meta_schema_and_reference_are_current(self):
        contract.check()
        self.assertEqual(contract.REFERENCE.read_text(), contract.reference())

    def test_wire_fixtures_and_known_field_validation(self):
        fixtures = contract.read("fixtures.json")
        for key, endpoint, name in [("host_initialize", "host", "initialize"), ("sandbox_initialize", "sandbox", "initialize"), ("server_status", "host", "server.status"), ("server_status_unavailable", "host", "server.status"), ("server_stop", "host", "server.stop")]:
            reply = dict(jsonrpc="2.0", id=1, result=fixtures[key])
            self.assertTrue(contract.validate_reply(endpoint, name, reply))
            extended = dict(jsonrpc="2.0", id=1, result=fixtures["forward_compatibility"][key])
            self.assertTrue(contract.validate_reply(endpoint, name, extended))
            broken = copy.deepcopy(extended)
            field = "api" if name == "initialize" else "sessions" if name == "server.status" else "accepted"
            broken["result"][field] = "invalid-known-field-type"
            with self.assertRaises(ValidationError):
                contract.validate_reply(endpoint, name, broken)
        for key in ("server_status", "server_status_unavailable"):
            value = copy.deepcopy(fixtures[key]); value["history"]["owner"] = not value["history"]["owner"]
            with self.assertRaises(ValidationError):
                contract.validate_reply("host", "server.status", dict(jsonrpc="2.0", id=1, result=value))
        for missing in ("configuration", "features", "limits"):
            value = copy.deepcopy(fixtures["host_initialize"]); del value[missing]
            with self.assertRaises(ValidationError):
                contract.validate_reply("host", "initialize", dict(jsonrpc="2.0", id=1, result=value))

    def test_byte_limits_count_utf8_and_do_not_apply_defaults(self):
        validator = contract.schema_validator(contract.common()["x-envelope-schemas"]["Call"])
        value = {"jsonrpc": "2.0", "id": 1, "method": "é" * 32}
        validator.validate(value)
        self.assertNotIn("params", value)
        with self.assertRaises(ValidationError):
            validator.validate({**value, "method": "é" * 33})
        for bad in ("bad\n", "é", "", None, True, 1.0, 2**53):
            with self.assertRaises(ValidationError):
                validator.validate({**value, "id": bad})

    def test_parameter_corpus_matches_named_and_positional_schemas(self):
        for case in contract.read("fixtures.json")["parameter_cases"]:
            entry = contract.method("host", case["method"])
            schema = entry["x-positional-schema"] if isinstance(case["params"], list) else entry["x-params-schema"]
            self.assertEqual(contract.schema_validator(schema).is_valid(case["params"]), case["valid"], case)

    def test_shell_initialization_boundary_uses_contract_fixtures(self):
        script = (contract.ROOT / "nix/goblins/claude/rpc.sh").read_text()
        predicate = script.split("jq -e '\n", 1)[1].split("' <<<\"$init\"", 1)[0]
        fixtures = contract.read("fixtures.json")
        good = fixtures["sandbox_initialize"]
        bad = copy.deepcopy(good); bad["limits"]["calls"] = "invalid"
        extended = fixtures["forward_compatibility"]["sandbox_initialize"]
        historical = fixtures["historical_api1"]["sandbox_initialize"]
        for value, expected in [(good, 0), (extended, 0), (historical, 0), (fixtures["host_initialize"], 1), (bad, 1)]:
            result = subprocess.run(["jq", "-e", predicate], input=json.dumps(value), text=True, capture_output=True)
            self.assertEqual(result.returncode, expected, result.stderr)

    def test_legacy_reply_schema_does_not_weaken_current_producer_checks(self):
        historical = contract.read("fixtures.json")["historical_api1"]
        for endpoint, name, key in [("host", "initialize", "host_initialize"), ("sandbox", "initialize", "sandbox_initialize"), ("host", "server.status", "server_status")]:
            reply = dict(jsonrpc="2.0", id=1, result=historical[key])
            self.assertTrue(contract.validate_reply(endpoint, name, reply, compatibility=True))
            with self.assertRaises(ValidationError):
                contract.validate_reply(endpoint, name, reply)

    def test_coverage_matches_each_endpoint_and_notification_exchange(self):
        contract.check_coverage()

    def test_coverage_rejects_cross_endpoint_and_notification_misassignments(self):
        wrong = copy.deepcopy(contract.read("coverage.json"))
        wrong["pending"]["C02"]["sandbox"].remove("sessions.list")
        with self.assertRaisesRegex(ValueError, "sandbox coverage"):
            contract.check_coverage(wrong)
        wrong = copy.deepcopy(contract.read("coverage.json"))
        for row in wrong["notifications"]:
            if row["name"] == "host.submitted": row["endpoint"] = "host"
        with self.assertRaisesRegex(ValueError, "notification endpoint"):
            contract.check_coverage(wrong)

    def test_coverage_discovers_or_patterns_and_matches_allowlists(self):
        sources = contract.implementation_sources()
        changed = dict(sources)
        before = '"host.cancel" | "host.status" | "host.list"'
        changed["controller"] = sources["controller"].replace(before, before + ' | "host.new"', 1)
        self.assertNotEqual(changed["controller"], sources["controller"])
        with self.assertRaisesRegex(ValueError, "host coverage"):
            contract.check_coverage(sources=changed)
        changed = dict(sources)
        before = '"permissions.request"\n            | "flake.run"'
        changed["gate"] = sources["gate"].replace(before, '"sandbox.new" | ' + before, 1)
        self.assertNotEqual(changed["gate"], sources["gate"])
        with self.assertRaisesRegex(ValueError, "gate/controller disagree"):
            contract.check_coverage(sources=changed)

    def test_singleton_matches_guard_requires_an_endpoint_assignment(self):
        sources = contract.implementation_sources()
        changed = dict(sources)
        before = '"host.cancel" | "host.status" | "host.list"'
        changed["controller"] = sources["controller"].replace(
            before, 'm if matches!(m, "server.new") => Ok(json!({})),\n            ' + before, 1)
        self.assertNotEqual(changed["controller"], sources["controller"])
        with self.assertRaisesRegex(ValueError, "host coverage"):
            contract.check_coverage(sources=changed)
        assigned = copy.deepcopy(contract.read("coverage.json"))
        assigned["pending"]["C03"]["host"].append("server.new")
        contract.check_coverage(assigned, changed)

    def test_new_emitted_notification_requires_an_exchange_assignment(self):
        sources = contract.implementation_sources()
        changed = dict(sources)
        start = sources["controller"].index("fn publish(&mut self)")
        tail = sources["controller"][start:].replace(
            'for c in &mut self.connections {',
            'for c in &mut self.connections {\n c.queue(json!({"jsonrpc":"2.0","method":"state.new","params":{"id":"nested"}}));', 1)
        changed["controller"] = sources["controller"][:start] + tail
        self.assertNotEqual(changed["controller"], sources["controller"])
        with self.assertRaisesRegex(ValueError, "notification endpoint"):
            contract.check_coverage(sources=changed)
        assigned = copy.deepcopy(contract.read("coverage.json"))
        assigned["notifications"].append(dict(endpoint="host", name="state.new", direction="server-to-client", chunk="C15"))
        contract.check_coverage(assigned, changed)


class HistoricalClientTests(unittest.TestCase):
    def legacy_cli(self, command):
        binary = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary = re.search(r'exec (\S+) --runtime', app().read_text()).group(1)
        historical = contract.read("fixtures.json")["historical_api1"]
        errors, requests = [], []
        with tempfile.TemporaryDirectory(prefix="gc-legacy-") as root:
            state = Path(root) / "state"
            state.mkdir(mode=0o700)
            with socket.socket(socket.AF_UNIX) as listener:
                listener.settimeout(5)
                listener.bind(str(state / "host.sock")); listener.listen()
                def serve():
                    try:
                        peer, _ = listener.accept()
                        with peer:
                            peer.settimeout(5)
                            init = receive(peer)
                            requests.append(init)
                            peer.sendall(frame(dict(jsonrpc="2.0", id=init["id"], result=historical["host_initialize"])))
                            call = receive(peer)
                            requests.append(call)
                            peer.sendall(frame(dict(jsonrpc="2.0", id=call["id"], result=historical["server_" + command])))
                    except Exception as error:
                        errors.append(error)
                server = threading.Thread(target=serve, daemon=True)
                server.start()
                result = subprocess.run([binary, "--state-dir", str(state), "server", command], text=True, capture_output=True, timeout=10)
                server.join(timeout=6)
                self.assertFalse(server.is_alive(), "legacy daemon exchange did not finish")
                self.assertEqual(errors, [])
                self.assertEqual([row["method"] for row in requests], ["initialize", "server." + command])
                self.assertEqual(result.returncode, 0, result.stderr)
                return result.stdout

    def test_real_status_cli_accepts_historical_api1_daemon(self):
        output = self.legacy_cli("status")
        self.assertIn("Server running", output)
        self.assertIn("0 live agents", output)
        self.assertNotIn("Session history", output)
        self.assertNotIn("No session history", output)

    def test_real_stop_cli_accepts_historical_api1_daemon(self):
        self.assertIn("Server stopped", self.legacy_cli("stop"))


class ContractBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def raw(self):
        peer = socket.socket(socket.AF_UNIX)
        self.addCleanup(peer.close)
        peer.settimeout(5)
        peer.connect(str(self.d.state / "host.sock"))
        return peer

    def exchange(self, peer, name, params, id=1):
        peer.sendall(frame(dict(jsonrpc="2.0", id=id, method=name, params=params)))
        reply = receive(peer)
        contract.validate_reply("host", name, reply)
        return reply

    def test_parameter_forms_bounds_and_initialization_precedence(self):
        for value in ({}, [], None, {"api": None}, {"api": True}, {"api": -1}, {"api": 2**32}, {"api": 1, "extra": 1}, [1, 2]):
            peer = self.raw()
            self.assertEqual(self.exchange(peer, "initialize", value)["error"]["code"], -32602)
        peer = self.raw()
        self.assertEqual(self.exchange(peer, "initialize", [0])["error"]["code"], -32002)
        self.assertEqual(self.exchange(peer, "initialize", [1], 2)["result"]["api"], 1)
        self.assertEqual(self.exchange(peer, "initialize", {"api": None}, 3)["error"]["code"], -32602)
        self.assertEqual(self.exchange(peer, "initialize", {"api": 0}, 4)["error"]["code"], -32009)
        self.assertEqual(self.exchange(peer, "server.status", [], 5)["result"]["sessions"], 0)
        for value in (None, {"extra": 1}, [1]):
            self.assertEqual(self.exchange(peer, "server.status", value, 6)["error"]["code"], -32602)
        peer.sendall(frame(dict(jsonrpc="2.0", id=7, method="server.status")))
        contract.validate_reply("host", "server.status", receive(peer))

    def test_sandbox_initialization_has_its_own_role_and_limits(self):
        launch = self.d.start()
        self.d.wait(lambda: self.d.get(launch["session"])["state"] == "running")
        rpc = RPC(self.d.state / launch["session"] / "resources/request.sock")
        self.addCleanup(rpc.close)
        self.assertEqual(rpc.init["role"], "sandbox")
        self.assertEqual(rpc.init["limits"]["calls"], 2)

    def test_stop_acceptance_closes_the_connection(self):
        peer = self.raw()
        self.exchange(peer, "initialize", {"api": 1})
        self.assertEqual(self.exchange(peer, "server.stop", [], 2)["result"], {"accepted": True})
        self.assertEqual(peer.recv(1), b"")
