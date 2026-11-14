"""Static contract checks: the JSON Schemas and OpenRPC documents against the fixture corpus.

Rust never validates against JSON Schema, so these checks are what keep the schemas themselves honest. How the Rust readers, CLIs and controller treat the same fixtures is tested in Rust.
"""
import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path

from jsonschema import ValidationError
import api_contract as contract

OPPOSITE = {"daemon-to-supervisor": "supervisor-to-daemon", "supervisor-to-daemon": "daemon-to-supervisor"}


def accepts(check):
    try:
        return check()
    except ValidationError:
        return False


def schema(name, cases, key="value"):
    """Cases checked against one shared definition."""
    return lambda fixtures: [(name, case, contract.schema_validator({"$ref": "common.json#/definitions/" + name}).is_valid(case[key]), case["valid"]) for case in fixtures[cases]]


def schemas(cases, suffix=""):
    """A map from definition name to its cases."""
    return lambda fixtures: [(name, case, contract.schema_validator({"$ref": "common.json#/definitions/" + name + suffix}).is_valid(case["value"]), case["valid"]) for name, rows in fixtures[cases].items() for case in rows]


def labelled_schema(cases, key="result"):
    """Cases naming their own definition."""
    return lambda fixtures: [(case["schema"], case, contract.schema_validator({"$ref": "common.json#/definitions/" + case["schema"]}).is_valid(case[key]), case["valid"]) for case in fixtures[cases]]


def parameter_schema(cases, endpoints):
    """Named or positional parameters against the method's parameter schemas alone."""
    def check(fixtures):
        rows = []
        for case in fixtures[cases]:
            for endpoint in [case["endpoint"]] if "endpoint" in case else endpoints:
                entry = contract.method(endpoint, case["method"])
                params = {} if case["params"] is None and entry.get("x-null-params-as-empty") else case["params"]
                chosen = entry["x-positional-schema"] if isinstance(params, list) else entry["x-params-schema"]
                rows.append((endpoint + " " + case["method"], case, contract.schema_validator(chosen).is_valid(params), case["valid"]))
        return rows
    return check


def parameters(cases, endpoints, name=None):
    """Parameters through the full request validation, including request constraints."""
    return lambda fixtures: [(endpoint + " " + (name or case["method"]), case, accepts(lambda: contract.validate_params(endpoint, name or case["method"], case["params"])), case["valid"]) for case in fixtures[cases] for endpoint in ([case["endpoint"]] if "endpoint" in case else endpoints)]


def results(cases, endpoints, name=None, valid="valid", wrap=lambda result: result):
    """Results against the current producer schema."""
    return lambda fixtures: [(endpoint + " " + (name or case["method"]), case, accepts(lambda: contract.validate_reply(endpoint, name or case["method"], dict(jsonrpc="2.0", id=1, result=wrap(case["result"])))), case[valid]) for case in fixtures[cases] for endpoint in endpoints]


def supervisor_notifications(cases, direction=None, one_way=True):
    """Private supervisor notifications; a one-way frame is never valid in the other direction."""
    def check(fixtures):
        rows = []
        for case in fixtures[cases]:
            sent = direction or case["direction"]
            rows.append((sent, case, accepts(lambda: contract.validate_notification("supervisor", sent, case["frame"])), case["valid"]))
            if one_way and sent in OPPOSITE:
                rows.append((OPPOSITE[sent], case, contract.validate_notification("supervisor", OPPOSITE[sent], case["frame"]), False))
        return rows
    return check


def notifications(cases, endpoint, direction, method=None, valid="valid"):
    """Public notifications, from their frame or from params of `method`."""
    def frame(case):
        return case["frame"] if method is None else dict(jsonrpc="2.0", method=method, params=case["params"])
    return lambda fixtures: [(direction, case, accepts(lambda: contract.validate_notification(endpoint, direction, frame(case))), case[valid]) for case in fixtures[cases]]


def process_roots(fixtures):
    return [row for root in fixtures["c17b1_process_roots"] for row in schema(root["schema"], root["cases"])(fixtures)]


def terminal_io_results(fixtures):
    rows = []
    for case in fixtures["terminal_io_result_cases"]:
        host_only = case["method"] in ("terminals.detach", "terminals.resize")
        rows += results("terminal_io_result_cases", ("host",) if host_only else ("host", "sandbox"))({"terminal_io_result_cases": [case]})
    return rows


def completions(fixtures):
    return [(name, case, accepts(lambda: contract.validate_completion(name, case["reply"])), case["valid"]) for case in fixtures["attach_completion_cases"] for name in ("attach.reply", "legacy.attach")]


def host_jobs(fixtures):
    return results("host_job_result_cases", ("host", "sandbox"), "host.status", "current")(fixtures) + results("host_job_result_cases", ("host", "sandbox"), "host.list", "current", lambda result: dict(jobs=[result]))(fixtures)


def permission_records(fixtures):
    return results("permission_record_cases", ("host",), "permissions.get", "current")(fixtures) + results("permission_record_cases", ("host",), "permissions.list", "current", lambda result: [result])(fixtures)


# Every *_cases key of libs/api/tests/fixtures.json, with how its cases are checked here.
CASES = {
    "parameter_cases": parameter_schema("parameter_cases", ("host",)),
    "self_parameter_cases": parameter_schema("self_parameter_cases", ("sandbox",)),
    "session_parameter_cases": parameter_schema("session_parameter_cases", ("host", "sandbox")),
    "lifecycle_parameter_cases": parameter_schema("lifecycle_parameter_cases", ()),
    "exec_parameter_cases": parameter_schema("exec_parameter_cases", ("host",)),
    "terminal_parameter_cases": parameter_schema("terminal_parameter_cases", ()),
    "terminal_io_parameter_cases": parameters("terminal_io_parameter_cases", ()),
    "state_parameter_cases": parameters("state_parameter_cases", ("host",)),
    "history_parameter_cases": parameters("history_parameter_cases", ("host",)),
    "host_job_parameter_cases": parameters("host_job_parameter_cases", ("host", "sandbox")),
    "host_raid_parameter_cases": parameters("host_raid_parameter_cases", ("host",)),
    "sandbox_raid_parameter_cases": parameters("sandbox_raid_parameter_cases", ("sandbox",)),
    "permission_read_parameter_cases": parameters("permission_read_parameter_cases", ("host",)),
    "permission_request_parameter_cases": parameters("permission_request_parameter_cases", ("sandbox",), "permissions.request"),
    "permission_decision_parameter_cases": parameters("permission_decision_parameter_cases", ("host",), "permissions.decide"),
    "editor_parameter_cases": parameters("editor_parameter_cases", ("sandbox",), "editor.open"),
    "devshell_diff_parameter_cases": parameters("devshell_diff_parameter_cases", ("sandbox",), "devshell.diff"),
    "devshell_app_parameter_cases": parameters("devshell_app_parameter_cases", ("sandbox",)),
    "mailbox_mutation_cases": parameters("mailbox_mutation_cases", ("host", "sandbox")),
    "self_result_cases": labelled_schema("self_result_cases"),
    "session_result_cases": labelled_schema("session_result_cases"),
    "lifecycle_result_cases": labelled_schema("lifecycle_result_cases"),
    "state_result_cases": results("state_result_cases", ("host",)),
    "history_result_cases": results("history_result_cases", ("host",)),
    "exec_result_cases": results("exec_result_cases", ("host",), "sessions.exec"),
    "terminal_result_cases": results("terminal_result_cases", ("host", "sandbox")),
    "terminal_io_result_cases": terminal_io_results,
    "devshell_app_result_cases": results("devshell_app_result_cases", ("sandbox",)),
    "editor_result_cases": results("editor_result_cases", ("sandbox",), "editor.open"),
    "permission_decision_result_cases": results("permission_decision_result_cases", ("host",), "permissions.decide"),
    "permission_request_result_cases": results("permission_request_result_cases", ("sandbox",), "permissions.request"),
    "permission_record_cases": permission_records,
    "host_job_result_cases": host_jobs,
    "raid_record_cases": results("raid_record_cases", ("host", "sandbox"), "raids.members", "current"),
    "mailbox_next_cases": results("mailbox_next_cases", ("host", "sandbox"), "inbox.next"),
    "state_snapshot_cases": schema("StateSnapshot", "state_snapshot_cases"),
    "state_notification_cases": notifications("state_notification_cases", "host", "server-to-client", "state.changed"),
    "exec_signal_cases": notifications("exec_signal_cases", "host", "client-to-server", "exec.signal"),
    "attach_hello_parameter_cases": notifications("attach_hello_parameter_cases", "attach", "client-to-server", "attach.hello"),
    "attach_waiting_cases": notifications("attach_waiting_cases", "attach", "server-to-client"),
    "attach_completion_cases": completions,
    "host_submitted_cases": notifications("host_submitted_cases", "sandbox", "server-to-client", valid="current"),
    "c16a1_supervisor_notifications": supervisor_notifications("c16a1_supervisor_notifications", one_way=False),
    "c16a1_maintenance_operations": schema("SupervisorMaintenanceOperation", "c16a1_maintenance_operations"),
    "c16a2_descriptor_cases": schema("SupervisorDescriptor", "c16a2_descriptor_cases"),
    "c16a2_start_projection_cases": schema("SupervisorStartRead", "c16a2_start_projection_cases"),
    "c16a2_maintenance_projection_cases": schema("SupervisorMaintenanceParams", "c16a2_maintenance_projection_cases"),
    "c16b1_stdio_cases": schema("SupervisorStdio", "c16b1_stdio_cases"),
    "c16b1_source_cases": schema("SupervisorSource", "c16b1_source_cases"),
    "c16b1_recreate_cases": schema("SupervisorRecreate", "c16b1_recreate_cases"),
    "c16b1_worktree_cases": schema("SupervisorWorktree", "c16b1_worktree_cases"),
    "c16b2_directory_cases": schema("SupervisorDirectory", "c16b2_directory_cases"),
    "c16b2_lease_cases": schema("SupervisorLease", "c16b2_lease_cases"),
    "c16b2_handoff_cases": schema("SupervisorHandoff", "c16b2_handoff_cases"),
    "c16b2_strict_bootstrap_cases": schema("SupervisorStartParams", "c16b2_strict_bootstrap_cases"),
    "c16b2_leases_notification_cases": supervisor_notifications("c16b2_leases_notification_cases", one_way=False),
    "c17a_command_start_cases": schema("SupervisorCommandStartParams", "c17a_command_start_cases"),
    "c17a_event_exported_cases": schema("SupervisorEventExportedParams", "c17a_event_exported_cases"),
    "c17a_event_manifest_cases": schema("SupervisorEventManifestParams", "c17a_event_manifest_cases"),
    "c17a_handoff_outcome_cases": schema("SupervisorHandoffOutcome", "c17a_handoff_outcome_cases"),
    "c17a_notification_cases": supervisor_notifications("c17a_notification_cases"),
    "c17b1_spawn_kind_cases": schema("SupervisorSpawnKind", "c17b1_spawn_kind_cases"),
    "c17b1_spawn_cases": process_roots,
    "c17b1_signal_cases": process_roots,
    "c17b1_cancel_process_cases": process_roots,
    "c17b1_kill_all_cases": process_roots,
    "c17b1_spawned_cases": process_roots,
    "c17b1_spawn_failed_cases": process_roots,
    "c17b1_exited_cases": process_roots,
    "c17b1_notification_cases": supervisor_notifications("c17b1_notification_cases"),
    "c17b2_terminal_schema_cases": schemas("c17b2_terminal_schema_cases"),
    "c17b2_notification_cases": supervisor_notifications("c17b2_notification_cases"),
    "c17c1_approval_schema_cases": schemas("c17c1_approval_schema_cases"),
    "c17c1_notification_cases": supervisor_notifications("c17c1_notification_cases"),
    "c17c2_devshell_schema_cases": schemas("c17c2_devshell_schema_cases"),
    "c17c2_inner_json_cases": schemas("c17c2_inner_json_cases", "/x-json-content-schema"),
    "c17c2_notification_cases": supervisor_notifications("c17c2_notification_cases"),
    "c17d1_host_job_schema_cases": schemas("c17d1_host_job_schema_cases"),
    "c17d1_notification_cases": supervisor_notifications("c17d1_notification_cases"),
    "c17d2_notification_cases": supervisor_notifications("c17d2_notification_cases"),
    "c17d3a_membership_outcome_cases": schema("SupervisorScopeMembershipOutcome", "c17d3a_membership_outcome_cases"),
    "c17d3a_notification_cases": supervisor_notifications("c17d3a_notification_cases", "daemon-to-supervisor"),
    "c17d3b1_started_policy_cases": schemas("c17d3b1_started_policy_cases"),
    "c17d3b2_started_value_cases": schemas("c17d3b2_started_value_cases"),
    "c17d3b3_restoration_cases": schemas("c17d3b3_restoration_cases"),
    "c17d3b4_started_cases": schema("SupervisorEventStartedParams", "c17d3b4_started_cases"),
    "c17d3b4_notification_cases": supervisor_notifications("c17d3b4_notification_cases", "supervisor-to-daemon"),
    "c17d3b4_inner_dev_shell_cases": schema("SupervisorStartedDevShellText/x-json-content-schema", "c17d3b4_inner_dev_shell_cases"),
}

# Cases only the Rust readers and producers use, or that need their own construction below.
ELSEWHERE = {
    "session_raid_cases": "the session raid reader (Rust)",
    "inbox_status_cases": "the inbox status reader (Rust)",
    "communications_page_cases": "the communications page reader (Rust)",
    "c16c_phase_method_cases": "the generated phase predicates (Rust)",
    "mailbox_byte_cases": "test_mailbox_byte_and_integer_bounds",
    "mailbox_u64_cases": "test_mailbox_byte_and_integer_bounds",
    "communication_payload_cases": "test_communications_parameters_and_payload_pairs",
    "communications_parameter_cases": "test_communications_parameters_and_payload_pairs",
}


class ContractTests(unittest.TestCase):
    def test_fixture_cases_match_the_schemas(self):
        fixtures = contract.read("fixtures.json")
        keys = {key for key in fixtures if key.endswith("_cases")}
        self.assertEqual(keys, {key for key in CASES if key.endswith("_cases")} | set(ELSEWHERE))
        for key, check in CASES.items():
            rows = check(fixtures)
            self.assertTrue(rows, key)
            for label, case, actual, expected in rows:
                with self.subTest(fixture=key, check=label, case=case):
                    self.assertEqual(actual, expected)

    def test_openrpc_inline_wire_baseline_for_shared_descriptors_and_errors(self):
        fixtures = contract.read("fixtures.json")
        for endpoint in ["host", "sandbox"]:
            for name, params, result, schema, expected_params, errors in [
                ("initialize", {"api":1}, fixtures[endpoint+"_initialize"], "InitializeResult",
                 [{"name":"api","required":True,"schema":{"$ref":"common.json#/definitions/InitializeParams/properties/api"}}],
                 [{"code":-32602,"message":"Invalid parameters"},{"code":-32002,"message":"Unsupported API version"},{"code":-32009,"message":"Stale or conflicting operation"}]),
                ("host.status", {"request":"fixture-job-1"}, fixtures["host_job_active"], "HostJob",
                 [{"name":"request","required":True,"schema":{"$ref":"common.json#/definitions/HostJobParams/properties/request"}}],
                 [{"code":-32602,"message":"Invalid parameters"},{"code":-32004,"message":"No visible host command"},{"code":-32009,"message":"Stale job, lifecycle conflict or finalized cancellation"}]),
            ]:
                with self.subTest(endpoint=endpoint,method=name):
                    row = next(row for row in contract.document(endpoint)["methods"] if row["name"] == name)
                    self.assertEqual(row["params"], expected_params)
                    self.assertEqual(row["result"], {"name":"result","schema":{"$ref":"common.json#/definitions/"+schema}})
                    self.assertEqual(row["errors"], errors)
                    self.assertTrue(contract.validate_params(endpoint,name,params))
                    self.assertTrue(contract.validate_reply(endpoint,name,dict(jsonrpc="2.0",id=1,result=result)))

    def test_openrpc_local_method_error_and_content_components_expand(self):
        from unittest.mock import patch
        original_read = contract.read
        inline = copy.deepcopy(contract.document("host"))
        expected = copy.deepcopy(next(row for row in inline["methods"] if row["name"] == "initialize"))
        raw = copy.deepcopy(inline)
        method = copy.deepcopy(expected)
        components = raw.setdefault("components", {})
        components.setdefault("contentDescriptors", {}).update(InitializeApi=method["params"][0], InitializeResult=method["result"])
        components.setdefault("errors", {}).update({"InitializeError"+str(i):error for i,error in enumerate(method["errors"])})
        method["params"] = [{"$ref":"#/components/contentDescriptors/InitializeApi"}]
        method["result"] = {"$ref":"#/components/contentDescriptors/InitializeResult"}
        method["errors"] = [{"$ref":"#/components/errors/InitializeError"+str(i)} for i in range(len(expected["errors"]))]
        raw.setdefault("x-methods", {})["Initialize"] = method
        index = next(i for i,row in enumerate(raw["methods"]) if row["name"] == "initialize")
        raw["methods"][index] = {"$ref":"#/x-methods/Initialize"}
        frozen = copy.deepcopy(raw)
        with patch.object(contract,"read",side_effect=lambda name:copy.deepcopy(raw) if name=="host.openrpc.json" else original_read(name)):
            expanded = contract.document.__wrapped__("host")
        self.assertEqual(expanded["methods"][index], expected)
        self.assertEqual(raw, frozen)
        with patch.object(contract,"document",return_value=expanded):
            self.assertTrue(contract.validate_params("host","initialize",{"api":1}))
            self.assertTrue(contract.validate_reply("host","initialize",dict(jsonrpc="2.0",id=1,result=original_read("fixtures.json")["host_initialize"])))

    def test_openrpc_common_components_expand_for_both_endpoints(self):
        from unittest.mock import patch
        original_read = contract.read
        documents = {endpoint:copy.deepcopy(contract.document(endpoint)) for endpoint in ["host","sandbox"]}
        expected = {endpoint:copy.deepcopy(next(row for row in doc["methods"] if row["name"]=="initialize")) for endpoint,doc in documents.items()}
        shared = copy.deepcopy(contract.common())
        method = copy.deepcopy(expected["host"])
        components = shared.setdefault("components", {})
        components.setdefault("contentDescriptors", {}).update(SharedApi=method["params"][0], SharedResult=method["result"])
        components.setdefault("errors", {}).update({"SharedError"+str(i):error for i,error in enumerate(method["errors"])})
        # References inside a shared method remain relative to common.json.
        method["params"] = [{"$ref":"#/components/contentDescriptors/SharedApi"}]
        method["result"] = {"$ref":"#/components/contentDescriptors/SharedResult"}
        method["errors"] = [{"$ref":"#/components/errors/SharedError"+str(i)} for i in range(len(method["errors"]))]
        shared.setdefault("x-methods", {})["SharedInitialize"] = method
        indices = {}
        for endpoint,doc in documents.items():
            indices[endpoint] = next(i for i,row in enumerate(doc["methods"]) if row["name"]=="initialize")
            doc["methods"][indices[endpoint]] = {"$ref":"common.json#/x-methods/SharedInitialize"}
        files = {endpoint+".openrpc.json":doc for endpoint,doc in documents.items()}
        files["common.json"] = shared
        frozen = copy.deepcopy(files)
        with patch.object(contract,"read",side_effect=lambda name:copy.deepcopy(files[name]) if name in files else original_read(name)), patch.object(contract,"common",return_value=shared):
            expanded = {endpoint:contract.document.__wrapped__(endpoint) for endpoint in documents}
        for endpoint,doc in expanded.items():
            self.assertEqual(doc["methods"][indices[endpoint]], expected[endpoint])
        self.assertEqual(files, frozen)
        with patch.object(contract,"document",side_effect=lambda endpoint:expanded[endpoint]):
            for endpoint in documents:
                self.assertTrue(contract.validate_params(endpoint,"initialize",{"api":1}))
                self.assertTrue(contract.validate_reply(endpoint,"initialize",dict(jsonrpc="2.0",id=1,result=original_read("fixtures.json")[endpoint+"_initialize"])))

    def test_offline_meta_schema_and_emacs_readers_are_current(self):
        contract.check()
        self.assertEqual(contract.ELISP_READERS.read_text(), contract.elisp_readers())

    def test_emacs_generation_and_freshness_check(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            readers = Path(directory) / "readers.el"
            readers.write_text(contract.elisp_readers())
            with patch.object(contract, "ELISP_READERS", readers), patch.object(contract, "check"), patch.object(sys, "argv", ["api_contract.py"]):
                contract.main()
                readers.write_text("stale readers")
                with self.assertRaisesRegex(SystemExit, "stale Emacs API readers"):
                    contract.main()
                with patch.object(sys, "argv", ["api_contract.py", "--write"]):
                    contract.main()
                self.assertEqual(readers.read_text(), contract.elisp_readers())

    def test_nested_references_resolve_from_actual_documents(self):
        documents={"common.json":copy.deepcopy(contract.common()),**{f"{endpoint}.openrpc.json":copy.deepcopy(contract.document(endpoint)) for endpoint in contract.ENDPOINTS}}
        contract.check_references(documents)
        entry=next(m for m in documents["sandbox.openrpc.json"]["methods"] if m["name"]=="flake.run")
        # Standalone fixture validation injects shared definitions, so it
        # cannot establish the reference scope of the OpenRPC document.
        entry["x-positional-schema"]={"type":"array","items":[{"$ref":"#/definitions/FlakeAppName"}]}
        self.assertTrue(contract.schema_validator(entry["x-positional-schema"]).is_valid(["default"]))
        with self.assertRaisesRegex(ValueError,"dangling reference.*sandbox.openrpc.json"):contract.check_references(documents)
        entry["x-positional-schema"]={"type":"array","items":[{"$ref":"common.json#/definitions/MissingAppName"}]}
        with self.assertRaisesRegex(ValueError,"dangling reference.*MissingAppName"):contract.check_references(documents)
        entry["x-positional-schema"]={"$ref":"common.json#/x-positional-schemas/FlakeRunParams"}
        documents["common.json"]["x-positional-schemas"]["FlakeRunParams"]["items"][0]={"$ref":"#/definitions/MissingAppName"}
        with self.assertRaisesRegex(ValueError,"dangling reference.*common.json"):contract.check_references(documents)

    def test_nested_positional_metadata_must_match_wire_and_generated_records(self):
        entry=copy.deepcopy(contract.method("sandbox","editor.open"))
        params=contract.resolve(entry["x-params-schema"]);wire=copy.deepcopy(contract.resolve(entry["x-wire-params-schema"]))
        contract.check_nested_params(entry,params,wire)
        wrong=copy.deepcopy(entry);wrong["x-nested-positional-params"][0]["order"]=["column","line","path"]
        with self.assertRaisesRegex(ValueError,"nested positional order"):contract.check_nested_params(wrong,params,wire)
        wrong=copy.deepcopy(entry);wrong["x-nested-positional-params"]*=2
        with self.assertRaisesRegex(ValueError,"invalid nested positional field"):contract.check_nested_params(wrong,params,wire)
        wire["properties"]["files"]["items"]["anyOf"].pop()
        with self.assertRaisesRegex(ValueError,"wire parameters differ"):contract.check_nested_params(entry,params,wire)
        entry=copy.deepcopy(contract.method("sandbox","permissions.request"))
        params=contract.resolve(entry["x-params-schema"]);wire=contract.resolve(entry["x-wire-params-schema"])
        contract.check_nested_params(entry,params,wire)
        entry["x-nested-positional-params"][0]["minimum"]=3
        with self.assertRaisesRegex(ValueError,"nested positional order"):contract.check_nested_params(entry,params,wire)

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

    def test_producer_records_require_every_field(self):
        fixtures = contract.read("fixtures.json")
        reply = lambda endpoint, name, result: contract.validate_reply(endpoint, name, dict(jsonrpc="2.0", id=1, result=result))
        for endpoint, name, value in [("sandbox", "sessions.status", fixtures["self_status"]), ("host", "sessions.get", fixtures["host_session"]), ("sandbox", "sessions.get", fixtures["sandbox_session"])]:
            self.assertTrue(reply(endpoint, name, value))
            for field in value:
                bad = dict(value); del bad[field]
                with self.subTest(method=name, missing=field), self.assertRaises(ValidationError):
                    reply(endpoint, name, bad)
        for endpoint in ("host", "sandbox"):
            self.assertTrue(reply(endpoint, "sessions.list", [fixtures[endpoint + "_session"]]))
        for field, value in [("state","finished"),("agent_state","future-activity"),("terminals",[{**fixtures["host_session"]["terminals"][0], "origin":"remote"}])]:
            result = copy.deepcopy(fixtures["host_session"]); result[field] = value
            with self.subTest(field=field), self.assertRaises(ValidationError):
                reply("host", "sessions.get", result)
        self.assertEqual(set(fixtures["sandbox_session"]),set(contract.common()["definitions"]["SandboxSession"]["properties"]))
        for hidden in ("configuration","identity","cwd","checkout","terminal","attach","detail","server_log"):
            self.assertNotIn(hidden, fixtures["sandbox_session"])
        self.assertEqual(fixtures["sandbox_session"]["worktree"],fixtures["host_session"]["worktree"])
        for endpoint,name,key in [("host","sessions.start","host_launch"),("sandbox","sessions.start","member_launch"),("sandbox","targets.list","run_targets")]:
            for result in [fixtures[key],fixtures["forward_compatibility"][key]]:
                self.assertTrue(reply(endpoint,name,result))
        for key,endpoint in [("host_launch","host"),("member_launch","sandbox")]:
            bad=copy.deepcopy(fixtures[key]);bad["state"]="running"
            with self.assertRaises(ValidationError): reply(endpoint,"sessions.start",bad)

    def test_history_list_accepts_a_new_record_without_variables(self):
        # Record::new starts with BTreeMap::new(); the public projection copies it.
        record = dict(version=3, id="new-record", configuration="shell", agent_name="fixture",
                      cwd=None, scope=None, dev_shell=None, docker_enabled=False,
                      packages=dict(approved=[], denied=[], auto_approve=[]), variables={},
                      sessions=[], skipped=[], worktree=None, created=1, updated=1,
                      stopped=None, running=False)
        contract.schema_validator({"$ref":"common.json#/definitions/HistoryRecord"}).validate(record)
        self.assertTrue(contract.validate_reply("host", "history.list", dict(jsonrpc="2.0", id=1,
            result=dict(directory="/history", records=[record], truncated=False))))

    def test_history_producer_results_are_precise(self):
        result_cases = contract.read("fixtures.json")["history_result_cases"]
        current = next(copy.deepcopy(case["result"]) for case in result_cases
                       if case["valid"] and case["method"] == "history.list" and case["result"]["records"])
        for field, value in (("version", 1), ("version", 2), ("version", 4),
                             ("running", None), ("running", 0), ("running", "false")):
            broken = copy.deepcopy(current)
            broken["records"][0][field] = value
            with self.subTest(current_field=field, value=value), self.assertRaises(ValidationError):
                contract.validate_reply("host", "history.list", dict(jsonrpc="2.0", id=1, result=broken))
        for broken in (dict(removed=[17], failed=[], dry_run=False),
                       dict(removed=[], failed=[dict(id=17, reason="bad")], dry_run=False),
                       dict(removed=[], failed=[dict(id="a", reason=None)], dry_run=False),
                       dict(removed=[], failed=[], dry_run=0)):
            with self.subTest(current_prune=broken), self.assertRaises(ValidationError):
                contract.validate_reply("host", "history.prune", dict(jsonrpc="2.0", id=1, result=broken))

    def test_notification_identities_and_directions(self):
        frame = dict(jsonrpc="2.0", method="state.changed", params=contract.read("fixtures.json")["state_notification_cases"][0]["params"])
        with self.assertRaises(ValidationError):
            contract.validate_notification("host", "server-to-client", dict(frame, id=1))
        self.assertFalse(contract.validate_notification("host", "client-to-server", frame))
        self.assertFalse(contract.validate_notification("sandbox", "server-to-client", frame))
        self.assertIsNone(contract.method("host", "state.changed"))
        good=dict(jsonrpc="2.0",method="exec.signal",params=dict(signal=15))
        self.assertFalse(contract.validate_notification("sandbox","client-to-server",good))
        self.assertFalse(contract.validate_notification("host","server-to-client",good))
        with self.assertRaises(ValidationError):contract.validate_notification("host","client-to-server",dict(good,id=1))
        self.assertIsNone(contract.method("host","exec.signal"))
        self.assertFalse(contract.validate_notification("supervisor", "client-to-server", dict(jsonrpc="2.0",method="Leases",params={})))
        self.assertFalse(contract.validate_notification("supervisor", "daemon-to-supervisor", dict(jsonrpc="2.0", method="Stopping", params={})))
        self.assertEqual(contract.document("attach")["methods"],[])

    def test_mailbox_byte_and_integer_bounds(self):
        fixtures=contract.read("fixtures.json")
        for endpoint in ("host","sandbox"):
            for case in fixtures["mailbox_byte_cases"]:
                params={**case["base"],case["field"]:case["unit"]*case["repeat"]+case["suffix"]}
                self.assertEqual(len(params[case["field"]].encode("utf-8")),case["bytes"])
                positional=[params[p["name"]] for p in contract.method(endpoint,case["method"])["params"]]
                for wire in (params,positional):
                    self.assertEqual(accepts(lambda: contract.validate_params(endpoint,case["method"],wire)),case["valid"],str(case))
            for case in fixtures["mailbox_mutation_results"]:
                self.assertTrue(contract.validate_reply(endpoint,case["method"],dict(jsonrpc="2.0",id=1,result=case["result"])))
            for value in [dict(message=None,claim_generation=1),dict(message=fixtures["mailbox_message"],claim_generation=None)]:
                with self.assertRaises(ValidationError):contract.validate_reply(endpoint,"inbox.next",dict(jsonrpc="2.0",id=1,result=value))
            with self.assertRaises(ValidationError):contract.validate_reply(endpoint,"inbox.complete",dict(jsonrpc="2.0",id=1,result=dict(message="m",state="queued")))
            for value in ({},[]):self.assertTrue(contract.validate_params(endpoint,"inbox.status",value))
            for value in ({"message":""},["m"]):self.assertTrue(contract.validate_params(endpoint,"messages.get",value))
        for case in fixtures["mailbox_u64_cases"]:
            for endpoint in ("host","sandbox"):
                valid=lambda name,result:accepts(lambda: contract.validate_reply(endpoint,name,dict(jsonrpc="2.0",id=1,result=result)))
                for name,base in [("messages.reply",dict(key="k",message="m",body="hello")),("inbox.complete",dict(key="k",message="m")),("inbox.requeue",dict(key="k",message="m",reason="later"))]:
                    params={**base,"claim_generation":case["value"]}
                    positional=[params[p["name"]] for p in contract.method(endpoint,name)["params"]]
                    for wire in (params,positional):
                        self.assertEqual(accepts(lambda: contract.validate_params(endpoint,name,wire)),case["claim"],str(case))
                for field in ("created_sequence","created_at","claim_generation"):
                    self.assertEqual(valid("messages.send",{**fixtures["mailbox_message"],field:case["value"]}),case["unsigned"],str(case))
                self.assertEqual(valid("inbox.requeue",dict(message="m",claim_generation=case["value"])),case["unsigned"],str(case))
                result=dict(message=None if case["value"] is None else {**fixtures["mailbox_message"],"state":"claimed","claim_generation":case["value"]},claim_generation=case["value"])
                self.assertEqual(valid("inbox.next",result),case["claim"] or case["value"] is None,str(case))

    def test_communications_parameters_and_payload_pairs(self):
        fixtures=contract.read("fixtures.json")
        entry=contract.method("host","communications.list")
        for value in fixtures["communications_parameter_cases"]:
            structural=contract.schema_validator(entry["x-positional-schema"] if isinstance(value,list) else entry["x-params-schema"]).is_valid(value)
            valid=structural and (not isinstance(value,dict) or 1<=value.get("limit",100)<=100)
            self.assertEqual(accepts(lambda: contract.validate_params("host","communications.list",value)),valid,str(value))
        self.assertTrue(contract.validate_params("host","communications.list",dict(after=0,limit=100)))
        page=dict(events=[fixtures["communications_event"]],cursor=1,latest=1,session=None)
        self.assertTrue(contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=page)))
        for case in fixtures["communication_payload_cases"]:
            event=dict(instance="i",sequence=1,timestamp=9,kind=case["kind"],actor="host",sessions=["s"],data=case["data"])
            page=dict(events=[event],cursor=1,session=None,latest=1)
            self.assertEqual(accepts(lambda: contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=page))),case["valid"],str(case))

    def test_raid_owner_requests_use_their_client_profile(self):
        for case in contract.read("fixtures.json")["raid_owner_requests"]:
            entry=contract.method("host",case["method"])
            contract.schema_validator(entry["x-client-param-profiles"][0]["x-params-schema"]).validate(case["params"])
            if case["canonical"]:self.assertTrue(contract.validate_params("host",case["method"],case["params"]))
            elif case["preview"].get("raid") is not None:
                with self.assertRaises(ValidationError):contract.validate_params("host",case["method"],case["params"])

    def test_acknowledgements_require_acceptance(self):
        reply=lambda endpoint,name,result:contract.validate_reply(endpoint,name,dict(jsonrpc="2.0",id=1,result=result))
        self.assertTrue(reply("sandbox","raids.leave",dict(accepted=True,dissolved=True,paused=False)))
        for name,result in [("raids.destroy",dict(accepted=True,stopped=[])),("raids.kick",dict(accepted=True,dissolved=False,paused=True))]:
            self.assertTrue(reply("host",name,result))
            with self.assertRaises(ValidationError):reply("host",name,{**result,"accepted":False})
        for endpoint in ("host","sandbox"):
            self.assertTrue(reply(endpoint,"host.cancel",dict(accepted=True,request="fixture-job-1")))

    def test_permission_records_are_precise(self):
        records=contract.read("fixtures.json")["permission_records"]
        reply=lambda name,result:contract.validate_reply("host",name,dict(jsonrpc="2.0",id=1,result=result))
        self.assertTrue(reply("permissions.list",records))
        for record in records:
            for key in ("approved","preview","message"):
                broken=copy.deepcopy(record);del broken[key]
                with self.assertRaises(ValidationError):reply("permissions.get",broken)
            if record["kind"] != "devshell":
                with self.assertRaises(ValidationError):reply("permissions.get",{**record,"preview":[]})
        # The private refresh payload stays opaque.
        self.assertTrue(reply("permissions.get",{**records[3],"preview":17}))

    def test_devshell_preview_requires_every_field(self):
        fixtures=contract.read("fixtures.json")
        preview=fixtures["devshell_preview"]
        validate=lambda value:contract.validate_reply("sandbox","devshell.diff",dict(jsonrpc="2.0",id=1,result=value))
        for value in (preview,fixtures["devshell_preview_unchanged"],fixtures["devshell_preview_produced"],{**preview,"extension":True}):
            self.assertTrue(validate(value))
        for key in preview:
            broken=copy.deepcopy(preview);del broken[key]
            with self.assertRaises(ValidationError,msg=key):validate(broken)
        for path,value in [(('attention',0,'kind'),'future'),(('attention',1,'source'),None),(('files',0,'added'),-1),(('inputs',0,'new','date'),7),(('packages',0,'old'),[1]),(('env',0,'long'),1),(('incoming',0,'action'),'future')]:
            broken=copy.deepcopy(preview);target=broken
            for key in path[:-1]:target=target[key]
            target[path[-1]]=value
            with self.assertRaises(ValidationError,msg=str(path)):validate(broken)
        with self.assertRaises(ValidationError):validate({**preview,'incoming':preview['incoming']*501})
        for nonobject in (None,[],1,"preview"):
            with self.assertRaises(ValidationError):validate(nonobject)


if __name__ == "__main__":
    unittest.main()
