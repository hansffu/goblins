"""Schema and real-boundary checks for generated contracts, including legacy parameter forms."""
import copy
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

from jsonschema import ValidationError
import api_contract as contract
from daemon_support import Daemon, RPC, app, frame, receive


class ContractTests(unittest.TestCase):
    def test_c17d2_lifecycle_instances_bindings_and_remaining_inventory(self):
        fixtures = contract.read("fixtures.json")
        for case in fixtures["c17d2_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        doc = contract.document("supervisor")
        for binding in fixtures["c17d2_lifecycle_bindings"]:
            row = next(row for row in doc["x-notifications"] if row["name"] == binding["method"])
            self.assertEqual(row["x-rust-name"], binding["marker"])
            self.assertEqual(row["direction"], binding["direction"])
            self.assertEqual(row["x-params-schema"], {"$ref":"common.json#/definitions/" + binding["schema"]})
            self.assertEqual([field["name"] for field in row["params"]], binding["fields"])
        pending = [name for group in doc["x-pending-notifications"].values() for name in group]
        completed = {binding["method"] for binding in fixtures["c17d2_lifecycle_bindings"]}
        self.assertEqual(len(completed), 10)
        self.assertEqual(len(pending), 8)
        self.assertEqual(doc["x-pending-notifications"]["C17d-lifecycle-host-jobs"], ["Command.ScopeMembership", "Event.Started"])
        self.assertEqual(set(doc["x-pending-notifications"]), {"C17d-lifecycle-host-jobs", "C17e-relay-logging"})
        self.assertFalse(completed & set(pending))
        self.assertTrue(completed <= {row["name"] for row in doc["x-notifications"]})

    def test_c17d2_lifecycle_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def definition(s, name): return s["definitions"]["Supervisor" + name]
        for root,key in [("CommandExportParams","member"),("EventScopeFailedParams","name"),("EventScopeNeededParams","name"),("EventModeParams","server"),("EventFailedParams","error")]:
            changed(root + " native scalar", lambda s,root=root,key=key: definition(s,root)["properties"][key].update(type="integer"))
        changed("Stopped nullable i32 reference", lambda s: definition(s,"EventStoppedParams")["properties"]["exit_code"].update({"$ref":"#/definitions/SupervisorOptionalSequence"}))
        for root in ["CommandExportParams","EventScopeFailedParams","EventScopeNeededParams","EventModeParams","EventStoppedParams","EventFailedParams"]:
            changed(root + " required field", lambda s,root=root: definition(s,root)["required"].clear())
            changed(root + " root closed", lambda s,root=root: definition(s,root).update(additionalProperties=True))
        changed("Stopped serde original name", lambda s: s["x-rust-serde-records"]["SupervisorEventStoppedParams"].update(name="Wrong"))
        changed("Stopped serde field order", lambda s: s["x-rust-serde-records"]["SupervisorEventStoppedParams"]["order"].clear())
        changed("Stopped required nullable slot", lambda s: s["x-rust-serde-records"]["SupervisorEventStoppedParams"]["required-nullable"].remove("exit_code"))
        changed("Stopped no default field", lambda s: s["x-rust-serde-records"]["SupervisorEventStoppedParams"].update({"emit-defaults":["exit_code"]}))
        changed("Stopped no field aliases", lambda s: s["x-rust-serde-records"]["SupervisorEventStoppedParams"].update({"aliases":{"exit_code":["status"]}}))
        changed("Empty params closed", lambda s: definition(s,"EmptyParams").update(additionalProperties=True))
        changed("Empty params object", lambda s: definition(s,"EmptyParams").update(type="array"))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17d2_lifecycle_notification_markers_directions_and_payloads_reject_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.document
        doc = original("supervisor")
        for binding in contract.read("fixtures.json")["c17d2_lifecycle_bindings"]:
            for mutation in ["marker", "direction", "payload"]:
                changed = copy.deepcopy(doc)
                row = next(row for row in changed["x-notifications"] if row["name"] == binding["method"])
                if mutation == "marker": row["x-rust-name"] = "Wrong" + binding["marker"]
                elif mutation == "direction": row["direction"] = "supervisor-to-daemon" if binding["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
                else: row["x-params-schema"] = {"$ref":"common.json#/definitions/SupervisorDescriptor"}
                with self.subTest(method=binding["method"], mutation=mutation), patch.object(contract,"document",side_effect=lambda endpoint: changed if endpoint == "supervisor" else original(endpoint)):
                    with self.assertRaises(ValueError):
                        contract.check()

    def test_c17d1_host_job_instances_notifications_and_remaining_inventory(self):
        fixtures = contract.read("fixtures.json")
        for name, cases in fixtures["c17d1_host_job_schema_cases"].items():
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            for case in cases:
                with self.subTest(schema=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17d1_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        doc = contract.document("supervisor")
        pending = [name for group in doc["x-pending-notifications"].values() for name in group]
        completed = {case["frame"]["method"] for case in fixtures["c17d1_notification_cases"]}
        self.assertEqual(len(completed), 6)
        completed |= {case["frame"]["method"] for case in fixtures["c17d2_notification_cases"]}
        self.assertEqual(len(pending), 8)
        self.assertEqual(set(doc["x-pending-notifications"]), {"C17d-lifecycle-host-jobs", "C17e-relay-logging"})
        self.assertFalse(completed & set(pending))
        self.assertTrue(completed <= {row["name"] for row in doc["x-notifications"]})

    def test_c17d1_host_outcome_public_description_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        self.assertEqual(original["definitions"]["SupervisorHostOutcome"]["description"], "What a host job's supervisor records in its `status.json`.")
        for mutation in ["removed", "changed"]:
            schema = copy.deepcopy(original)
            if mutation == "removed":
                del schema["definitions"]["SupervisorHostOutcome"]["description"]
            else:
                schema["definitions"]["SupervisorHostOutcome"]["description"] = "Changed public host outcome explanation."
            with self.subTest(description=mutation), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17d1_host_job_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def definition(s, name): return s["definitions"]["Supervisor" + name]
        def field(s, name, key): return definition(s,name)["properties"][key]
        for name in ["HostOutcome", "EventHostJobFinishedParams"]:
            changed(name + " registry", lambda s,name=name: s["x-rust-explicit-records"].remove("Supervisor"+name))
            changed(name + " original name", lambda s,name=name: definition(s,name).update({"x-rust-name":"Wrong"}))
            changed(name + " no Clone", lambda s,name=name: definition(s,name)["x-rust-derives"].insert(0,"Clone"))
            changed(name + " Deserialize", lambda s,name=name: definition(s,name)["x-rust-derives"].remove("Deserialize"))
            changed(name + " field order", lambda s,name=name: definition(s,name)["x-rust-field-order"].reverse())
        changed("Outcome Default", lambda s: definition(s,"HostOutcome")["x-rust-derives"].remove("Default"))
        changed("Outcome original module", lambda s: definition(s,"HostOutcome").update({"x-rust-module":"host_metadata"}))
        changed("Outcome open fields", lambda s: definition(s,"HostOutcome").update(additionalProperties=False))
        for key in ["exit_code","signal","spawn_error","cancelled","timed_out","truncated"]:
            changed("Outcome " + key + " native type", lambda s,key=key: field(s,"HostOutcome",key).update({"x-rust-type":"u64"}))
            changed("Outcome " + key + " native name", lambda s,key=key: field(s,"HostOutcome",key).update({"x-rust-field-name":"other"}))
            changed("Outcome " + key + " no serde default", lambda s,key=key: field(s,"HostOutcome",key).update({"x-rust-default":True}))
            changed("Outcome " + key + " no skip", lambda s,key=key: field(s,"HostOutcome",key).update({"x-rust-skip-serializing-if":"Option::is_none"}))
        for key in ["exit_code","signal","spawn_error"]:
            changed("Outcome " + key + " optional missing", lambda s,key=key: definition(s,"HostOutcome")["required"].append(key))
            changed("Outcome " + key + " nullable reference", lambda s,key=key: field(s,"HostOutcome",key).update({"$ref":"#/definitions/SupervisorSequence"}))
        for key in ["cancelled","timed_out","truncated"]:
            changed("Outcome " + key + " required bool", lambda s,key=key: definition(s,"HostOutcome")["required"].remove(key))
            changed("Outcome " + key + " wire bool", lambda s,key=key: field(s,"HostOutcome",key).update(type="integer"))
        changed("Finished outcome native Result", lambda s: field(s,"EventHostJobFinishedParams","outcome").update({"x-rust-type":"String"}))
        changed("Finished outcome native name", lambda s: field(s,"EventHostJobFinishedParams","outcome").update({"x-rust-field-name":"other"}))
        changed("Finished request native String", lambda s: field(s,"EventHostJobFinishedParams","request").update({"x-rust-type":"u64"}))
        for name,wrong in [("HostPreparedOutcome","OptionalPath"),("HostFinishedOutcome","ApprovalReply")]:
            changed(name + " Ok reference", lambda s,name=name,wrong=wrong: definition(s,name)["oneOf"][0]["properties"]["Ok"].update({"$ref":"#/definitions/Supervisor"+wrong}))
            changed(name + " Err String", lambda s,name=name: definition(s,name)["oneOf"][1]["properties"]["Err"].update(type="integer"))
            changed(name + " external Result closed", lambda s,name=name: definition(s,name)["oneOf"][0].update(additionalProperties=True))
            changed(name + " Result required branch", lambda s,name=name: definition(s,name)["oneOf"][1]["required"].clear())
        changed("Run timeout required nullable metadata", lambda s: s["x-rust-serde-records"]["SupervisorCommandRunHostJobParams"]["required-nullable"].remove("timeout"))
        changed("Run extraction order", lambda s: s["x-rust-serde-records"]["SupervisorCommandRunHostJobParams"]["order"].reverse())
        changed("Run original serde name", lambda s: s["x-rust-serde-records"]["SupervisorCommandRunHostJobParams"].update(name="Wrong"))
        changed("Arguments array", lambda s: definition(s,"Arguments").update(type="object"))
        changed("Arguments String elements", lambda s: definition(s,"Arguments")["items"].update(type="integer"))
        changed("Environment pair minimum", lambda s: definition(s,"Environment")["items"].update(minItems=1))
        changed("Environment pair maximum", lambda s: definition(s,"Environment")["items"].update(maxItems=3))
        changed("Environment pair String element", lambda s: definition(s,"Environment")["items"]["items"][1].update(type="integer"))
        changed("Timeout u64 reference", lambda s: definition(s,"OptionalSequence")["anyOf"][0].update({"$ref":"#/definitions/SupervisorGeneration"}))
        changed("Timeout nullable", lambda s: definition(s,"OptionalSequence")["anyOf"].pop())
        for root,key,wrong in [("CommandPrepareHostJobParams","cwd","OptionalPath"),("CommandRunHostJobParams","argv","OptionalArguments"),("CommandRunHostJobParams","env","Arguments"),("CommandRunHostJobParams","timeout","Sequence"),("EventHostJobPreparedParams","result","HostFinishedOutcome"),("EventHostJobFinishedParams","outcome","HostPreparedOutcome")]:
            changed(root + "." + key + " root reference", lambda s,root=root,key=key,wrong=wrong: field(s,root,key).update({"$ref":"#/definitions/Supervisor"+wrong}))
        for root in ["CommandPrepareHostJobParams","CommandRunHostJobParams","CommandCancelHostJobParams","EventHostJobPreparedParams","EventHostJobFailedParams","EventHostJobFinishedParams"]:
            changed(root + " request String", lambda s,root=root: field(s,root,"request").update(type="integer"))
            changed(root + " required slots", lambda s,root=root: definition(s,root)["required"].pop())
            changed(root + " root closed", lambda s,root=root: definition(s,root).update(additionalProperties=True))
        changed("Failed error String", lambda s: field(s,"EventHostJobFailedParams","error").update(type="integer"))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17c2_devshell_instances_opaque_strings_and_notifications(self):
        fixtures = contract.read("fixtures.json")
        for name, cases in fixtures["c17c2_devshell_schema_cases"].items():
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            for case in cases:
                with self.subTest(schema=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        # Validate explicit inner JSON values separately. Outer String validation
        # deliberately accepts malformed JSON and does not invoke these schemas.
        for name, cases in fixtures["c17c2_inner_json_cases"].items():
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name + "/x-json-content-schema"})
            for case in cases:
                with self.subTest(inner_schema=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17c2_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        doc = contract.document("supervisor")
        pending = [name for group in doc["x-pending-notifications"].values() for name in group]
        completed = {case["frame"]["method"] for case in fixtures["c17c2_notification_cases"]}
        self.assertEqual(len(completed), 6)
        completed |= {case["frame"]["method"] for case in fixtures["c17d1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17d2_notification_cases"]}
        self.assertEqual(len(pending), 8)
        self.assertEqual(set(doc["x-pending-notifications"]), {"C17d-lifecycle-host-jobs", "C17e-relay-logging"})
        self.assertFalse(completed & set(pending))
        self.assertTrue(completed <= {row["name"] for row in doc["x-notifications"]})

    def test_c17c2_devshell_public_descriptions_reject_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        for name in ["SupervisorDevShellChanges", "SupervisorDevShellSummary"]:
            self.assertTrue(original["definitions"][name]["description"])
            for mutation in ["removed", "changed"]:
                schema = copy.deepcopy(original)
                if mutation == "removed":
                    del schema["definitions"][name]["description"]
                else:
                    schema["definitions"][name]["description"] = "Changed public devshell contract explanation."
                with self.subTest(schema=name, description=mutation), patch.object(contract,"common",return_value=schema):
                    with self.assertRaises(ValueError):
                        contract.check()

    def test_c17c2_devshell_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def definition(s, name): return s["definitions"]["Supervisor" + name]
        def field(s, name, key): return definition(s,name)["properties"][key]
        def inner(s, name): return definition(s,name)["x-json-content-schema"]
        for name in ["DevShellChanges", "DevShellSummary"]:
            changed(name + " registry", lambda s,name=name: s["x-rust-explicit-records"].remove("Supervisor"+name))
            changed(name + " original name", lambda s,name=name: definition(s,name).update({"x-rust-name":"Wrong"}))
            changed(name + " module", lambda s,name=name: definition(s,name).update({"x-rust-module":"wrong"}))
            changed(name + " traits", lambda s,name=name: definition(s,name)["x-rust-derives"].remove("Clone"))
            changed(name + " order", lambda s,name=name: definition(s,name)["x-rust-field-order"].reverse())
            changed(name + " open fields", lambda s,name=name: definition(s,name).update(additionalProperties=False))
        changed("Changes constructor Default", lambda s: definition(s,"DevShellChanges")["x-rust-derives"].remove("Default"))
        changed("Changes equality", lambda s: definition(s,"DevShellChanges")["x-rust-derives"].remove("PartialEq"))
        changed("Summary has no constructor Default", lambda s: definition(s,"DevShellSummary")["x-rust-derives"].insert(0,"Default"))
        changed("Changes lock required", lambda s: definition(s,"DevShellChanges")["required"].remove("lock"))
        changed("Changes update optional", lambda s: definition(s,"DevShellChanges")["required"].append("update"))
        changed("Changes update nullable", lambda s: field(s,"DevShellChanges","update")["anyOf"].pop())
        changed("Changes update array", lambda s: field(s,"DevShellChanges","update")["anyOf"][0].update(type="object"))
        changed("Changes update String elements", lambda s: field(s,"DevShellChanges","update")["anyOf"][0]["items"].update(type="integer"))
        changed("Changes lock bool", lambda s: field(s,"DevShellChanges","lock").update(type="integer"))
        for name,keys in [("DevShellChanges",["update","lock"]),("DevShellSummary",["identity","generation","lock"])]:
            for key in keys:
                changed(name + "." + key + " native type", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-type":"u64"}))
                changed(name + "." + key + " native name", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-field-name":"other"}))
                changed(name + "." + key + " no serde default", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-default":True}))
                changed(name + "." + key + " emits nulls", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-skip-serializing-if":"Option::is_none"}))
        for key in ["identity","generation","lock"]:
            changed("Summary required " + key, lambda s,key=key: definition(s,"DevShellSummary")["required"].remove(key))
        changed("Summary identity String", lambda s: field(s,"DevShellSummary","identity").update(type="integer"))
        changed("Summary generation ref", lambda s: field(s,"DevShellSummary","generation").update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("Summary lock arbitrary Value", lambda s: field(s,"DevShellSummary","lock").update(type="object"))
        changed("Generation minimum", lambda s: definition(s,"Generation").update(minimum=1))
        changed("Generation maximum", lambda s: definition(s,"Generation").update(maximum=2**64-1))
        changed("Generation integer representation", lambda s: definition(s,"Generation").pop("x-json-integer"))
        for name in ["RefreshResult","CallResult","DevShellSummaryText"]:
            changed(name + " opaque outer String", lambda s,name=name: definition(s,name).update(type="object"))
            changed(name + " inner schema removed", lambda s,name=name: definition(s,name).pop("x-json-content-schema"))
        for name in ["RefreshResult","CallResult"]:
            changed(name + " Err String", lambda s,name=name: inner(s,name)["oneOf"][1]["properties"]["Err"].update(type="integer"))
            changed(name + " external Result closed", lambda s,name=name: inner(s,name)["oneOf"][0].update(additionalProperties=True))
            changed(name + " Result required", lambda s,name=name: inner(s,name)["oneOf"][1]["required"].clear())
        changed("Refresh tuple minimum", lambda s: inner(s,"RefreshResult")["oneOf"][0]["properties"]["Ok"].update(minItems=1))
        changed("Refresh tuple maximum", lambda s: inner(s,"RefreshResult")["oneOf"][0]["properties"]["Ok"].update(maxItems=3))
        changed("Refresh tuple arbitrary Value", lambda s: inner(s,"RefreshResult")["oneOf"][0]["properties"]["Ok"]["items"][0].update(type="object"))
        changed("Refresh tuple String second", lambda s: inner(s,"RefreshResult")["oneOf"][0]["properties"]["Ok"]["items"][1].update(type="integer"))
        changed("Call arbitrary Value", lambda s: inner(s,"CallResult")["oneOf"][0]["properties"]["Ok"].update(type="object"))
        changed("Summary text content reference", lambda s: inner(s,"DevShellSummaryText").update({"$ref":"#/definitions/SupervisorApprovalReply"}))
        for root,key,wrong in [("CommandRefreshParams","approval","Generation"),("CommandRefreshParams","changes","DevShellSummary"),("CommandRunParams","serial","Generation"),("CommandDiffParams","serial","Generation"),("CommandDiffParams","changes","DevShellSummary"),("EventRefreshParams","approval","Generation"),("EventRefreshParams","result","CallResult"),("EventCallParams","serial","Generation"),("EventCallParams","result","RefreshResult"),("EventDevShellParams","summary","DevShellSummary")]:
            changed(root + "." + key + " reference", lambda s,root=root,key=key,wrong=wrong: field(s,root,key).update({"$ref":"#/definitions/Supervisor"+wrong}))
        changed("Run app String", lambda s: field(s,"CommandRunParams","app").update(type="integer"))
        for root in ["CommandRefreshParams","CommandRunParams","CommandDiffParams","EventRefreshParams","EventCallParams","EventDevShellParams"]:
            changed(root + " required slot", lambda s,root=root: definition(s,root)["required"].pop())
            changed(root + " root closed", lambda s,root=root: definition(s,root).update(additionalProperties=True))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17c1_approval_instances_notifications_and_remaining_inventory(self):
        fixtures = contract.read("fixtures.json")
        for name, cases in fixtures["c17c1_approval_schema_cases"].items():
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            for case in cases:
                with self.subTest(schema=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17c1_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        doc = contract.document("supervisor")
        pending = [name for group in doc["x-pending-notifications"].values() for name in group]
        completed = {case["frame"]["method"] for case in fixtures["c17c1_notification_cases"]}
        self.assertEqual(len(completed), 6)
        completed |= {case["frame"]["method"] for case in fixtures["c17c2_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17d1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17d2_notification_cases"]}
        self.assertEqual(len(pending), 8)
        self.assertEqual(set(doc["x-pending-notifications"]), {"C17d-lifecycle-host-jobs", "C17e-relay-logging"})
        self.assertFalse(completed & set(pending))
        self.assertTrue(completed <= {row["name"] for row in doc["x-notifications"]})

    def test_c17c1_approval_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def definition(s, name): return s["definitions"]["Supervisor" + name]
        def field(s, name, key): return definition(s,name)["properties"][key]
        for name in ["ApprovalRequest", "ApprovalReply", "Preview", "EventGrantedParams"]:
            changed(name + " registry", lambda s,name=name: s["x-rust-explicit-records"].remove("Supervisor"+name))
            changed(name + " original name", lambda s,name=name: definition(s,name).update({"x-rust-name":"Wrong"}))
            changed(name + " traits", lambda s,name=name: definition(s,name)["x-rust-derives"].remove("Deserialize"))
            changed(name + " order", lambda s,name=name: definition(s,name)["x-rust-field-order"].reverse())
            changed(name + " closedness", lambda s,name=name: definition(s,name).update(additionalProperties=True))
        for name in ["ApprovalRequest", "ApprovalReply"]:
            changed(name + " module", lambda s,name=name: definition(s,name).update({"x-rust-module":"wrong"}))
        changed("Reply has no Clone", lambda s: definition(s,"ApprovalReply")["x-rust-derives"].insert(0,"Clone"))
        changed("Granted has no Clone", lambda s: definition(s,"EventGrantedParams")["x-rust-derives"].insert(0,"Clone"))
        changed("Request retains Clone", lambda s: definition(s,"ApprovalRequest")["x-rust-derives"].remove("Clone"))
        changed("Request kind default function", lambda s: field(s,"ApprovalRequest","kind").update({"x-rust-default":"crate::wrong_kind"}))
        changed("Request kind function not boolean", lambda s: field(s,"ApprovalRequest","kind").update({"x-rust-default":True}))
        changed("Request kind default removed", lambda s: field(s,"ApprovalRequest","kind").pop("x-rust-default"))
        changed("Request canonical package default", lambda s: field(s,"ApprovalRequest","kind").update(default="future"))
        changed("Request kind optional", lambda s: definition(s,"ApprovalRequest")["required"].append("kind"))
        for name, key in [("ApprovalRequest","id"),("ApprovalRequest","package"),("ApprovalRequest","reason"),("ApprovalReply","status"),("Preview","in_store"),("Preview","build_required")]:
            changed(name + "." + key + " required", lambda s,name=name,key=key: definition(s,name)["required"].remove(key))
            changed(name + "." + key + " no default", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-default":True}))
        changed("Reply message skip predicate", lambda s: field(s,"ApprovalReply","message").update({"x-rust-skip-serializing-if":"Option::is_some"}))
        changed("Reply message skip removed", lambda s: field(s,"ApprovalReply","message").pop("x-rust-skip-serializing-if"))
        changed("Reply id emits null", lambda s: field(s,"ApprovalReply","id").update({"x-rust-skip-serializing-if":"Option::is_none"}))
        for name,key in [("ApprovalReply","id"),("ApprovalReply","message"),("Preview","output_path"),("Preview","download")]:
            changed(name + "." + key + " optional missing", lambda s,name=name,key=key: definition(s,name)["required"].append(key))
            changed(name + "." + key + " no serde default", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-default":True}))
        for name,key in [("ApprovalRequest","kind"),("ApprovalReply","message"),("Preview","output_path"),("Preview","in_store"),("Preview","download"),("EventGrantedParams","reply")]:
            changed(name + "." + key + " native type", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-type":"u64"}))
            changed(name + "." + key + " native field", lambda s,name=name,key=key: field(s,name,key).update({"x-rust-field-name":"other"}))
        changed("Preview path reference", lambda s: field(s,"Preview","output_path")["anyOf"][0].update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("Preview path nullable", lambda s: field(s,"Preview","output_path")["anyOf"].pop())
        changed("Preview download reference", lambda s: field(s,"Preview","download").update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("Preview bool wire type", lambda s: field(s,"Preview","in_store").update(type="integer"))
        changed("Preview keeps private path", lambda s: field(s,"Preview","output_path").update({"x-rust-skip-serializing-if":"Option::is_none"}))
        changed("Preview Ok reference", lambda s: definition(s,"PreviewOutcome")["oneOf"][0]["properties"]["Ok"].update({"$ref":"#/definitions/SupervisorApprovalReply"}))
        changed("Preview Err String", lambda s: definition(s,"PreviewOutcome")["oneOf"][1]["properties"]["Err"].update(type="integer"))
        changed("Preview Result external envelope", lambda s: definition(s,"PreviewOutcome")["oneOf"][0].update(additionalProperties=True))
        changed("Preview Result branch required", lambda s: definition(s,"PreviewOutcome")["oneOf"][1]["required"].clear())
        changed("Decide required nullable metadata", lambda s: s["x-rust-serde-records"]["SupervisorCommandDecideParams"]["required-nullable"].remove("output"))
        changed("Decide extraction order", lambda s: s["x-rust-serde-records"]["SupervisorCommandDecideParams"]["order"].reverse())
        for key in ["detail","output"]:
            changed("Granted required " + key, lambda s,key=key: definition(s,"EventGrantedParams")["required"].remove(key))
            changed("Granted no default " + key, lambda s,key=key: field(s,"EventGrantedParams",key).update({"x-rust-default":True}))
        for root,key,wrong in [("CommandCancelParams","id","Dimension"),("CommandPreviewParams","approval","Dimension"),("CommandDecideParams","request","ApprovalReply"),("CommandDecideParams","output","OptionalString"),("EventPreviewParams","result","ApprovalReply"),("EventGrantedParams","reply","ApprovalRequest"),("EventGrantedParams","output","OptionalString"),("EventGrantedParams","detail","Sequence")]:
            changed(root + "." + key + " root reference", lambda s,root=root,key=key,wrong=wrong: field(s,root,key).update({"$ref":"#/definitions/Supervisor"+wrong}))
        for root,key in [("CommandPreviewParams","package"),("CommandDecideParams","approved"),("EventProgressParams","request"),("EventProgressParams","state")]:
            changed(root + "." + key + " wire type", lambda s,root=root,key=key: field(s,root,key).update(type="integer"))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17b2_terminal_public_descriptions_reject_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        for name in ["SupervisorTerminalId", "SupervisorTerminalStatus", "SupervisorTerminalCall"]:
            self.assertTrue(original["definitions"][name]["description"])
            for mutation in ["removed", "changed"]:
                schema = copy.deepcopy(original)
                definition = schema["definitions"][name]
                if mutation == "removed":
                    del definition["description"]
                else:
                    definition["description"] = "Changed public terminal contract explanation."
                with self.subTest(schema=name, description=mutation), patch.object(contract, "common", return_value=schema):
                    with self.assertRaises(ValueError):
                        contract.check()

    def test_c17b2_terminal_instances_notifications_and_opaque_result(self):
        fixtures = contract.read("fixtures.json")
        for name, cases in fixtures["c17b2_terminal_schema_cases"].items():
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            for case in cases:
                with self.subTest(schema=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17b2_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        # This validates explicit inner JSON values, never decodes the outer String.
        inner = contract.schema_validator(contract.common()["definitions"]["SupervisorTerminalResult"]["x-json-content-schema"])
        for value in [{"Ok":None},{"Ok":{"fd":0}},{"Ok":[1,True]},{"Err":"failed"}]:
            self.assertTrue(inner.is_valid(value))
        for value in [None,{"Err":0},{"Ok":0,"Err":"failed"},{"Other":0}]:
            self.assertFalse(inner.is_valid(value))
        doc = contract.document("supervisor")
        pending = [name for group in doc["x-pending-notifications"].values() for name in group]
        self.assertEqual(len(pending), 8)
        completed = {case["frame"]["method"] for case in fixtures["c17b2_notification_cases"]}
        self.assertEqual(len(completed), 7)
        completed |= {case["frame"]["method"] for case in fixtures["c17c1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17c2_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17d1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in fixtures["c17d2_notification_cases"]}
        self.assertFalse(completed & set(pending))
        self.assertTrue(completed <= {row["name"] for row in doc["x-notifications"]})

    def test_c17b2_terminal_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def definition(s, name): return s["definitions"]["Supervisor" + name]
        def call(s, index, name): return definition(s,"TerminalCall")["oneOf"][index]["properties"][name]
        for name in ["TerminalId","TerminalSource","TerminalCondition","TerminalCall"]:
            changed(name + " registry", lambda s,name=name: s["x-rust-external-enums"].remove("Supervisor"+name))
            changed(name + " original name", lambda s,name=name: definition(s,name).update({"x-rust-name":"Wrong"}))
            changed(name + " original traits", lambda s,name=name: definition(s,name)["x-rust-derives"].remove("Eq"))
        changed("Status registry", lambda s: s["x-rust-explicit-records"].remove("SupervisorTerminalStatus"))
        for name in ["TerminalSource","TerminalCondition","TerminalCall"]:
            changed(name + " module", lambda s,name=name: definition(s,name).update({"x-rust-module":"wrong"}))
        changed("Id ordering", lambda s: definition(s,"TerminalId")["x-rust-derives"].remove("Ord"))
        changed("Id newtype native", lambda s: definition(s,"TerminalId")["x-rust-variants"][1].update({"x-rust-type":"usize"}))
        changed("Id newtype pointer", lambda s: definition(s,"TerminalId")["x-rust-variants"][1].update({"schema-ref":"#/definitions/SupervisorSequence"}))
        changed("Id newtype kind", lambda s: definition(s,"TerminalId")["x-rust-variants"][1].update(kind="unit"))
        changed("Condition newtype type", lambda s: definition(s,"TerminalCondition")["x-rust-variants"][0].update({"x-rust-type":"u64"}))
        changed("Condition explanation", lambda s: definition(s,"TerminalCondition")["x-rust-variants"][1].update(description="Wrong units"))
        changed("Status Default", lambda s: definition(s,"TerminalStatus")["x-rust-derives"].remove("Default"))
        changed("Status name", lambda s: definition(s,"TerminalStatus").update({"x-rust-name":"Wrong"}))
        changed("Status order", lambda s: definition(s,"TerminalStatus")["x-rust-field-order"].reverse())
        changed("Status missing required bool", lambda s: definition(s,"TerminalStatus")["required"].remove("connected"))
        changed("Status open", lambda s: definition(s,"TerminalStatus").update(additionalProperties=False))
        changed("Status wire bool", lambda s: definition(s,"TerminalStatus")["properties"]["eof"].update(type="integer"))
        changed("Status native bool", lambda s: definition(s,"TerminalStatus")["properties"]["eof"].update({"x-rust-type":"u64"}))
        changed("Status field name", lambda s: definition(s,"TerminalStatus")["properties"]["eof"].update({"x-rust-field-name":"other"}))
        changed("Status constructor default is not serde default", lambda s: definition(s,"TerminalStatus")["properties"]["eof"].update({"x-rust-default":True}))
        for index,name in enumerate(["Read","Send","Wait"]):
            changed(name + " payload pointer", lambda s,index=index: definition(s,"TerminalCall")["x-rust-variants"][index].update({"schema-ref":"#/definitions/SupervisorSequence"}))
            changed(name + " field order", lambda s,index=index,name=name: call(s,index,name)["x-rust-field-order"].reverse())
            changed(name + " open payload", lambda s,index=index,name=name: call(s,index,name).update(additionalProperties=False))
            changed(name + " required", lambda s,index=index,name=name: call(s,index,name)["required"].pop())
        for index,name,field in [(0,"Read","lines"),(1,"Send","text"),(2,"Wait","lines")]:
            changed(name + " no serde default", lambda s,index=index,name=name,field=field: call(s,index,name)["properties"][field].update({"x-rust-default":True}))
        changed("Read source native", lambda s: call(s,0,"Read")["properties"]["source"].update({"x-rust-type":"Condition"}))
        changed("Wait timeout unit", lambda s: call(s,2,"Wait")["properties"]["timeout"].update(description="Seconds."))
        changed("Wait timeout native", lambda s: call(s,2,"Wait")["properties"]["timeout"].update({"x-rust-type":"usize"}))
        changed("Read optional index", lambda s: call(s,0,"Read")["properties"]["lines"]["anyOf"][0].update({"$ref":"#/definitions/SupervisorDimension"}))
        for name in ["Dimension","DescriptorIndex"]:
            changed(name + " minimum", lambda s,name=name: definition(s,name).update(minimum=1))
            changed(name + " maximum", lambda s,name=name: definition(s,name).update(maximum=500))
            changed(name + " integer representation", lambda s,name=name: definition(s,name).pop("x-json-integer"))
        changed("Result opaque String", lambda s: definition(s,"TerminalResult").update(type="object"))
        changed("Result inner Err String", lambda s: definition(s,"TerminalResult")["x-json-content-schema"]["oneOf"][1]["properties"]["Err"].update(type="integer"))
        changed("Event error nullable required", lambda s: s["x-rust-serde-records"]["SupervisorEventTerminalParams"]["required-nullable"].remove("error"))
        changed("Event extraction order", lambda s: s["x-rust-serde-records"]["SupervisorEventTerminalParams"]["order"].reverse())
        for root,field,wrong in [("CommandAttachParams","terminal","Sequence"),("CommandAttachParams","stream","Sequence"),("CommandResizeParams","rows","Sequence"),("CommandTerminalCallParams","terminal","TerminalId"),("CommandTerminalCallParams","call","TerminalCondition"),("EventTerminalParams","status","TerminalId"),("EventTerminalParams","error","Sequence"),("EventTerminalResultParams","result","Sequence")]:
            changed(root + "." + field, lambda s,root=root,field=field,wrong=wrong: definition(s,root)["properties"][field].update({"$ref":"#/definitions/Supervisor"+wrong}))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract,"common",return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17b1_process_instances_and_notification_directions(self):
        fixtures = contract.read("fixtures.json")
        kind = contract.schema_validator({"$ref":"common.json#/definitions/SupervisorSpawnKind"})
        for case in fixtures["c17b1_spawn_kind_cases"]:
            with self.subTest(spawn_kind=case):
                self.assertEqual(kind.is_valid(case["value"]), case["valid"])
        for root in fixtures["c17b1_process_roots"]:
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + root["schema"]})
            for case in fixtures[root["cases"]]:
                with self.subTest(method=root["method"], case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17b1_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        # Unit maps and variant sequences are old Serde compatibility tested
        # through the actual runtime alias; canonical schemas use producer forms.

    def test_c17b1_process_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def changed(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def kind(s): return s["definitions"]["SupervisorSpawnKind"]
        def payload(s): return kind(s)["oneOf"][1]["properties"]["Terminal"]
        changed("external registry missing", lambda s: s.update({"x-rust-external-enums":[]}))
        changed("external registry duplicate", lambda s: s["x-rust-external-enums"].append("SupervisorSpawnKind"))
        changed("original enum name", lambda s: kind(s).update({"x-rust-name":"OtherSpawnKind"}))
        changed("Copy trait", lambda s: kind(s)["x-rust-derives"].remove("Copy"))
        changed("variant name", lambda s: kind(s)["x-rust-variants"][0].update(name="Execute"))
        changed("variant kind", lambda s: kind(s)["x-rust-variants"][2].update(kind="unit"))
        changed("duplicate variant", lambda s: kind(s)["x-rust-variants"].append(copy.deepcopy(kind(s)["x-rust-variants"][0])))
        changed("variant payload pointer", lambda s: kind(s)["x-rust-variants"][2].update({"schema-ref":"#/definitions/SupervisorWorktree"}))
        changed("open Terminal payload", lambda s: payload(s).update(additionalProperties=False))
        changed("closed external envelope", lambda s: kind(s)["oneOf"][1].update(additionalProperties=True))
        changed("optional Terminal field", lambda s: payload(s).update(required=["exit_report"]))
        changed("Terminal field order", lambda s: payload(s)["x-rust-field-order"].append("exit_report"))
        changed("Terminal default flag", lambda s: payload(s)["properties"]["exit_report"].update({"x-rust-default":True}))
        changed("Terminal native type", lambda s: payload(s)["properties"]["exit_report"].update({"x-rust-type":"Option<i32>"}))
        changed("i32 minimum", lambda s: s["definitions"]["SupervisorI32"].update(minimum=0))
        changed("i32 maximum", lambda s: s["definitions"]["SupervisorI32"].update(maximum=2**32-1))
        changed("i32 exact integer", lambda s: s["definitions"]["SupervisorI32"].pop("x-json-integer"))
        changed("optional i32 reference", lambda s: s["definitions"]["SupervisorOptionalI32"]["anyOf"][0].update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("environment tuple length", lambda s: s["definitions"]["SupervisorEnvironment"]["items"].update(maxItems=3))
        changed("environment tuple element", lambda s: s["definitions"]["SupervisorEnvironment"]["items"]["items"][0].update(type="integer"))
        for field, wrong in [("kind","SupervisorStdio"),("argv","SupervisorOptionalDirectory"),("cwd","SupervisorOptionalArguments"),("env","SupervisorOptionalArguments"),("stdio","SupervisorDescriptor")]:
            changed("Spawn reference " + field, lambda s,field=field,wrong=wrong: s["definitions"]["SupervisorCommandSpawnParams"]["properties"][field].update({"$ref":"#/definitions/" + wrong}))
        changed("signal signed type", lambda s: s["definitions"]["SupervisorCommandSignalParams"]["properties"]["signal"].update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("pid signed type", lambda s: s["definitions"]["SupervisorEventSpawnedParams"]["properties"]["pid"].update({"$ref":"#/definitions/SupervisorSequence"}))
        changed("cancel bool type", lambda s: s["definitions"]["SupervisorCommandCancelProcessParams"]["properties"]["everything"].update(type="integer"))
        for name, field in [("SupervisorCommandSpawnParams","argv"),("SupervisorCommandSpawnParams","cwd"),("SupervisorEventSpawnFailedParams","code"),("SupervisorEventExitedParams","signal")]:
            changed(name + " required nullable " + field, lambda s,name=name,field=field: s["x-rust-serde-records"][name]["required-nullable"].remove(field))
        changed("Spawn field extraction order", lambda s: s["x-rust-serde-records"]["SupervisorCommandSpawnParams"]["order"].reverse())
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17b1_operational_pending_inventory_rejects_completed_or_missing_methods(self):
        from unittest.mock import patch
        contract.check()
        original = contract.document
        doc = original("supervisor")
        pending = doc["x-pending-notifications"]
        names = [name for group in pending.values() for name in group]
        self.assertEqual(len(names), 8)
        self.assertEqual(len(set(names)), 8)
        completed = {r["method"] for r in contract.read("fixtures.json")["c17b1_process_roots"]}
        completed |= {case["frame"]["method"] for case in contract.read("fixtures.json")["c17b2_notification_cases"]}
        completed |= {case["frame"]["method"] for case in contract.read("fixtures.json")["c17c1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in contract.read("fixtures.json")["c17c2_notification_cases"]}
        completed |= {case["frame"]["method"] for case in contract.read("fixtures.json")["c17d1_notification_cases"]}
        completed |= {case["frame"]["method"] for case in contract.read("fixtures.json")["c17d2_notification_cases"]}
        declared = {r["name"] for r in doc["x-notifications"]}
        self.assertTrue(completed <= declared)
        self.assertFalse(set(names) & declared)
        self.assertEqual(set(pending), {"C17d-lifecycle-host-jobs","C17e-relay-logging"})
        for mutation in ["completed method pending", "missing pending method", "duplicate pending method", "unknown pending method", "wrong pending family"]:
            changed = copy.deepcopy(doc)
            registry = changed["x-pending-notifications"]
            group = registry["C17d-lifecycle-host-jobs"]
            if mutation == "completed method pending": group.append("Command.Spawn")
            elif mutation == "missing pending method": group.pop()
            elif mutation == "duplicate pending method": group.append(group[0])
            elif mutation == "unknown pending method": group.append("Command.Unknown")
            else: registry["C17e-relay-logging"].append(group.pop())
            with self.subTest(mutation=mutation), patch.object(contract, "document", side_effect=lambda endpoint: changed if endpoint == "supervisor" else original(endpoint)):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c17a_root_contract_instances_and_retired_phase_dependency(self):
        fixtures = contract.read("fixtures.json")
        for key, name in [("c17a_command_start_cases", "SupervisorCommandStartParams"),
                          ("c17a_event_exported_cases", "SupervisorEventExportedParams"),
                          ("c17a_event_manifest_cases", "SupervisorEventManifestParams"),
                          ("c17a_handoff_outcome_cases", "SupervisorHandoffOutcome")]:
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            for case in fixtures[key]:
                with self.subTest(record=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        for case in fixtures["c17a_notification_cases"]:
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
            opposite = "supervisor-to-daemon" if case["direction"] == "daemon-to-supervisor" else "daemon-to-supervisor"
            self.assertFalse(contract.validate_notification("supervisor", opposite, case["frame"]))
        doc = contract.document("supervisor")
        phase = next(p for p in doc["x-phases"] if p["name"] == "manifest-result")
        self.assertEqual(phase["pending-identities"], {})
        self.assertEqual(phase["allowed"], ["Event.Manifest", "PreflightFailed"])
        row = next(n for n in doc["x-notifications"] if n["name"] == "Event.Manifest")
        self.assertEqual(row["x-rust-name"], "EventManifest")
        self.assertEqual(row["direction"], phase["direction"])
        self.assertEqual(row["x-params-schema"], {"$ref":"common.json#/definitions/SupervisorEventManifestParams"})
        self.assertEqual(contract.resolve(row["x-params-schema"]), contract.common()["definitions"]["SupervisorEventManifestParams"])
        # Catalogue is only a String here. Original typed inner parsing and all
        # root/owner/count/adoption semantics stay at their runtime boundaries.

    def test_c17a_root_and_outcome_metadata_consistency_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def mutated(name, label, change):
            schema = copy.deepcopy(original)
            change(schema["definitions"][name])
            cases.append((label, schema))
        mutated("SupervisorCommandStartParams", "Start handoff reference", lambda d: d["properties"]["handoff"].update({"$ref":"#/definitions/SupervisorSource"}))
        mutated("SupervisorEventExportedParams", "Exported result reference", lambda d: d["properties"]["result"].update({"$ref":"#/definitions/SupervisorMaintenanceOutcome"}))
        mutated("SupervisorEventExportedParams", "Exported member type", lambda d: d["properties"]["member"].update(type="integer"))
        mutated("SupervisorHandoffOutcome", "Ok handoff reference", lambda d: d["oneOf"][0]["properties"]["Ok"].update({"$ref":"#/definitions/SupervisorDirectory"}))
        mutated("SupervisorHandoffOutcome", "Err must be string", lambda d: d["oneOf"][1]["properties"]["Err"].update(type=["string","null"]))
        mutated("SupervisorHandoffOutcome", "closed Ok branch", lambda d: d["oneOf"][0].update(additionalProperties=True))
        mutated("SupervisorHandoffOutcome", "required Err tag", lambda d: d["oneOf"][1].update(required=[]))
        mutated("SupervisorHandoffOutcome", "exclusive Result branches", lambda d: d.update(oneOf=d["oneOf"]+[copy.deepcopy(d["oneOf"][0])]))
        mutated("SupervisorEventManifestParams", "manifest sequence type", lambda d: d["properties"]["sequence"].update({"$ref":"#/definitions/SupervisorPath"}))
        mutated("SupervisorEventManifestParams", "manifest path type", lambda d: d["properties"]["path"].update({"$ref":"#/definitions/SupervisorSequence"}))
        mutated("SupervisorEventManifestParams", "nullable root shape", lambda d: d["properties"]["root"]["anyOf"][0].update({"$ref":"#/definitions/SupervisorDescriptor"}))
        mutated("SupervisorEventManifestParams", "nullable owner reference", lambda d: d["properties"]["owner"].update({"$ref":"#/definitions/SupervisorHandoffSlot"}))
        mutated("SupervisorEventManifestParams", "opaque catalogue string", lambda d: d["properties"]["catalogue"].update(type="object"))
        mutated("SupervisorEventManifestParams", "manifest transfer type", lambda d: d["properties"]["transfer"].update({"$ref":"#/definitions/SupervisorLeaseCount"}))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()
        for mutation in ["container name", "field order", "required nullable root", "required nullable owner"]:
            schema = copy.deepcopy(original)
            record = schema["x-rust-serde-records"]["SupervisorEventManifestParams"]
            if mutation == "container name": record["name"] = "OtherManifestParams"
            elif mutation == "field order": record["order"].reverse()
            elif mutation == "required nullable root": record["required-nullable"].remove("root")
            else: record["required-nullable"].remove("owner")
            with self.subTest(mutation=mutation), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c16c_phase_manifest_matches_original_identities_and_pending_scope(self):
        fixtures = contract.read("fixtures.json")
        doc = contract.document("supervisor")
        phases = doc["x-phases"]
        self.assertEqual([r["name"] for r in phases], [r["name"] for r in fixtures["c16c_phase_definitions"]])
        notifications = {r["name"]:r for r in doc["x-notifications"]}
        for expected in fixtures["c16c_phase_definitions"]:
            row = next(r for r in phases if r["name"] == expected["name"])
            with self.subTest(phase=expected["name"]):
                self.assertEqual(row["x-rust-name"], expected["rust"])
                self.assertEqual(row["direction"], expected["direction"])
                self.assertEqual(row["allowed"], expected["allowed"])
                self.assertEqual(row["consumer"], expected["consumer"])
                self.assertTrue(row["description"])
                self.assertEqual(row.get("x-dispatch-variants", {}), expected.get("variants", {}))
                self.assertEqual(set(row["pending-identities"]), set(expected["pending"]))
                for name in row["allowed"]:
                    if name in expected["pending"]:
                        self.assertEqual(row["pending-identities"][name]["chunk"], expected["pending"][name])
                        self.assertTrue(row["pending-identities"][name]["reason"])
                        self.assertNotIn(name, notifications)  # Identity-only is not payload migration.
                    else:
                        self.assertIn(notifications[name]["direction"], [row["direction"], "bidirectional"])
                        self.assertTrue(notifications[name]["x-rust-name"])
        for case in fixtures["c16c_phase_method_cases"]:
            row = next(r for r in phases if r["name"] == case["phase"])
            with self.subTest(case=case):
                self.assertEqual(case["method"] in row["allowed"], case["allowed"])
        # This checks the manifest. Separate Rust tests execute generated APIs;
        # reachable actor baseline tests cover guard placement and FD disposal.

    def test_c16c_phase_manifest_consistency_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.document
        cases = []
        def mutated(label, change):
            doc = copy.deepcopy(original("supervisor"))
            change(doc)
            cases.append((label, doc))
        mutated("missing phase", lambda d: d["x-phases"].pop())
        mutated("duplicate phase", lambda d: d["x-phases"].append(copy.deepcopy(d["x-phases"][0])))
        mutated("unknown phase name", lambda d: d["x-phases"][0].update(name="new-entry"))
        mutated("duplicate Rust phase name", lambda d: d["x-phases"][1].update({"x-rust-name":"Entry"}))
        mutated("wrong Rust phase name", lambda d: d["x-phases"][0].update({"x-rust-name":"OtherEntry"}))
        mutated("missing allowed method", lambda d: d["x-phases"][0]["allowed"].pop())
        mutated("duplicate allowed method", lambda d: d["x-phases"][0]["allowed"].append("Maintenance"))
        mutated("unknown allowed method", lambda d: d["x-phases"][0]["allowed"].append("Unknown"))
        mutated("wrong direction method", lambda d: d["x-phases"][1]["allowed"].append("MaintenanceFinished"))
        mutated("phase direction", lambda d: d["x-phases"][0].update(direction="supervisor-to-daemon"))
        mutated("consumer binding", lambda d: d["x-phases"][0].update(consumer="libs/supervisor/src/preflight.rs:start"))
        mutated("missing consumer", lambda d: d["x-phases"][0].pop("consumer"))
        mutated("wrong entry dispatch variant", lambda d: d["x-phases"][0]["x-dispatch-variants"].update(Preflight="Maintenance"))
        mutated("missing entry dispatch variant", lambda d: d["x-phases"][0]["x-dispatch-variants"].pop("Setup"))
        mutated("extra entry dispatch variant", lambda d: d["x-phases"][0]["x-dispatch-variants"].update(Unknown="Unknown"))
        mutated("missing Setup pending", lambda d: d["x-phases"][0]["pending-identities"].pop("Setup"))
        mutated("wrong Setup chunk", lambda d: d["x-phases"][0]["pending-identities"]["Setup"].update(chunk="C17"))
        mutated("empty pending reason", lambda d: d["x-phases"][0]["pending-identities"]["Setup"].update(reason=""))
        mutated("retired Event.Manifest pending reintroduced", lambda d: d["x-phases"][3]["pending-identities"].update({"Event.Manifest":{"chunk":"C17","reason":"retired payload fallback"}}))
        mutated("retired Event.Manifest rebound to wrong chunk", lambda d: d["x-phases"][3]["pending-identities"].update({"Event.Manifest":{"chunk":"C18","reason":"wrong payload fallback"}}))
        mutated("generated identity marked pending", lambda d: d["x-phases"][0]["pending-identities"].update(Maintenance={"chunk":"C18","reason":"wrong fallback"}))
        mutated("pending identity outside allowed list", lambda d: d["x-phases"][1]["pending-identities"].update(Setup={"chunk":"C18","reason":"wrong scope"}))
        mutated("dispatch classifier outside entry", lambda d: d["x-phases"][1].update({"x-dispatch-variants":{"Maintenance":"Maintenance"}}))
        for label, doc in cases:
            with self.subTest(mutation=label), patch.object(contract, "document", side_effect=lambda endpoint: doc if endpoint == "supervisor" else original(endpoint)):
                with self.assertRaises(ValueError):
                    contract.check()

    def _assert_c16b2_bootstrap_schema_closure(self):
        defs = contract.common()["definitions"]
        self.assertEqual(defs["SupervisorSourceSlot"], {"$ref":"#/definitions/SupervisorSource"})
        self.assertEqual(defs["SupervisorHandoffSlot"], {"anyOf":[{"$ref":"#/definitions/SupervisorHandoff"},{"type":"null"}]})
        for field, canonical, read in [("source", "SupervisorSourceSlot", "SupervisorSourceRead"),
                                       ("handoff", "SupervisorHandoffSlot", "SupervisorHandoffRead")]:
            self.assertEqual(defs["SupervisorStartParams"]["properties"][field], {"$ref":"#/definitions/" + canonical})
            self.assertEqual(defs["SupervisorStartRead"]["properties"][field], {"$ref":"#/definitions/" + read})
        for name in ["SupervisorSourceSlot", "SupervisorHandoffSlot", "SupervisorSource", "SupervisorHandoff"]:
            self.assertNotIn("x-generator-pending", defs[name])
        row = next(r for r in contract.document("supervisor")["x-notifications"] if r["name"] == "SupervisorStart")
        self.assertEqual(row["x-params-schema"], {"$ref":"common.json#/definitions/SupervisorStartParams"})

    def test_c16b2_nested_records_and_strict_bootstrap_schema_instances(self):
        self._assert_c16b2_bootstrap_schema_closure()
        fixtures = contract.read("fixtures.json")
        for key, name in [("c16b2_directory_cases", "SupervisorDirectory"), ("c16b2_lease_cases", "SupervisorLease"),
                          ("c16b2_handoff_cases", "SupervisorHandoff"), ("c16b2_strict_bootstrap_cases", "SupervisorStartParams")]:
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            self.assertTrue(fixtures[key])
            for case in fixtures[key]:
                with self.subTest(record=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        # Schema validates structural metadata, leaving count/more, FD ownership,
        # phase and inner dev-shell JSON semantics at the original runtime checks.

    def test_c16b2_leases_notification_both_directions_and_envelopes(self):
        for case in contract.read("fixtures.json")["c16b2_leases_notification_cases"]:
            with self.subTest(case=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
        self.assertFalse(contract.validate_notification("supervisor", "client-to-server", dict(jsonrpc="2.0",method="Leases",params={})))

    def test_c16b2_handoff_codegen_and_profile_links_reject_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def mutated(name, label, change):
            schema = copy.deepcopy(original)
            change(schema["definitions"][name])
            cases.append((name, label, schema))
        for name in ["SupervisorDirectory", "SupervisorLease", "SupervisorHandoff"]:
            mutated(name, "original record name", lambda d: d.update({"x-rust-name":"OtherRecord"}))
            mutated(name, "field order", lambda d: d["x-rust-field-order"].reverse())
            mutated(name, "closed fields", lambda d: d.update(additionalProperties=True))
            mutated(name, "required fields", lambda d: d["required"].pop())
            mutated(name, "derive traits", lambda d: d["x-rust-derives"].remove("Deserialize"))
        mutated("SupervisorDirectory", "fd Rust type", lambda d: d["properties"]["fd"].update({"x-rust-type":"u64"}))
        mutated("SupervisorLease", "lock Rust field name", lambda d: d["properties"]["lock"].update({"x-rust-field-name":"descriptor"}))
        mutated("SupervisorHandoff", "count native type", lambda d: d["properties"]["count"].update({"x-rust-type":"u64"}))
        mutated("SupervisorHandoff", "count schema reference", lambda d: d["properties"]["count"].update({"$ref":"#/definitions/SupervisorSequence"}))
        mutated("SupervisorHandoff", "working directory ref", lambda d: d["properties"]["working_directory"]["anyOf"][0].update({"$ref":"#/definitions/SupervisorLease"}))
        mutated("SupervisorHandoff", "working directory default flag", lambda d: d["properties"]["working_directory"].update({"x-rust-default":True}))
        mutated("SupervisorHandoff", "opaque string shape", lambda d: d["properties"]["dev_shell"].update(type="object"))
        mutated("SupervisorHandoff", "lease array reference", lambda d: d["properties"]["leases"]["items"].update({"$ref":"#/definitions/SupervisorDirectory"}))
        mutated("SupervisorLeaseCount", "native count bounds", lambda d: d.update(maximum=8388608))
        mutated("SupervisorLeaseCount", "exact integer representation", lambda d: d.pop("x-json-integer"))
        mutated("SupervisorSourceSlot", "canonical source link", lambda d: d.update({"$ref":"#/definitions/SupervisorSourceRead"}))
        mutated("SupervisorHandoffSlot", "canonical handoff link", lambda d: d["anyOf"][0].update({"$ref":"#/definitions/SupervisorHandoffRead"}))
        mutated("SupervisorStartRead", "raw source read link", lambda d: d["properties"]["source"].update({"$ref":"#/definitions/SupervisorSourceSlot"}))
        mutated("SupervisorStartRead", "raw handoff read link", lambda d: d["properties"]["handoff"].update({"$ref":"#/definitions/SupervisorHandoffSlot"}))
        for name, label, schema in cases:
            with self.subTest(record=name, mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c16b1_nested_subcodec_schema_instances(self):
        fixtures = contract.read("fixtures.json")
        for key, name in [("c16b1_stdio_cases", "SupervisorStdio"),
                          ("c16b1_source_cases", "SupervisorSource"),
                          ("c16b1_recreate_cases", "SupervisorRecreate"),
                          ("c16b1_worktree_cases", "SupervisorWorktree")]:
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            self.assertTrue(fixtures[key])
            for case in fixtures[key]:
                with self.subTest(record=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        # These canonical named records intentionally exclude legacy Serde
        # sequences pinned by runtime tests. Bootstrap now links complete schemas.
        self._assert_c16b2_bootstrap_schema_closure()

    def test_c16b1_nested_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def mutated(name, label, change):
            schema = copy.deepcopy(original)
            change(schema["definitions"][name])
            cases.append((name, label, schema))
        for name in ["SupervisorStdio", "SupervisorSource"]:
            mutated(name, "original container name", lambda d: d.update({"x-rust-name":"OtherWireType"}))
            mutated(name, "derive traits", lambda d: d["x-rust-derives"].remove("Deserialize"))
            mutated(name, "duplicate variant", lambda d: d["oneOf"].append(copy.deepcopy(d["oneOf"][0])))
            mutated(name, "unknown field order", lambda d: d["oneOf"][0]["x-rust-field-order"].append("ghost"))
            mutated(name, "open variant", lambda d: d["oneOf"][0].update(additionalProperties=True))
        mutated("SupervisorStdio", "descriptor array Rust type", lambda d: d["oneOf"][0]["properties"]["fds"].update({"x-rust-type":"Vec<crate::contract::Descriptor>"}))
        mutated("SupervisorStdio", "descriptor array length", lambda d: d["oneOf"][0]["properties"]["fds"].update(maxItems=4))
        mutated("SupervisorStdio", "u16 exact integer", lambda d: d["oneOf"][2]["properties"]["rows"].pop("x-json-integer"))
        mutated("SupervisorSource", "Recreate reference", lambda d: d["oneOf"][0]["properties"]["recreate"]["anyOf"][0].update({"$ref":"#/definitions/SupervisorWorktree"}))
        mutated("SupervisorSource", "scope descriptor Rust type", lambda d: d["oneOf"][1]["properties"]["scope"].update({"x-rust-type":"Option<String>"}))
        mutated("SupervisorSource", "worktree explicit default", lambda d: d["oneOf"][1]["properties"]["worktree"].pop("x-rust-default"))
        mutated("SupervisorSource", "arguments explicit default", lambda d: d["oneOf"][1]["properties"]["arguments"].pop("x-rust-default"))
        mutated("SupervisorSource", "required sequence", lambda d: d["oneOf"][0]["required"].remove("sequence"))
        for name in ["SupervisorRecreate", "SupervisorWorktree"]:
            mutated(name, "original record name", lambda d: d.update({"x-rust-name":"OtherRecord"}))
            mutated(name, "field order", lambda d: d["x-rust-field-order"].reverse())
            mutated(name, "required fields", lambda d: d["required"].pop())
        mutated("SupervisorRecreate", "ignore additive fields", lambda d: d.update(additionalProperties=False))
        mutated("SupervisorRecreate", "worktree explicit default", lambda d: d["properties"]["worktree"].pop("x-rust-default"))
        mutated("SupervisorRecreate", "worktree Rust type", lambda d: d["properties"]["worktree"].update({"x-rust-type":"Option<::std::path::PathBuf>"}))
        mutated("SupervisorWorktree", "closed fields", lambda d: d.update(additionalProperties=True))
        mutated("SupervisorWorktree", "path public field name", lambda d: d["properties"]["path"].update({"x-rust-field-name":"directory"}))
        mutated("SupervisorWorktree", "path Rust type", lambda d: d["properties"]["path"].update({"x-rust-type":"String"}))
        for name, label, schema in cases:
            with self.subTest(record=name, mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c16a2_descriptor_and_outer_projection_schema_instances(self):
        fixtures = contract.read("fixtures.json")
        for key, name in [("c16a2_descriptor_cases", "SupervisorDescriptor"),
                          ("c16a2_start_projection_cases", "SupervisorStartRead"),
                          ("c16a2_maintenance_projection_cases", "SupervisorMaintenanceParams")]:
            validator = contract.schema_validator({"$ref":"common.json#/definitions/" + name})
            self.assertTrue(fixtures[key])
            for case in fixtures[key]:
                with self.subTest(record=name, case=case):
                    self.assertEqual(validator.is_valid(case["value"]), case["valid"])
        # Preserve C16a2's unchanged outer fixture data against the separate
        # decoder projection, and require complete canonical bootstrap links.
        self._assert_c16b2_bootstrap_schema_closure()

    def test_c16a2_explicit_descriptor_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def mutated(label, change):
            schema = copy.deepcopy(original)
            change(schema)
            cases.append((label, schema))
        def record(schema):
            return schema["definitions"]["SupervisorDescriptor"]
        mutated("missing explicit registry", lambda s: s.update({"x-rust-explicit-records":[]}))
        mutated("duplicate explicit registry", lambda s: s["x-rust-explicit-records"].append("SupervisorDescriptor"))
        mutated("original container name", lambda s: record(s).update({"x-rust-name":"OtherDescriptor"}))
        mutated("public Rust field name", lambda s: record(s)["properties"]["$fd"].update({"x-rust-field-name":"fd"}))
        mutated("usize Rust field type", lambda s: record(s)["properties"]["$fd"].update({"x-rust-type":"u64"}))
        mutated("Copy trait", lambda s: record(s)["x-rust-derives"].remove("Copy"))
        mutated("unknown derive", lambda s: record(s)["x-rust-derives"].append("Default"))
        mutated("missing field order", lambda s: record(s).update({"x-rust-field-order":[]}))
        mutated("duplicate field order", lambda s: record(s)["x-rust-field-order"].append("$fd"))
        mutated("index schema reference", lambda s: record(s)["properties"]["$fd"].update({"$ref":"#/definitions/SupervisorSequence"}))
        mutated("required descriptor index", lambda s: record(s).update(required=[]))
        mutated("closed descriptor object", lambda s: record(s).update(additionalProperties=True))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c16a2_added_private_catalog_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.document
        for name in ["SupervisorStart", "Maintenance"]:
            for mutation in ["direction", "order", "required", "schema", "closed strategy"]:
                doc = copy.deepcopy(original("supervisor"))
                row = next(r for r in doc["x-notifications"] if r["name"] == name)
                if mutation == "direction": row["direction"] = "supervisor-to-daemon"
                elif mutation == "order": row["params"].reverse()
                elif mutation == "required": row["params"][0]["required"] = False
                elif mutation == "schema": row["params"][0]["schema"] = {"type":"string"}
                else: row["x-positional-schema"] = {"type":"array"}
                with self.subTest(record=name, mutation=mutation), patch.object(contract, "document", side_effect=lambda endpoint: doc if endpoint == "supervisor" else original(endpoint)):
                    with self.assertRaises(ValueError):
                        contract.check()

    def test_c16a1_private_notification_instances(self):
        for case in contract.read("fixtures.json")["c16a1_supervisor_notifications"]:
            with self.subTest(case=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("supervisor", case["direction"], case["frame"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("supervisor", case["direction"], case["frame"])
        self.assertFalse(contract.validate_notification("supervisor", "daemon-to-supervisor",
            dict(jsonrpc="2.0", method="Stopping", params={})))

    def test_c16a1_maintenance_operation_schema_instances(self):
        validator = contract.schema_validator({"$ref":"common.json#/definitions/SupervisorMaintenanceOperation"})
        for case in contract.read("fixtures.json")["c16a1_maintenance_operations"]:
            with self.subTest(case=case):
                self.assertEqual(validator.is_valid(case["value"]), case["valid"])

    def test_c16a1_private_catalog_consistency_rejects_drift(self):
        from unittest.mock import patch
        contract.check()  # The unmodified input must have no setup/reference errors.
        original = contract.document
        cases = []
        def mutated(label, change):
            doc = copy.deepcopy(original("supervisor"))
            change(doc)
            cases.append((label, doc))
        mutated("duplicate notification", lambda d: d["x-notifications"].append(copy.deepcopy(d["x-notifications"][0])))
        mutated("duplicate Rust marker", lambda d: d["x-notifications"][1].update({"x-rust-name":"Preflight"}))
        mutated("invalid direction", lambda d: d["x-notifications"][0].update(direction="client-to-server"))
        mutated("missing sender", lambda d: d["x-notifications"][0].pop("sender"))
        mutated("parameter order", lambda d: d["x-notifications"][0]["params"].reverse())
        mutated("required mismatch", lambda d: d["x-notifications"][0]["params"][0].update(required=False))
        mutated("parameter shape", lambda d: d["x-notifications"][0]["params"][0].update(schema={"type":"string"}))
        mutated("unknown parameter", lambda d: d["x-notifications"][0]["params"][0].update(name="not_a_field"))
        mutated("positional accepted", lambda d: d["x-notifications"][0].update({"x-positional-schema":{"type":"array"}}))
        for label, doc in cases:
            with self.subTest(mutation=label), patch.object(contract, "document", side_effect=lambda endpoint: doc if endpoint == "supervisor" else original(endpoint)):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c16a1_tagged_enum_codegen_metadata_rejects_drift(self):
        from unittest.mock import patch
        contract.check()
        original = contract.common()
        cases = []
        def mutated(label, change):
            schema = copy.deepcopy(original)
            change(schema["definitions"]["SupervisorMaintenanceOperation"])
            cases.append((label, schema))
        mutated("duplicate variant", lambda e: e["oneOf"][1]["properties"]["kind"].update(enum=["Gc"]))
        mutated("unknown ordered field", lambda e: e["oneOf"][0]["x-rust-field-order"].append("ghost"))
        mutated("missing ordered field", lambda e: e["oneOf"][0]["x-rust-field-order"].pop())
        mutated("bool Rust type mismatch", lambda e: e["oneOf"][0]["properties"]["dry_run"].update({"x-rust-type":"String"}))
        mutated("optional Rust type mismatch", lambda e: e["oneOf"][0]["properties"]["runtime"].update({"x-rust-type":"::std::path::PathBuf"}))
        mutated("nullable required Rust field", lambda e: e["oneOf"][0]["properties"]["state"].update({"x-rust-type":"Option<::std::path::PathBuf>"}))
        for label, schema in cases:
            with self.subTest(mutation=label), patch.object(contract, "common", return_value=schema):
                with self.assertRaises(ValueError):
                    contract.check()

    def test_c15_state_parameter_result_and_snapshot_contracts(self):
        fixtures = contract.read("fixtures.json")
        for key in ("state_parameter_cases", "state_result_cases", "state_snapshot_cases"):
            self.assertTrue(fixtures[key], key)
        for case in fixtures["state_parameter_cases"]:
            with self.subTest(parameters=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_params("host", case["method"], case["params"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_params("host", case["method"], case["params"])
        for case in fixtures["state_result_cases"]:
            reply = dict(jsonrpc="2.0", id=1, result=case["result"])
            with self.subTest(result=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_reply("host", case["method"], reply))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_reply("host", case["method"], reply)
        validator = contract.schema_validator({"$ref": "common.json#/definitions/StateSnapshot"})
        for case in fixtures["state_snapshot_cases"]:
            with self.subTest(snapshot=case):
                self.assertEqual(validator.is_valid(case["value"]), case["valid"])

    def test_c15_state_notification_direction_and_legacy_initial_normalization(self):
        fixtures = contract.read("fixtures.json")
        self.assertTrue(fixtures["state_notification_cases"])
        self.assertTrue(fixtures["state_legacy_initial_cases"])
        for case in fixtures["state_notification_cases"]:
            frame = dict(jsonrpc="2.0", method="state.changed", params=case["params"])
            with self.subTest(notification=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_notification("host", "server-to-client", frame))
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("host", "server-to-client", dict(frame, id=1))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_notification("host", "server-to-client", frame)
            self.assertFalse(contract.validate_notification("host", "client-to-server", frame))
            self.assertFalse(contract.validate_notification("sandbox", "server-to-client", frame))
        self.assertIsNone(contract.method("host", "state.changed"))
        for raw in fixtures["state_legacy_initial_cases"]:
            # Only the compatible field-normalization artifact is validated;
            # actual Rust/Emacs acceptance is pinned by the runtime baseline.
            with self.subTest(legacy_initial=raw):
                self.assertTrue(contract.validate_reply("host", "state.subscribe",
                    dict(jsonrpc="2.0", id=1, result=raw), compatibility=True))

    def test_c15_public_coverage_rejects_pending_exemption_reintroduction(self):
        registry = copy.deepcopy(contract.read("coverage.json"))
        self.assertEqual(registry["pending"], {})
        self.assertEqual(registry["notifications"], [])
        self.assertEqual(registry["exchanges"], [])
        contract.check_coverage()
        sources = contract.implementation_sources()
        anchor = "contract::host::HostCancel::NAME"
        self.assertIn(anchor, sources["controller"])
        changed = dict(sources)
        changed["controller"] = sources["controller"].replace(anchor,
            'm if matches!(m, "server.extra") => Ok(json!({})),\n            ' + anchor, 1)
        registry["pending"]["C99"] = {"host": ["server.extra"]}
        with self.assertRaises(ValueError):
            contract.check_coverage(registry, changed)

    def test_c15_coverage_rejects_catalog_direction_completion_and_refusal_changes(self):
        from unittest.mock import patch
        original = contract.document
        host = copy.deepcopy(original("host"))
        host["methods"] = [row for row in host["methods"] if row["name"] != "state.subscribe"]
        self.assertNotEqual(len(host["methods"]), len(original("host")["methods"]))
        with patch.object(contract, "document", side_effect=lambda endpoint: host if endpoint == "host" else original(endpoint)):
            with self.assertRaises(ValueError):
                contract.check_coverage()
        host = copy.deepcopy(original("host"))
        row = next(row for row in host["x-notifications"] if row["name"] == "state.changed")
        row["direction"] = "client-to-server"
        with patch.object(contract, "document", side_effect=lambda endpoint: host if endpoint == "host" else original(endpoint)):
            with self.assertRaises(ValueError):
                contract.check_coverage()
        attach = copy.deepcopy(original("attach"))
        self.assertTrue(attach["x-completions"])
        attach["x-completions"] = attach["x-completions"][1:]
        with patch.object(contract, "document", side_effect=lambda endpoint: attach if endpoint == "attach" else original(endpoint)):
            with self.assertRaises(ValueError):
                contract.check_coverage()
        registry = copy.deepcopy(contract.read("coverage.json"))
        registry["refusals"] = [row for row in registry["refusals"] if row["name"] != "sessions.attach"]
        with self.assertRaises(ValueError):
            contract.check_coverage(registry)
        self.assertIsNone(contract.method("sandbox", "sessions.attach"))
        sandbox = copy.deepcopy(original("sandbox"))
        added = copy.deepcopy(sandbox["methods"][0])
        added.update(name="sessions.attach", **{"x-rust-name": "ForbiddenAttach"})
        sandbox["methods"].append(added)
        with patch.object(contract, "document", side_effect=lambda endpoint: sandbox if endpoint == "sandbox" else original(endpoint)):
            with self.assertRaises(ValueError):
                contract.check_coverage()

    def test_c15_coverage_dispatch_cannot_be_supplied_by_comments_or_raw_strings(self):
        sources = contract.implementation_sources()
        source = sources["controller"]
        body = contract.rust_function(source, "host")
        generated = "contract::host::StateSubscribe::NAME"
        target = generated if generated in body else '"state.subscribe"'
        replacement = "contract::host::StateUnsubscribe::NAME" if generated in body else '"state.unsubscribe"'
        self.assertEqual(body.count(target), 1)
        missing = body.replace(target, replacement, 1)
        for decoy in ("", "// " + generated + "\n", "/* outer /* nested */ " + generated + " */\n",
                      'let decoy = r###"' + generated + '"###;\n'):
            changed = dict(sources)
            changed["controller"] = source.replace(body, decoy + missing, 1)
            with self.subTest(decoy=decoy), self.assertRaises(ValueError):
                contract.check_coverage(sources=changed)

    def test_c15_coverage_requires_real_state_emitter_and_receiver(self):
        import re
        sources = contract.implementation_sources()
        for key, function in (("controller", "publish"), ("host_client", "tick")):
            source = sources[key]
            body = contract.rust_function(source, function)
            if key == "controller":
                pattern = r"contract::notification_value::<\s*contract::host_notifications::StateChanged\s*,?\s*>"
                match = re.search(pattern, body)
                self.assertIsNotNone(match)
                missing = body[:match.start()] + "unbound_state_value" + body[match.end():]
                identity = "contract::notification_value::<contract::host_notifications::StateChanged>"
            else:
                identity = "contract::host_notifications::StateChanged::NAME"
                self.assertEqual(body.count(identity), 1)
                missing = body.replace(identity, '\"unbound\"', 1)
            for decoy in ("", "// " + identity + "\n", "/* " + identity + " */\n",
                          'let decoy = r###"' + identity + '"###;\n'):
                changed = dict(sources)
                changed[key] = source.replace(body, decoy + missing, 1)
                with self.subTest(peer=key, decoy=decoy), self.assertRaises(ValueError):
                    contract.check_coverage(sources=changed)

    def test_c15_coverage_incidental_method_probe_cannot_replace_real_dispatch(self):
        import re
        sources = contract.implementation_sources()
        source = sources["controller"]
        body = contract.rust_function(source, "host")
        header = re.search(r"contract::host::StateSubscribe::NAME\s*=>\s*\{", body)
        self.assertIsNotNone(header)
        arm_body = contract.rust_block(body, header.end() - 1)
        arm = body[header.start():header.end()] + arm_body + "}"
        self.assertIn(arm, body)
        missing = body.replace(arm, "", 1)
        self.assertNotIn("contract::host::StateSubscribe::NAME", missing)
        self.assertIn("match method {", missing)
        # Real code mentions the identity, but never dispatches that request.
        decoy = "let _is_state = matches!(method, contract::host::StateSubscribe::NAME);\n"
        modified = missing.replace("match method {", decoy + "match method {", 1)
        changed = dict(sources)
        changed["controller"] = source.replace(body, modified, 1)
        self.assertNotIn(arm, contract.rust_function(changed["controller"], "host"))
        with self.assertRaises(ValueError):
            contract.check_coverage(sources=changed)

    def test_c15_coverage_incidental_helper_cannot_replace_actual_tick_reader(self):
        import re
        sources = contract.implementation_sources()
        source = sources["host_client"]
        body = contract.rust_function(source, "tick")
        comparison = r"\|\|\s*value\.method\.as_deref\(\)\s*!=\s*Some\(\s*contract::host_notifications::StateChanged::NAME\s*\)"
        missing, count = re.subn(comparison, "", body, count=1)
        self.assertEqual(count, 1)
        self.assertNotIn("contract::host_notifications::StateChanged::NAME", missing)
        changed = dict(sources)
        modified = source.replace(body, missing, 1)
        marker = "\n#[cfg(test)]\nmod tests {"
        self.assertIn(marker, modified)
        # A separate production helper is not the Subscription.tick reader.
        helper = '\nfn incidental_state_label(method: &str) -> bool { method == contract::host_notifications::StateChanged::NAME }\n'
        changed["host_client"] = modified.replace(marker, helper + marker, 1)
        self.assertNotIn("contract::host_notifications::StateChanged::NAME",
                         contract.rust_function(changed["host_client"], "tick"))
        with self.assertRaises(ValueError):
            contract.check_coverage(sources=changed)

    def test_c15_coverage_incidental_sandbox_probe_cannot_replace_dispatch(self):
        import re
        sources = contract.implementation_sources()
        source = sources["controller"]
        body = contract.rust_function(source, "sandbox_control")
        header = re.search(r"contract::sandbox::SessionsSetState::NAME\s*=>\s*\{", body)
        self.assertIsNotNone(header)
        arm_body = contract.rust_block(body, header.end() - 1)
        arm = body[header.start():header.end()] + arm_body + "}"
        self.assertIn(arm, body)
        missing = body.replace(arm, "", 1)
        self.assertNotIn("contract::sandbox::SessionsSetState::NAME", missing)
        self.assertIn("match method {", missing)
        probe = "let _is_state = matches!(method, contract::sandbox::SessionsSetState::NAME);\n"
        modified = missing.replace("match method {", probe + "match method {", 1)
        changed = dict(sources)
        changed["controller"] = source.replace(body, modified, 1)
        self.assertNotIn(arm, contract.rust_function(changed["controller"], "sandbox_control"))
        with self.assertRaises(ValueError):
            contract.check_coverage(sources=changed)

    def test_c15_coverage_nested_test_helper_cannot_replace_tick_reader(self):
        import re
        sources = contract.implementation_sources()
        source = sources["host_client"]
        body = contract.rust_function(source, "tick")
        comparison = r"\|\|\s*value\.method\.as_deref\(\)\s*!=\s*Some\(\s*contract::host_notifications::StateChanged::NAME\s*\)"
        missing, count = re.subn(comparison, "", body, count=1)
        self.assertEqual(count, 1)
        self.assertNotIn("contract::host_notifications::StateChanged::NAME", missing)
        helper = '''
        #[cfg(test)]
        fn test_only_state_guard(value: &contract::StateChangedRead) -> Result<()> {
            if value.method.as_deref() != Some(contract::host_notifications::StateChanged::NAME) {
                return Err("subscription gap; reconnect for a fresh snapshot".into());
            }
            Ok(())
        }
'''
        changed = dict(sources)
        changed["host_client"] = source.replace(body, helper + missing, 1)
        mutated = contract.rust_function(changed["host_client"], "tick")
        self.assertIn("#[cfg(test)]", mutated)
        self.assertEqual(mutated.count("contract::host_notifications::StateChanged::NAME"), 1)
        with self.assertRaises(ValueError):
            contract.check_coverage(sources=changed)

    def test_c14_history_parameters_and_current_results_match_contract(self):
        fixtures = contract.read("fixtures.json")
        parameter_cases = fixtures["history_parameter_cases"]
        result_cases = fixtures["history_result_cases"]
        self.assertTrue(parameter_cases)
        self.assertTrue(result_cases)
        self.assertEqual({case["method"] for case in parameter_cases}, {"history.list", "history.prune"})
        self.assertEqual({case["method"] for case in result_cases}, {"history.list", "history.prune"})
        for case in parameter_cases:
            with self.subTest(parameters=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_params("host", case["method"], case["params"]))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_params("host", case["method"], case["params"])
        for case in result_cases:
            reply = dict(jsonrpc="2.0", id=1, result=case["result"])
            with self.subTest(result=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_reply("host", case["method"], reply))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_reply("host", case["method"], reply)
        # Pin the current producer boundary independently of legacy acceptance.
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

    def test_c14_history_compatible_readers_preserve_legacy_projection_cases(self):
        fixtures = contract.read("fixtures.json")
        list_cases = fixtures["history_legacy_list_cases"]
        prune_cases = fixtures["history_legacy_prune_cases"]
        self.assertTrue(list_cases)
        self.assertTrue(prune_cases)
        for raw in list_cases:
            with self.subTest(legacy_list=raw):
                self.assertTrue(contract.validate_reply("host", "history.list",
                    dict(jsonrpc="2.0", id=1, result=raw), compatibility=True))
        # The original list reader falls back to an empty array for these.
        for raw in ({}, None, [], {"records": None}, {"records": 17}):
            with self.subTest(empty_fallback=raw):
                self.assertTrue(contract.validate_reply("host", "history.list",
                    dict(jsonrpc="2.0", id=1, result=raw), compatibility=True))
        for case in prune_cases:
            reply = dict(jsonrpc="2.0", id=1, result=case["result"])
            with self.subTest(legacy_prune=case):
                if case["valid"]:
                    self.assertTrue(contract.validate_reply("host", "history.prune", reply, compatibility=True))
                else:
                    with self.assertRaises(ValidationError):
                        contract.validate_reply("host", "history.prune", reply, compatibility=True)
        # Individual failure rows are opaque to collection decoding, while
        # malformed collections and nonstring removed IDs remain errors.
        partial = dict(removed=[], failed=[None, 17, dict(id=17, reason=False)])
        self.assertTrue(contract.validate_reply("host", "history.prune",
            dict(jsonrpc="2.0", id=1, result=partial), compatibility=True))
        for broken in ({}, dict(removed=[], failed=None), dict(removed=[], failed=17),
                       dict(removed=None, failed=[]), dict(removed=[17], failed=[])):
            with self.subTest(legacy_collection=broken), self.assertRaises(ValidationError):
                contract.validate_reply("host", "history.prune",
                    dict(jsonrpc="2.0", id=1, result=broken), compatibility=True)

    def _c13b3_check_coverage(self, *, sources=None, schema=None):
        """Supply the proposed override API, or equivalent overlays on the old checker."""
        import inspect
        from unittest.mock import patch
        check = contract.check_communication_readers_and_producers
        if "sources" in inspect.signature(check).parameters:
            return check(sources=sources, schema=schema)
        # This shim lets the unchanged assertions expose the old scanner bug
        # before its override API exists; after implementation use the real API.
        schema = contract.common() if schema is None else schema
        original_read = Path.read_text
        def read(path, *args, **kwargs):
            relative = str(path.relative_to(contract.ROOT)) if path.is_relative_to(contract.ROOT) else None
            if relative in (sources or {}):
                return sources[relative]
            return original_read(path, *args, **kwargs)
        with patch.object(contract, "common", return_value=schema), patch.object(Path, "read_text", read):
            return check()

    def test_c13b3_constructor_scanner_accepts_current_sources(self):
        self._c13b3_check_coverage()

    def test_c13b3_constructor_scanner_rejects_missing_or_changed_constructor(self):
        path = "packages/daemon/src/raid.rs"
        source = (contract.ROOT / path).read_text()
        constructor = "CommunicationPayload::new::<kinds::RaidCreated>"
        self.assertEqual(source.count(constructor), 1)
        self.assertIn("contract::communication_events::RaidCreated::KIND", source)
        for replacement in ("removed_constructor", "CommunicationPayload::new::<kinds::RaidJoined>"):
            with self.subTest(replacement=replacement), self.assertRaises(ValueError):
                self._c13b3_check_coverage(sources={path: source.replace(constructor, replacement)})

    def test_c13b3_constructor_scanner_ignores_comments_and_raw_strings(self):
        path = "packages/daemon/src/raid.rs"
        source = (contract.ROOT / path).read_text()
        constructor = "CommunicationPayload::new::<kinds::RaidCreated>"
        missing = source.replace(constructor, "removed_constructor")
        for decoy in ("// " + constructor + "\n",
                      "/* outer /* nested */ " + constructor + " */\n",
                      'const DECOY: &str = r###"' + constructor + '"###;\n',
                      'const DECOY: &str = "CommunicationPayload::new::<kinds::RaidCreated>";\n'):
            with self.subTest(decoy=decoy), self.assertRaises(ValueError):
                self._c13b3_check_coverage(sources={path: decoy + missing})

    def test_c13b3_constructor_scanner_rejects_wrong_alias(self):
        path = "packages/daemon/src/raid.rs"
        source = (contract.ROOT / path).read_text()
        alias = "use contract::communication_events as kinds;"
        self.assertEqual(source.count(alias), 1)
        with self.assertRaises(ValueError):
            self._c13b3_check_coverage(sources={path: source.replace(alias, "use unrelated::communication_events as kinds;")})

    def test_c13b3_constructor_scanner_rejects_raw_boundary_escape(self):
        path = "packages/daemon/src/mailbox.rs"
        source = (contract.ROOT / path).read_text()
        self.assertIn("    fn record(\n", source)
        for visibility in ("pub", "pub(crate)"):
            with self.subTest(visibility=visibility), self.assertRaises(ValueError):
                self._c13b3_check_coverage(sources={path: source.replace("    fn record(\n", f"    {visibility} fn record(\n", 1)})
        worker = "packages/daemon/src/messaging.rs"
        worker_source = (contract.ROOT / worker).read_text()
        escape = '\nfn untyped_escape(mailbox: &mut Mailbox) { mailbox.record("a", "future.kind", serde_json::json!({}), 0, journal); }\n'
        worker_source = worker_source.replace("\n#[cfg(test)]", escape + "\n#[cfg(test)]", 1)
        with self.assertRaises(ValueError):
            self._c13b3_check_coverage(sources={worker: worker_source})

    def test_c13b3_constructor_scanner_rejects_reader_kind_retargeting(self):
        schema = copy.deepcopy(contract.common())
        reader = next(row for row in schema["x-communication-string-readers"]
                      if row["record"] == "CommunicationSessionStartedRead")
        self.assertEqual(reader["kind"], "session.started")
        reader["kind"] = "session.exited"
        with self.assertRaises(ValueError):
            self._c13b3_check_coverage(schema=schema)

    def test_c13b3_constructor_scanner_rejects_restricted_raw_visibility(self):
        path = "packages/daemon/src/mailbox.rs"
        source = (contract.ROOT / path).read_text()
        declaration = "    fn record(\n"
        self.assertEqual(source.count(declaration), 1)
        # crate and this defining module are valid visibility ancestors.
        for visibility in ("pub(in crate)", "pub(in crate::mailbox)"):
            with self.subTest(visibility=visibility), self.assertRaises(ValueError):
                self._c13b3_check_coverage(sources={
                    path: source.replace(declaration, f"    {visibility} fn record(\n", 1)})

    def test_c13b3_constructor_scanner_rejects_ufcs_raw_publication(self):
        path = "packages/daemon/src/mailbox.rs"
        source = (contract.ROOT / path).read_text()
        wrapper = source.index("    pub fn record_typed<")
        delegation = "        self.record(actor, kind, data, now, audit)"
        insertion = source.index(delegation, wrapper)
        for receiver in ("Self", "Mailbox"):
            escape = f'        {receiver}::record(self, actor, "unbound.kind", json!({{}}), now, audit)?;\n'
            modified = source[:insertion] + escape + source[insertion:]
            # The proper typed delegation is retained: the extra UFCS call
            # must be detected in addition to the ordinary .record call.
            self.assertEqual(modified.count(delegation), 1)
            with self.subTest(receiver=receiver), self.assertRaises(ValueError):
                self._c13b3_check_coverage(sources={path: modified})

    def test_communication_payload_kind_pairing_and_notice_correlations(self):
        for case in contract.read("fixtures.json")["communication_payload_cases"]:
            event=dict(instance="i",sequence=1,timestamp=9,kind=case["kind"],actor="host",sessions=["s"],data=case["data"])
            page=dict(events=[event],cursor=1,session=None,latest=1)
            if case["valid"]:self.assertTrue(contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=page)))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=page))

    def test_communications_pagination_contract_and_legacy_page_projection(self):
        fixtures=contract.read("fixtures.json")
        for value in fixtures["communications_parameter_cases"]:
            structural=contract.schema_validator(contract.method("host","communications.list")["x-positional-schema"] if isinstance(value,list) else contract.method("host","communications.list")["x-params-schema"]).is_valid(value)
            valid=structural and (not isinstance(value,dict) or 1<=value.get("limit",100)<=100)
            if valid:self.assertTrue(contract.validate_params("host","communications.list",value))
            else:
                with self.assertRaises(ValidationError):contract.validate_params("host","communications.list",value)
        for value in fixtures["communications_page_cases"]:
            self.assertTrue(contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=value),compatibility=True))
        self.assertTrue(contract.validate_params("host","communications.list",dict(after=0,limit=100)))
        page=dict(events=[fixtures["communications_event"]],cursor=1,latest=1,session=None)
        self.assertTrue(contract.validate_reply("host","communications.list",dict(jsonrpc="2.0",id=1,result=page)))

    def test_inbox_status_contract_rejects_malformed_core_and_accepts_older_additions(self):
        for endpoint in ("host","sandbox"):
            for case in contract.read("fixtures.json")["inbox_status_cases"]:
                reply=dict(jsonrpc="2.0",id=1,result=case["value"])
                if case["valid"]:self.assertTrue(contract.validate_reply(endpoint,"inbox.status",reply,compatibility=True))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,"inbox.status",reply,compatibility=True)
            for value in ({},[]):self.assertTrue(contract.validate_params(endpoint,"inbox.status",value))
            for value in ({"message":""},["m"]):self.assertTrue(contract.validate_params(endpoint,"messages.get",value))

    def test_mailbox_mutation_contracts_and_result_relationships(self):
        fixtures=contract.read("fixtures.json")
        for endpoint in ("host","sandbox"):
            for case in fixtures["mailbox_mutation_cases"]:
                if case["valid"]:self.assertTrue(contract.validate_params(endpoint,case["method"],case["params"]))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params(endpoint,case["method"],case["params"])
            for case in fixtures["mailbox_byte_cases"]:
                params={**case["base"],case["field"]:case["unit"]*case["repeat"]+case["suffix"]}
                self.assertEqual(len(params[case["field"]].encode("utf-8")),case["bytes"])
                positional=[params[p["name"]] for p in contract.method(endpoint,case["method"])["params"]]
                for wire in (params,positional):
                    if case["valid"]:self.assertTrue(contract.validate_params(endpoint,case["method"],wire))
                    else:
                        with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params(endpoint,case["method"],wire)
            for case in fixtures["mailbox_mutation_results"]:
                self.assertTrue(contract.validate_reply(endpoint,case["method"],dict(jsonrpc="2.0",id=1,result=case["result"])))
            for case in fixtures["mailbox_next_cases"]:
                reply=dict(jsonrpc="2.0",id=1,result=case["result"])
                if case["valid"]:self.assertTrue(contract.validate_reply(endpoint,"inbox.next",reply))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,"inbox.next",reply)
            for value in [dict(message=None,claim_generation=1),dict(message=fixtures["mailbox_message"],claim_generation=None)]:
                with self.assertRaises(ValidationError):contract.validate_reply(endpoint,"inbox.next",dict(jsonrpc="2.0",id=1,result=value))
            with self.assertRaises(ValidationError):contract.validate_reply(endpoint,"inbox.complete",dict(jsonrpc="2.0",id=1,result=dict(message="m",state="queued")))

    def test_mailbox_u64_schema_matches_original_wire_integer_ranges(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["mailbox_u64_cases"]:
            for endpoint in ("host","sandbox"):
                for name,base in [("messages.reply",dict(key="k",message="m",body="hello")),("inbox.complete",dict(key="k",message="m")),("inbox.requeue",dict(key="k",message="m",reason="later"))]:
                    params={**base,"claim_generation":case["value"]}
                    positional=[params[p["name"]] for p in contract.method(endpoint,name)["params"]]
                    for wire in (params,positional):
                        if case["claim"]:self.assertTrue(contract.validate_params(endpoint,name,wire))
                        else:
                            with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params(endpoint,name,wire)
                for field in ("created_sequence","created_at","claim_generation"):
                    message={**fixtures["mailbox_message"],field:case["value"]}
                    if case["unsigned"]:self.assertTrue(contract.validate_reply(endpoint,"messages.send",dict(jsonrpc="2.0",id=1,result=message)))
                    else:
                        with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,"messages.send",dict(jsonrpc="2.0",id=1,result=message))
                result=dict(message="m",claim_generation=case["value"])
                if case["unsigned"]:self.assertTrue(contract.validate_reply(endpoint,"inbox.requeue",dict(jsonrpc="2.0",id=1,result=result)))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,"inbox.requeue",dict(jsonrpc="2.0",id=1,result=result))
                result=dict(message=None if case["value"] is None else {**fixtures["mailbox_message"],"state":"claimed","claim_generation":case["value"]},claim_generation=case["value"])
                if case["claim"] or case["value"] is None:self.assertTrue(contract.validate_reply(endpoint,"inbox.next",dict(jsonrpc="2.0",id=1,result=result)))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,"inbox.next",dict(jsonrpc="2.0",id=1,result=result))

    def test_sandbox_raid_and_ui_parameter_contracts(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["sandbox_raid_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("sandbox",case["method"],case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("sandbox",case["method"],case["params"])
        for case in fixtures["raid_owner_requests"]:
            entry=contract.method("host",case["method"])
            contract.schema_validator(entry["x-client-param-profiles"][0]["x-params-schema"]).validate(case["params"])
            if case["canonical"]:self.assertTrue(contract.validate_params("host",case["method"],case["params"]))
            elif case["preview"].get("raid") is not None:
                with self.assertRaises(ValidationError):contract.validate_params("host",case["method"],case["params"])
        for case in fixtures["session_raid_cases"]:
            schema={"$ref":"common.json#/x-client-result-schemas/SessionRaidReply"}
            if case["legacy"]:contract.schema_validator(schema).validate(case["result"])
            else:
                with self.assertRaises(ValidationError):contract.schema_validator(schema).validate(case["result"])
        for case in fixtures["raid_record_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("sandbox","raids.members",reply,compatibility=True))
            if case["current"]:self.assertTrue(contract.validate_reply("sandbox","raids.members",reply))
        self.assertTrue(contract.validate_reply("sandbox","raids.leave",dict(jsonrpc="2.0",id=1,result=dict(accepted=True,dissolved=True,paused=False))))

    def test_host_raid_contracts_and_legacy_display_projection(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["host_raid_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("host",case["method"],case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("host",case["method"],case["params"])
        for case in fixtures["raid_record_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("host","raids.members",reply,compatibility=True))
            if case["current"]:self.assertTrue(contract.validate_reply("host","raids.members",reply))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("host","raids.members",reply)
            # The strict compatible record reader is used by snapshots,
            # separately from the more permissive member display projection.
            schema={"$ref":"common.json#/x-client-result-schemas/RaidRecordReply"}
            if case["legacy"]:contract.schema_validator(schema).validate(case["result"])
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.schema_validator(schema).validate(case["result"])
        for record in fixtures["raid_display_cases"]:
            self.assertTrue(contract.validate_reply("host","raids.members",dict(jsonrpc="2.0",id=1,result=record),compatibility=True))
        for name,result in [("raids.destroy",dict(accepted=True,stopped=[])),("raids.kick",dict(accepted=True,dissolved=False,paused=True))]:
            self.assertTrue(contract.validate_reply("host",name,dict(jsonrpc="2.0",id=1,result=result)))
            result={**result,"accepted":False}
            with self.assertRaises(ValidationError):contract.validate_reply("host",name,dict(jsonrpc="2.0",id=1,result=result))
            self.assertTrue(contract.validate_reply("host",name,dict(jsonrpc="2.0",id=1,result=result),compatibility=True))

    def test_host_job_contracts_and_submission_legacy_reader(self):
        fixtures=contract.read("fixtures.json")
        for endpoint in ("host","sandbox"):
            for case in fixtures["host_job_parameter_cases"]:
                if case["valid"]:self.assertTrue(contract.validate_params(endpoint,case["method"],case["params"]))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params(endpoint,case["method"],case["params"])
            for case in fixtures["host_job_result_cases"]:
                reply=dict(jsonrpc="2.0",id=1,result=case["result"])
                self.assertTrue(contract.validate_reply(endpoint,"host.status",reply,compatibility=True))
                for name,result in [("host.status",case["result"]),("host.list",dict(jobs=[case["result"]]))]:
                    current=dict(jsonrpc="2.0",id=1,result=result)
                    if case["current"]:self.assertTrue(contract.validate_reply(endpoint,name,current))
                    else:
                        with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply(endpoint,name,current)
                for name in ("host.list","host.cancel"):
                    self.assertTrue(contract.validate_reply(endpoint,name,reply,compatibility=True))
            self.assertTrue(contract.validate_reply(endpoint,"host.cancel",dict(jsonrpc="2.0",id=1,result=dict(accepted=True,request="fixture-job-1"))))
        for case in fixtures["host_submitted_cases"]:
            self.assertEqual(contract.validate_notification("sandbox","server-to-client",case["frame"],compatibility=True),case["request"] is not None)
            try:valid=contract.validate_notification("sandbox","server-to-client",case["frame"])
            except ValidationError:valid=False
            self.assertEqual(valid,case["current"],str(case))

    def test_permission_read_contracts_and_legacy_record_forms(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["permission_read_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("host",case["method"],case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("host",case["method"],case["params"])
        for case in fixtures["permission_record_cases"]:
            for current,compatible in [(case["current"],False),(case["legacy"],True)]:
                for name,result in [("permissions.get",case["result"]),("permissions.list",[case["result"]])]:
                    reply=dict(jsonrpc="2.0",id=1,result=result)
                    if current:self.assertTrue(contract.validate_reply("host",name,reply,compatibility=compatible))
                    else:
                        with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("host",name,reply,compatibility=compatible)
        records=fixtures["permission_records"]
        self.assertTrue(contract.validate_reply("host","permissions.list",dict(jsonrpc="2.0",id=1,result=records)))
        for record in records:
            for key in ("approved","preview","message"):
                broken=copy.deepcopy(record);del broken[key]
                with self.assertRaises(ValidationError):contract.validate_reply("host","permissions.get",dict(jsonrpc="2.0",id=1,result=broken))
            if record["kind"] not in ("devshell",):
                broken={**record,"preview":[]}
                with self.assertRaises(ValidationError):contract.validate_reply("host","permissions.get",dict(jsonrpc="2.0",id=1,result=broken))
        # The private refresh payload remains explicitly opaque until C17.
        forwarded={**records[3],"preview":17}
        self.assertTrue(contract.validate_reply("host","permissions.get",dict(jsonrpc="2.0",id=1,result=forwarded)))

    def test_permission_request_complete_contract_and_distinct_client_profiles(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["permission_request_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("sandbox","permissions.request",case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("sandbox","permissions.request",case["params"])
        for case in fixtures["permission_request_result_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("sandbox","permissions.request",reply,compatibility=True))
            for expected,kwargs in [(case["valid"],{}),(case["grant"],dict(compatibility=True,client_profile="grant-cli"))]:
                if expected:self.assertTrue(contract.validate_reply("sandbox","permissions.request",reply,**kwargs))
                else:
                    with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("sandbox","permissions.request",reply,**kwargs)

    def test_optional_nested_host_positional_metadata_is_checked(self):
        entry=copy.deepcopy(contract.method("sandbox","permissions.request"))
        params=contract.resolve(entry["x-params-schema"]);wire=contract.resolve(entry["x-wire-params-schema"])
        contract.check_nested_params(entry,params,wire)
        entry["x-nested-positional-params"][0]["array-items"]=True
        with self.assertRaises((ValueError,KeyError)):contract.check_nested_params(entry,params,wire)
        entry=copy.deepcopy(contract.method("sandbox","permissions.request"));entry["x-nested-positional-params"][0]["minimum"]=3
        with self.assertRaisesRegex(ValueError,"nested positional order"):contract.check_nested_params(entry,params,wire)

    def test_permission_decision_contract_preserves_selectors_and_client_fallbacks(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["permission_decision_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("host","permissions.decide",case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("host","permissions.decide",case["params"])
        for case in fixtures["permission_decision_result_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("host","permissions.decide",reply,compatibility=True))
            if case["valid"]:self.assertTrue(contract.validate_reply("host","permissions.decide",reply))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("host","permissions.decide",reply)

    def test_editor_contract_preserves_nested_parameter_forms_and_precise_results(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["editor_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("sandbox","editor.open",case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("sandbox","editor.open",case["params"])
        for case in fixtures["editor_result_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("sandbox","editor.open",reply,compatibility=True))
            if case["valid"]:self.assertTrue(contract.validate_reply("sandbox","editor.open",reply))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("sandbox","editor.open",reply)

    def test_editor_reference_distinguishes_wire_requests_from_normalized_dtos(self):
        section=contract.reference().split("** =editor.open=",1)[1].split("\n** ",1)[0]
        def block(label):return json.loads(section.split(label+"\n\n#+begin_src json\n",1)[1].split("\n#+end_src",1)[0])
        wire=block("Named parameters:")
        normalized=block("Normalized generated request DTO (after nested positional mapping):")
        request=dict(files=[["/a"]],wait=True)
        contract.schema_validator(wire).validate(request)
        with self.assertRaises(ValidationError):contract.schema_validator(normalized).validate(request)
        contract.schema_validator(normalized).validate(dict(files=[dict(path="/a")],wait=True))
        self.assertNotEqual(wire,normalized)

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

    def test_devshell_diff_and_complete_preview_contracts(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["devshell_diff_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("sandbox","devshell.diff",case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("sandbox","devshell.diff",case["params"])
        preview=fixtures["devshell_preview"]
        validate=lambda value,compatible=False:contract.validate_reply("sandbox","devshell.diff",dict(jsonrpc="2.0",id=1,result=value),compatibility=compatible)
        self.assertTrue(validate(preview));self.assertTrue(validate(fixtures["devshell_preview_unchanged"]))
        self.assertTrue(validate(fixtures["devshell_preview_produced"]))
        self.assertTrue(validate({**preview,"extension":True}))
        for key in preview:
            broken=copy.deepcopy(preview);del broken[key]
            with self.assertRaises(ValidationError,msg=key):validate(broken)
        for path,value in [(('attention',0,'kind'),'future'),(('attention',1,'source'),None),(('files',0,'added'),-1),(('inputs',0,'new','date'),7),(('packages',0,'old'),[1]),(('env',0,'long'),1),(('incoming',0,'action'),'future')]:
            broken=copy.deepcopy(preview);target=broken
            for key in path[:-1]:target=target[key]
            target[path[-1]]=value
            with self.assertRaises(ValidationError,msg=str(path)):validate(broken)
            self.assertTrue(validate(broken,True))
        for incoming in (preview['incoming']*501,):
            with self.assertRaises(ValidationError):validate({**preview,'incoming':incoming})
        for legacy in fixtures["devshell_preview_legacy"]:self.assertTrue(validate(legacy,True))
        for nonobject in (None,[],1,"preview"):
            with self.assertRaises(ValidationError):validate(nonobject,True)

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

    def test_devshell_lock_and_flake_app_contract_corpora(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["devshell_app_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params("sandbox",case["method"],case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params("sandbox",case["method"],case["params"])
        for case in fixtures["devshell_app_result_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            self.assertTrue(contract.validate_reply("sandbox",case["method"],reply,compatibility=True))
            if case["valid"]:self.assertTrue(contract.validate_reply("sandbox",case["method"],reply))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_reply("sandbox",case["method"],reply)

    def test_attachment_notification_and_completion_contract_corpora(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["attach_hello_parameter_cases"]:
            wire=dict(jsonrpc="2.0",method="attach.hello",params=case["params"])
            if case["valid"]:self.assertTrue(contract.validate_notification("attach","client-to-server",wire))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_notification("attach","client-to-server",wire)
        for case in fixtures["attach_waiting_cases"]:
            wire=case["frame"]
            self.assertEqual(contract.validate_notification("attach","server-to-client",wire,compatibility=True),case["has_missing"] is not None)
            try:valid=contract.validate_notification("attach","server-to-client",wire)
            except ValidationError:valid=False
            self.assertEqual(valid,case["valid"],str(case))
        for case in fixtures["attach_completion_cases"]:
            for name in ("attach.reply","legacy.attach"):
                for compatible,expected in ((False,case["valid"]),(True,case["compatible"])):
                    if expected:self.assertTrue(contract.validate_completion(name,case["reply"],compatibility=compatible))
                    else:
                        with self.assertRaises(ValidationError,msg=str(case)):contract.validate_completion(name,case["reply"],compatibility=compatible)
        self.assertEqual(contract.document("attach")["methods"],[])

    def test_attachment_coverage_requires_notification_and_completion_peers(self):
        sources=contract.implementation_sources()
        changed=dict(sources)
        changed["attach_client"]=sources["attach_client"].replace("contract::attach_exchanges::LegacyAttach","contract::attach_exchanges::Unassigned",1)
        self.assertNotEqual(changed["attach_client"],sources["attach_client"])
        with self.assertRaisesRegex(ValueError,"unassigned generated completion"):contract.check_coverage(sources=changed)
        changed=dict(sources)
        changed["attach_server"]=sources["attach_server"]+'\nfn new_notice() { let _ = json!({"jsonrpc":"2.0","method":"attach.new","params":{}}); }\n'
        with self.assertRaisesRegex(ValueError,"notification endpoint"):contract.check_coverage(sources=changed)

    def test_terminal_io_contract_constraints_and_precise_results(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["terminal_io_parameter_cases"]:
            if case["valid"]:self.assertTrue(contract.validate_params(case["endpoint"],case["method"],case["params"]))
            else:
                with self.assertRaises(ValidationError,msg=str(case)):contract.validate_params(case["endpoint"],case["method"],case["params"])
        for case in fixtures["terminal_io_result_cases"]:
            for endpoint in ("host","sandbox") if case["method"] not in ("terminals.detach","terminals.resize") else ("host",):
                reply=dict(jsonrpc="2.0",id=1,result=case["result"])
                self.assertTrue(contract.validate_reply(endpoint,case["method"],reply,compatibility=True))
                if case["valid"]:self.assertTrue(contract.validate_reply(endpoint,case["method"],reply))
                else:
                    with self.assertRaises(ValidationError):contract.validate_reply(endpoint,case["method"],reply)

    def test_terminal_contract_corpora_and_endpoint_parameter_forms(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["terminal_parameter_cases"]:
            entry=contract.method(case["endpoint"],case["method"])
            params={} if case["params"] is None and entry.get("x-null-params-as-empty") else case["params"]
            schema=entry["x-positional-schema"] if isinstance(params,list) else entry["x-params-schema"]
            self.assertEqual(contract.schema_validator(schema).is_valid(params),case["valid"],str(case))
        for case in fixtures["terminal_result_cases"]:
            for endpoint in ("host","sandbox"):
                reply=dict(jsonrpc="2.0",id=1,result=case["result"])
                self.assertTrue(contract.validate_reply(endpoint,case["method"],reply,compatibility=True))
                if case["valid"]:self.assertTrue(contract.validate_reply(endpoint,case["method"],reply))
                else:
                    with self.assertRaises(ValidationError):contract.validate_reply(endpoint,case["method"],reply)

    def test_exec_contract_corpora_and_notification_direction(self):
        fixtures=contract.read("fixtures.json")
        for case in fixtures["exec_parameter_cases"]:
            entry=contract.method("host",case["method"])
            schema=entry["x-positional-schema"] if isinstance(case["params"],list) else entry["x-params-schema"]
            self.assertEqual(contract.schema_validator(schema).is_valid(case["params"]),case["valid"],str(case))
        for case in fixtures["exec_result_cases"]:
            reply=dict(jsonrpc="2.0",id=1,result=case["result"])
            for compatible,expected in ((False,case["valid"]),(True,case["compatible"])):
                if expected:self.assertTrue(contract.validate_reply("host","sessions.exec",reply,compatibility=compatible))
                else:
                    with self.assertRaises(ValidationError):contract.validate_reply("host","sessions.exec",reply,compatibility=compatible)
        for case in fixtures["exec_signal_cases"]:
            notification=dict(jsonrpc="2.0",method="exec.signal",params=case["params"])
            if case["valid"]:self.assertTrue(contract.validate_notification("host","client-to-server",notification))
            else:
                with self.assertRaises(ValidationError):contract.validate_notification("host","client-to-server",notification)
        good=dict(jsonrpc="2.0",method="exec.signal",params=dict(signal=15))
        self.assertFalse(contract.validate_notification("sandbox","client-to-server",good))
        self.assertFalse(contract.validate_notification("host","server-to-client",good))
        with self.assertRaises(ValidationError):contract.validate_notification("host","client-to-server",dict(good,id=1))
        self.assertIsNone(contract.method("host","exec.signal"))

    def test_generated_notification_emitter_and_consumer_require_coverage(self):
        from unittest.mock import patch
        sources=contract.implementation_sources()
        for key in ("exec_client","exec_input"):
            changed=dict(sources)
            changed[key]=sources[key].replace("contract::host_notifications::ExecSignal","contract::host_notifications::UnassignedSignal",1)
            self.assertNotEqual(changed[key],sources[key])
            with self.assertRaisesRegex(ValueError,"unassigned generated notification"):
                contract.check_coverage(sources=changed)
        original=contract.document
        host=copy.deepcopy(original("host"))
        host["x-notifications"][0]["direction"]="server-to-client"
        with patch.object(contract,"document",side_effect=lambda endpoint: host if endpoint=="host" else original(endpoint)):
            with self.assertRaisesRegex(ValueError,"notification endpoint"):
                contract.check_coverage()

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

    def test_self_reporting_schema_matches_parameter_corpus_and_producer(self):
        fixtures = contract.read("fixtures.json")
        for case in fixtures["self_parameter_cases"]:
            entry = contract.method("sandbox", case["method"])
            schema = entry["x-positional-schema"] if isinstance(case["params"], list) else entry["x-params-schema"]
            self.assertEqual(contract.schema_validator(schema).is_valid(case["params"]), case["valid"], case)
        value = fixtures["self_status"]
        self.assertTrue(contract.validate_reply("sandbox", "sessions.status", dict(jsonrpc="2.0", id=1, result=value)))
        for field in value:
            bad = dict(value); del bad[field]
            with self.assertRaises(ValidationError):
                contract.validate_reply("sandbox", "sessions.status", dict(jsonrpc="2.0", id=1, result=bad))
        legacy = dict(jsonrpc="2.0", id=1, result={"agent_name":"goblin", "attached":False})
        self.assertTrue(contract.validate_reply("sandbox", "sessions.status", legacy, compatibility=True))
        with self.assertRaises(ValidationError):
            contract.validate_reply("sandbox", "sessions.status", legacy)

    def test_nested_result_records_require_objects(self):
        for case in contract.read("fixtures.json")["self_result_cases"]:
            schema = {"$ref":"common.json#/definitions/" + case["schema"]}
            self.assertEqual(contract.schema_validator(schema).is_valid(case["result"]), case["valid"], case)

    def test_session_read_schemas_match_corpora_and_keep_producers_precise(self):
        fixtures = contract.read("fixtures.json")
        for endpoint in ("host", "sandbox"):
            for case in fixtures["session_parameter_cases"]:
                entry = contract.method(endpoint, case["method"])
                schema = entry["x-positional-schema"] if isinstance(case["params"], list) else entry["x-params-schema"]
                self.assertEqual(contract.schema_validator(schema).is_valid(case["params"]), case["valid"], case)
            value = fixtures[endpoint + "_session"]
            for name, result in [("sessions.list", [value]), ("sessions.get", value)]:
                self.assertTrue(contract.validate_reply(endpoint, name, dict(jsonrpc="2.0",id=1,result=result)))
            for missing in value:
                bad = dict(value); del bad[missing]
                with self.assertRaises(ValidationError):
                    contract.validate_reply(endpoint, "sessions.get", dict(jsonrpc="2.0",id=1,result=bad))
        for case in fixtures["session_result_cases"]:
            self.assertEqual(contract.schema_validator({"$ref":"common.json#/definitions/"+case["schema"]}).is_valid(case["result"]),case["valid"],case)
        for field, value in [("state","finished"),("agent_state","future-activity"),("terminals",[{**fixtures["host_session"]["terminals"][0], "origin":"remote"}])]:
            result = copy.deepcopy(fixtures["host_session"]); result[field] = value
            reply = dict(jsonrpc="2.0",id=1,result=result)
            self.assertTrue(contract.validate_reply("host","sessions.get",reply,compatibility=True))
            with self.assertRaises(ValidationError): contract.validate_reply("host","sessions.get",reply)
        original = dict(jsonrpc="2.0",id=1,result=fixtures["original_api1"]["host_session"])
        self.assertTrue(contract.validate_reply("host","sessions.get",original,compatibility=True))
        with self.assertRaises(ValidationError): contract.validate_reply("host","sessions.get",original)
        reply = dict(jsonrpc="2.0",id=1,result=fixtures["host_session_legacy"])
        self.assertTrue(contract.validate_reply("host","sessions.get",reply,compatibility=True))
        with self.assertRaises(ValidationError): contract.validate_reply("host","sessions.get",reply)
        self.assertEqual(set(fixtures["sandbox_session"]),set(contract.common()["definitions"]["SandboxSession"]["properties"]))
        for hidden in ("configuration","identity","cwd","checkout","terminal","attach","detail","server_log"):
            self.assertNotIn(hidden, fixtures["sandbox_session"])
        self.assertEqual(fixtures["sandbox_session"]["worktree"],fixtures["host_session"]["worktree"])

    def test_lifecycle_contracts_preserve_parameter_order_and_current_producers(self):
        fixtures = contract.read("fixtures.json")
        for case in fixtures["lifecycle_parameter_cases"]:
            entry = contract.method(case["endpoint"],case["method"])
            schema = entry["x-positional-schema"] if isinstance(case["params"],list) else entry["x-params-schema"]
            self.assertEqual(contract.schema_validator(schema).is_valid(case["params"]),case["valid"],case)
        for endpoint,name,key in [("host","sessions.start","host_launch"),("sandbox","sessions.start","member_launch"),("sandbox","targets.list","run_targets")]:
            for result in [fixtures[key],fixtures["forward_compatibility"][key]]:
                self.assertTrue(contract.validate_reply(endpoint,name,dict(jsonrpc="2.0",id=1,result=result)))
        for case in fixtures["lifecycle_result_cases"]:
            self.assertEqual(contract.schema_validator({"$ref":"common.json#/definitions/"+case["schema"]}).is_valid(case["result"]),case["valid"],case)
        legacy=dict(jsonrpc="2.0",id=1,result=fixtures["original_api1"]["host_launch"])
        self.assertTrue(contract.validate_reply("host","sessions.start",legacy,compatibility=True))
        with self.assertRaises(ValidationError): contract.validate_reply("host","sessions.start",legacy)
        for key,endpoint in [("host_launch","host"),("member_launch","sandbox")]:
            bad=copy.deepcopy(fixtures[key]);bad["state"]="running"
            with self.assertRaises(ValidationError): contract.validate_reply(endpoint,"sessions.start",dict(jsonrpc="2.0",id=1,result=bad))

    def test_claude_mod_message_projection_uses_contract_linked_raw_results(self):
        source=(contract.ROOT / "nix/goblins/claude/mod/hooks/register.tsx").read_text()
        function=source[source.index("function toMessage("):source.index("// Reads a message through")]
        function=function.replace("function toMessage(raw: any): GoblinsMessage | undefined", "function toMessage(raw)")
        cases=contract.read("fixtures.json")["claude_message_boundary"]
        program=function+"\nconst cases=JSON.parse(process.argv[1]);process.stdout.write(JSON.stringify(cases.map(c=>toMessage(c.raw)??null)));"
        result=subprocess.run(["node","--input-type=module","-e",program,json.dumps(cases)],capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(json.loads(result.stdout),[case["view"] for case in cases])
        fixture=contract.read("fixtures.json")["mailbox_message"]
        self.assertTrue(contract.validate_reply("sandbox","messages.get",dict(jsonrpc="2.0",id=1,result=fixture)))

    def test_claude_shell_message_helper_preserves_raw_results_and_exact_requests(self):
        fixtures=contract.read("fixtures.json")
        script=contract.ROOT / "nix/goblins/claude/rpc.sh"
        plugin=(contract.ROOT / "nix/goblins/claude/plugin.nix").read_text()
        self.assertIn("text = builtins.readFile ./rpc.sh;",plugin)
        shim=r'''#!/usr/bin/env python3
import sys,json,os
sys.stderr=open(os.environ["TRACE"],"w")
for index in range(2):
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if line in (b"\r\n",b"\n"):break
        if not line:sys.exit(2)
        key,value=line.decode().split(":",1);headers[key.lower()]=value.strip()
    call=json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    with open(os.environ["CALLS"],"a") as out:out.write(json.dumps(call)+"\n")
    result=json.loads(os.environ["INIT"] if index==0 else os.environ["RESULT"])
    body=json.dumps(dict(jsonrpc="2.0",id=call["id"],result=result)).encode()
    sys.stdout.buffer.write(b"Content-Length: "+str(len(body)).encode()+b"\r\n\r\n"+body);sys.stdout.buffer.flush()
sys.stdin.buffer.read()
'''
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);fake=root/"socat";fake.write_text(shim.replace("#!/usr/bin/env python3","#!"+sys.executable,1));fake.chmod(0o755)
            for case in fixtures["claude_message_boundary"]:
                calls=root/"calls";calls.write_text("")
                env={**os.environ,"PATH":str(root)+os.pathsep+os.environ["PATH"],"CALLS":str(calls),"TRACE":str(root/"trace"),"INIT":json.dumps(fixtures["sandbox_initialize"]),"RESULT":json.dumps(case["raw"])}
                result=subprocess.run(["bash","-euo","pipefail",str(script),"i-m1"],capture_output=True,text=True,env=env,timeout=15)
                self.assertEqual(result.returncode,0,result.stderr+((root/"trace").read_text() if (root/"trace").exists() else "transport shim did not start"))
                self.assertEqual(json.loads(result.stdout),case["raw"])
                sent=[json.loads(line) for line in calls.read_text().splitlines()]
                self.assertEqual([call["method"] for call in sent],["initialize","messages.get"])
                self.assertEqual(sent[1]["params"],dict(message="i-m1"))
                self.assertTrue(contract.validate_params("sandbox","messages.get",sent[1]["params"]))

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
        wrong["pending"]["C99"] = {"sandbox":["server.status"]}
        with self.assertRaisesRegex(ValueError, "sandbox coverage"):
            contract.check_coverage(wrong)
        wrong = copy.deepcopy(contract.read("coverage.json"))
        wrong["notifications"].append(dict(endpoint="host",name="host.submitted",direction="server-to-client",chunk="C10"))
        with self.assertRaisesRegex(ValueError, "notification endpoint"):
            contract.check_coverage(wrong)

    def test_coverage_discovers_or_patterns_and_matches_allowlists(self):
        sources = contract.implementation_sources()
        changed = dict(sources)
        before = 'contract::host::HostCancel::NAME'
        changed["controller"] = sources["controller"].replace(before, before + ' | "host.new"', 1)
        self.assertNotEqual(changed["controller"], sources["controller"])
        with self.assertRaisesRegex(ValueError, "host coverage"):
            contract.check_coverage(sources=changed)
        changed = dict(sources)
        before = 'contract::sandbox::PermissionsRequest::NAME\n            | contract::sandbox::FlakeRun::NAME'
        changed["gate"] = sources["gate"].replace(before, '"sandbox.new" | ' + before, 1)
        self.assertNotEqual(changed["gate"], sources["gate"])
        with self.assertRaisesRegex(ValueError, "gate/controller disagree"):
            contract.check_coverage(sources=changed)

    def test_singleton_matches_guard_requires_an_endpoint_assignment(self):
        sources = contract.implementation_sources()
        changed = dict(sources)
        before = 'contract::host::HostCancel::NAME'
        changed["controller"] = sources["controller"].replace(
            before, 'm if matches!(m, "server.new") => Ok(json!({})),\n            ' + before, 1)
        self.assertNotEqual(changed["controller"], sources["controller"])
        with self.assertRaisesRegex(ValueError, "host coverage"):
            contract.check_coverage(sources=changed)
        assigned = copy.deepcopy(contract.read("coverage.json"))
        assigned["pending"]["C99"] = {"host":["server.new"]}
        with self.assertRaises(ValueError):
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
        with self.assertRaises(ValueError):
            contract.check_coverage(assigned, changed)


class HistoricalClientTests(unittest.TestCase):
    def test_attachment_cli_preserves_exchanges_and_raw_switch(self):
        from terminal_support import Terminal
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        fixtures=contract.read("fixtures.json")
        waiting=[dict(jsonrpc="2.0",method="attach.waiting",params=dict(missing=[])),
                 dict(jsonrpc="old",id=None,method="attach.waiting",params=dict(missing=[7])),
                 dict(method="attach.waiting",params=dict(missing="bad"))]
        success=dict(jsonrpc="2.0",id=0,result=dict(attached=True,future=1))
        cases=[(True,waiting,success,None),(False,[],success,None),
               (True,[],dict(jsonrpc="2.0",id=1,result=dict(attached=True)),"invalid correlated response"),
               (True,[],dict(jsonrpc="2.0",id=0,result=dict(attached=False)),"invalid terminal handshake"),
               (False,[],dict(jsonrpc="2.0",id=0,error=dict(code=-32602,message="old refusal")),"old refusal")]
        payload=b"RAW_C06\nContent-Length: 2\r\n\r\n{}\n"
        for modern,notices,completion,error in cases:
            with self.subTest(modern=modern,completion=completion),tempfile.TemporaryDirectory(prefix="gc-attach-") as root:
                state=Path(root);errors=[];calls=[];hellos=[];inputs=[];ready=threading.Event()
                init=copy.deepcopy(fixtures["host_initialize"])
                if modern:init["features"].append("attach-handlers")
                record=copy.deepcopy(fixtures["original_api1"]["host_session"])
                record.update(attach=str(state/"attach.sock"),terminal=str(state/"terminal.sock"),state="stopped",pty_eof=True,terminal_complete=True,terminal_interrupted=False,terminal_detached=False,exit_code=0)
                output=state/"output";output.touch()
                with socket.socket(socket.AF_UNIX) as host,socket.socket(socket.AF_UNIX) as terminal:
                    host.settimeout(5);host.bind(str(state/"host.sock"));host.listen()
                    terminal.settimeout(5);terminal.bind(record["attach"] if modern else record["terminal"]);terminal.listen()
                    def control():
                        try:
                            peer,_=host.accept()
                            with peer:
                                peer.settimeout(5);request=receive(peer)
                                if not ready.wait(timeout=5):raise AssertionError("frontend window not configured")
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=init)))
                                while True:
                                    try:request=receive(peer)
                                    except EOFError:break
                                    calls.append(request)
                                    if request["method"]=="sessions.get":result=record
                                    elif request["method"]=="sessions.resize":result=dict(accepted=True)
                                    else:raise AssertionError(request)
                                    peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=result)))
                        except Exception as failure:errors.append(failure)
                    def stream():
                        try:
                            peer,_=terminal.accept()
                            with peer:
                                peer.settimeout(5)
                                if modern:hellos.append(receive(peer))
                                peer.sendall(b"".join(frame(n) for n in notices)+frame(completion)+payload)
                                peer.shutdown(socket.SHUT_WR)
                                try:
                                    while data:=peer.recv(4096):inputs.append(data)
                                except ConnectionResetError:
                                    if error is None:raise
                        except Exception as failure:errors.append(failure)
                    threads=[threading.Thread(target=f,daemon=True) for f in (control,stream)]
                    started=[]
                    client=Terminal([binary,"--state-dir",str(state),"attach",record["id"]],output=str(output))
                    try:
                        for thread in threads:thread.start();started.append(thread)
                        ready.set()
                        code=client.wait()
                        self.assertEqual(code==0,error is None,client.buffer)
                        if error:self.assertIn(error.encode(),client.buffer)
                        self.assertEqual(client.buffer.count(b"Starting"),1 if notices else 0)
                        self.assertEqual(output.read_bytes(),payload if error is None else b"")
                    finally:
                        client.close();ready.set()
                        for thread in started:thread.join(timeout=6)
                        self.assertFalse(any(thread.is_alive() for thread in started))
                    self.assertEqual(errors,[]);self.assertEqual(inputs,[])
                    if modern:
                        self.assertEqual(hellos,[dict(jsonrpc="2.0",method="attach.hello",params=dict(version=1,rows=24,cols=100))])
                    else:self.assertEqual(hellos,[])
                    self.assertTrue(calls);self.assertEqual(calls[0]["method"],"sessions.get")

    def terminal_io_cli_cases(self, member=False):
        target=["t1"] if member else ["a","t1"]
        base={} if member else dict(session="a")
        base["terminal"]="t1"
        cases=[
            (["read",*target],"read",base,{"text":"hello"},0,"hello\n",""),
            (["read",*target,"--source","recent","--lines","7"],"read",dict(base,source="recent",lines=7),None,0,"\n",""),
            (["read",*target],"read",base,{"text":7},0,"\n",""),
            (["send",*target,"--text",""],"send",dict(base,text="",keys=[]),[],0,"",""),
            (["send",*target,"--keys","g","A","ctrl+c"],"send",dict(base,keys=["g","A","ctrl+c"]),None,0,"",""),
            (["run",*target,"echo hi"],"send",dict(base,text="echo hi",keys=["enter"]),[],0,"",""),
            (["wait",*target,"--match","done","--timeout","0","--lines","5"],"wait",dict(base,match="done",timeout=0,lines=5),{"status":"matched","line":7},0,"\n",""),
            (["wait",*target,"--regex","^done$"],"wait",dict(base,regex="^done$"),{"status":"matched","line":"done","future":True},0,"done\n",""),
            (["wait",*target,"--exit"],"wait",dict(base,exit=True),{"status":"exited","code":"bad","signal":15,"text":7},0,"\n","t1 exited from signal 15\n"),
            (["wait",*target,"--exit"],"wait",dict(base,exit=True),{"status":"exited","code":7,"signal":"bad","text":"last"},0,"last\n","t1 exited with code 7\n"),
            (["wait",*target,"--match","done"],"wait",dict(base,match="done"),{"status":"future"},1,"","timed out waiting for t1\n"),
            (["wait",*target,"--match","done"],"wait",dict(base,match="done"),None,1,"","timed out waiting for t1\n"),
        ]
        if not member:cases.append((["detach",*target],"detach",base,[],0,"",""))
        return cases

    def test_terminal_io_host_cli_preserves_requests_and_legacy_reports(self):
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        init=copy.deepcopy(contract.read("fixtures.json")["host_initialize"]);init["features"].append("terminals")
        for args,method,params,result,code,output,error in self.terminal_io_cli_cases():
            with self.subTest(args=args,result=result),tempfile.TemporaryDirectory(prefix="gc-terminal-io-") as root:
                state=Path(root);calls=[];errors=[]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5);listener.bind(str(state/"host.sock"));listener.listen()
                    def serve():
                        try:
                            peer,_=listener.accept()
                            with peer:
                                peer.settimeout(5);request=receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=init)))
                                call=receive(peer);calls.append(call)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
                        except Exception as failure:errors.append(failure)
                    server=threading.Thread(target=serve,daemon=True);server.start()
                    cli=subprocess.run([binary,"--state-dir",str(state),"terminal",*args],text=True,capture_output=True,timeout=10)
                    server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                    self.assertEqual(cli.returncode,code,cli.stderr);self.assertEqual((cli.stdout,cli.stderr),(output,error))
                    self.assertEqual((calls[0]["method"],calls[0]["params"]),("terminals."+method,params))

    def test_terminal_io_sandbox_cli_preserves_requests_and_legacy_reports(self):
        for args,method,params,result,code,output,error in self.terminal_io_cli_cases(member=True):
            with self.subTest(args=args,result=result):
                cli,call=self.sandbox_fake_cli(["terminal",*args],result,features=("terminals",))
                self.assertEqual(cli.returncode,code,cli.stderr);self.assertEqual((cli.stdout,cli.stderr),(output,error))
                self.assertEqual((call["method"],call["params"]),("terminals."+method,params))

    def test_terminal_cli_binds_lifecycle_contracts_and_preserves_display_fallbacks(self):
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        fixtures=contract.read("fixtures.json");init=copy.deepcopy(fixtures["host_initialize"]);init["features"].append("terminals")
        record=fixtures["terminal_record"]
        cases=[
            (["create","a"],{"session":"a","env":[]},record,"t1\n"),
            (["create","a","--name","app","--cwd","/work","--env","A=b","--size","30x90","--","sh"],{"session":"a","env":[["A","b"]],"name":"app","cwd":"/work","rows":30,"cols":90,"argv":["sh"]},{"id":7},"?\n"),
            (["create","a"],{"session":"a","env":[]},None,"?\n"),
            (["list","a"],{"session":"a"},{"terminals":[record]},"t1\t-\thost\t40x120\tdetached\tshell\n"),
            (["list","a"],{"session":"a"},{"terminals":[{"id":7,"name":"odd","origin":"remote","rows":"wide","cols":None,"attached":"false","label":"bad"},None]},'-\todd\tremote\t"wide"xnull\tdetached\tbad\n-\t-\t-\tnullxnull\tdetached\t-\n'),
            (["list","a"],{"session":"a"},None,""),
            (["close","a","app"],{"session":"a","terminal":"app"},[],""),
        ]
        for args,params,result,expected in cases:
            with self.subTest(args=args,result=result),tempfile.TemporaryDirectory(prefix="gc-terminal-") as root:
                state=Path(root);errors=[];calls=[]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5);listener.bind(str(state/"host.sock"));listener.listen()
                    def serve():
                        try:
                            peer,_=listener.accept()
                            with peer:
                                peer.settimeout(5);request=receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=init)))
                                call=receive(peer);calls.append(call)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
                        except Exception as error:errors.append(error)
                    server=threading.Thread(target=serve,daemon=True);server.start()
                    cli=subprocess.run([binary,"--state-dir",str(state),"terminal",*args],text=True,capture_output=True,timeout=10)
                    server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                    self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(cli.stdout,expected);self.assertEqual(cli.stderr,"")
                    self.assertEqual(calls[0]["method"],"terminals."+args[0]);self.assertEqual(calls[0]["params"],params)

    def test_exec_cli_uses_generated_params_and_passes_stdio_outside_json(self):
        import array
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        init=copy.deepcopy(contract.read("fixtures.json")["host_initialize"])
        init["features"].append("exec")
        cases=[(False,False,{"code":7,"signal":None,"future":True},7),
               (False,True,{"code":127,"message":"fixture missing"},127),
               (True,False,{"code":None,"signal":15},143),
               (False,False,{},255),
               (False,False,{"code":0,"signal":"bad"},0),
               (False,False,{"code":"bad","signal":15},143),
               (False,False,{"code":3,"message":42},3),
               (False,False,{"code":True,"signal":15},143),
               (False,False,None,255)]
        for tty,named,result,expected_code in cases:
            with self.subTest(tty=tty,named=named,result=result),tempfile.TemporaryDirectory(prefix="gc-exec-") as root:
                state=Path(root);calls=[];errors=[]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5);listener.bind(str(state/"host.sock"));listener.listen()
                    def serve():
                        fds=[]
                        try:
                            peer,_=listener.accept()
                            with peer:
                                peer.settimeout(5);request=receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=init)))
                                first,ancillary,flags,_=peer.recvmsg(1,socket.CMSG_SPACE(3*array.array("i").itemsize))
                                for level,kind,data in ancillary:
                                    self.assertEqual((level,kind),(socket.SOL_SOCKET,socket.SCM_RIGHTS))
                                    values=array.array("i");values.frombytes(data);fds.extend(values)
                                self.assertFalse(flags & socket.MSG_CTRUNC)
                                class Prefix:
                                    def recv(self,size):
                                        nonlocal first
                                        if first: value,first=first,b"";return value
                                        return peer.recv(size)
                                call=receive(Prefix());calls.append(call)
                                self.assertEqual(len(fds),1 if tty else 3)
                                os.write(fds[0] if tty else fds[1],b"fixture output\n")
                                peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
                        except Exception as error:errors.append(error)
                        finally:
                            for fd in fds:os.close(fd)
                    server=threading.Thread(target=serve,daemon=True);server.start()
                    args=[binary,"--state-dir",str(state),"exec"]
                    if tty:args.append("-t")
                    if named:args.extend(["--session","pinned","--workdir","/work","-e","A=b"])
                    args.append("agent")
                    if named:args.extend(["--","echo","hello"])
                    cli=subprocess.run(args,stdin=subprocess.DEVNULL,text=True,capture_output=True,timeout=10)
                    server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                    self.assertEqual(cli.returncode,expected_code,cli.stderr)
                    if "message" not in (result or {}) or not isinstance(result["message"],str):
                        self.assertEqual(cli.stderr,"")
                    self.assertIn("fixture output",cli.stdout)
                    self.assertEqual(calls[0]["method"],"sessions.exec")
                    expected={"session":"agent","env":[["A","b"]] if named else [],"tty":tty,"interactive":tty}
                    if named:expected.update(pin="pinned",cwd="/work",argv=["echo","hello"])
                    self.assertEqual(calls[0]["params"],expected)

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

    def test_real_list_cli_validates_records_and_preserves_wire_json(self):
        binary = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary = re.search(r'exec (\S+)', app().read_text()).group(1)
        fixtures = contract.read("fixtures.json")
        good = [fixtures["host_session_legacy"], fixtures["forward_compatibility"]["host_session"]]
        future = copy.deepcopy(fixtures["host_session"]); future["state"]="finished"; future["agent_state"]="future-activity"; future["terminals"][0]["origin"]="remote"; good.append(future)
        broken = copy.deepcopy(good); broken[0]["identity"] = [42, 1, 2, 3, 4]
        for records, valid in [(good, True), ([fixtures["original_api1"]["host_session"]], True), (broken, False)]:
            with tempfile.TemporaryDirectory(prefix="gc-list-") as root:
                state = Path(root) / "state"; state.mkdir(mode=0o700)
                errors, calls = [], []
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5); listener.bind(str(state / "host.sock")); listener.listen()
                    def serve():
                        try:
                            peer, _ = listener.accept()
                            with peer:
                                peer.settimeout(5)
                                init = receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=init["id"],result=fixtures["original_api1"]["host_initialize"])))
                                request = receive(peer); calls.append(request)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=records)))
                        except Exception as error: errors.append(error)
                    server = threading.Thread(target=serve,daemon=True); server.start()
                    result = subprocess.run([binary,"--state-dir",str(state),"list"],text=True,capture_output=True,timeout=10)
                    server.join(timeout=6)
                    self.assertFalse(server.is_alive()); self.assertEqual(errors, [])
                    self.assertEqual(calls[0]["method"],"sessions.list"); self.assertEqual(calls[0]["params"],{})
                    if valid:
                        self.assertEqual(result.returncode,0,result.stderr)
                        self.assertEqual(json.loads(result.stdout), records)
                    else: self.assertNotEqual(result.returncode,0)

    def test_real_attach_cli_accepts_original_api1_record_without_agent_name(self):
        from terminal_support import Terminal
        binary = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary = re.search(r'exec (\S+)', app().read_text()).group(1)
        original = contract.read("fixtures.json")["original_api1"]
        errors, calls = [], []
        with tempfile.TemporaryDirectory(prefix="gc-original-") as root:
            state = Path(root) / "state"; state.mkdir(mode=0o700)
            record = copy.deepcopy(original["host_session"])
            record.update(terminal=str(state / "terminal.sock"), state="stopped", pty_eof=True, terminal_complete=True, exit_code=0)
            with socket.socket(socket.AF_UNIX) as host, socket.socket(socket.AF_UNIX) as terminal:
                host.settimeout(5); host.bind(str(state / "host.sock")); host.listen()
                terminal.settimeout(5); terminal.bind(record["terminal"]); terminal.listen()
                def control():
                    try:
                        peer, _ = host.accept()
                        with peer:
                            peer.settimeout(5)
                            init = receive(peer)
                            peer.sendall(frame(dict(jsonrpc="2.0",id=init["id"],result=original["host_initialize"])))
                            while True:
                                try: request = receive(peer)
                                except EOFError: break
                                calls.append(request)
                                if request["method"] == "sessions.get": result = record
                                elif request["method"] == "sessions.resize": result = {"accepted":True}
                                else: raise AssertionError(request)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=request["id"],result=result)))
                    except Exception as error: errors.append(error)
                def output():
                    try:
                        peer, _ = terminal.accept()
                        with peer:
                            peer.sendall(frame(dict(jsonrpc="2.0",id=0,result={"attached":True})) + b"original API-1 terminal output\n")
                            peer.shutdown(socket.SHUT_WR)
                            peer.settimeout(5)
                            while peer.recv(1024): pass
                    except Exception as error: errors.append(error)
                threads = [threading.Thread(target=f,daemon=True) for f in (control,output)]
                for thread in threads: thread.start()
                client = Terminal([binary,"--state-dir",str(state),"attach",record["id"]])
                try:
                    client.expect("original API-1 terminal output",timeout=10)
                    self.assertEqual(client.wait(),0)
                finally: client.close()
                for thread in threads:
                    thread.join(timeout=6); self.assertFalse(thread.is_alive())
                self.assertEqual(errors,[])
                reads = [c for c in calls if c["method"] == "sessions.get"]
                self.assertEqual(len(reads),2)
                self.assertTrue(all(c["params"] == {"session":record["id"]} for c in reads))

    def test_lifecycle_cli_binds_methods_and_preserves_ack_extensions(self):
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        fixture=contract.read("fixtures.json")
        for command,method in [("stop","sessions.stop"),("detach","sessions.detach")]:
            with tempfile.TemporaryDirectory(prefix="gc-lifecycle-") as root:
                state=Path(root)/"state";state.mkdir(mode=0o700)
                result={"accepted":True,"future":{"kept":True}}; errors=[];calls=[]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5);listener.bind(str(state/"host.sock"));listener.listen()
                    def serve():
                        try:
                            peer,_=listener.accept()
                            with peer:
                                peer.settimeout(5);init=receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=init["id"],result=fixture["historical_api1"]["host_initialize"])))
                                call=receive(peer);calls.append(call)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
                        except Exception as error: errors.append(error)
                    server=threading.Thread(target=serve,daemon=True);server.start()
                    cli=subprocess.run([binary,"--state-dir",str(state),command,"goblin"],text=True,capture_output=True,timeout=10)
                    server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                    self.assertEqual(calls[0]["method"],method);self.assertEqual(calls[0]["params"],{"session":"goblin"})
                    self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(json.loads(cli.stdout),result)

    def test_run_and_resume_omit_options_for_strict_older_daemons(self):
        binary=os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if binary is None:
            import re
            binary=re.search(r'exec (\S+)',app().read_text()).group(1)
        fixtures=contract.read("fixtures.json")
        legacy=contract.schema_validator(fixtures["pre_launch_options_api1"]["host_start_params"])
        for command in ("run","resume"):
            with tempfile.TemporaryDirectory(prefix="gc-launch-") as root:
                root=Path(root);state=root/"state";state.mkdir(mode=0o700)
                manifest=root/"manifest.json"
                manifest.write_text(json.dumps({"api":2,"helper_api":6,"goblins":{"shell":{"build_spec":"/unused","args":[],"env":{},"client_package":"/unused","helper":"/unused","flake":"unused"}}}))
                result=fixtures["forward_compatibility"]["host_launch"];errors=[];calls=[]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.settimeout(5);listener.bind(str(state/"host.sock"));listener.listen()
                    def serve():
                        try:
                            peer,_=listener.accept()
                            with peer:
                                peer.settimeout(5);init=receive(peer)
                                peer.sendall(frame(dict(jsonrpc="2.0",id=init["id"],result=fixtures["historical_api1"]["host_initialize"])))
                                call=receive(peer);calls.append(call)
                                try: legacy.validate(call["params"])
                                except ValidationError as error:
                                    peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],error=dict(code=-32602,message=str(error)))))
                                else: peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
                        except Exception as error: errors.append(error)
                    server=threading.Thread(target=serve,daemon=True);server.start()
                    argument="shell" if command=="run" else "record-id"
                    cli=subprocess.run([binary,"--runtime",str(manifest),"--state-dir",str(state),command,argument,"--detached"],text=True,capture_output=True,timeout=10)
                    server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                    self.assertEqual(calls[0]["method"],"sessions.start")
                    expected={"key","configuration","rows","cols","detached"}
                    expected |= {"name","agent_name","scope","dev_shell","cwd"} if command=="run" else {"recreate"}
                    self.assertEqual(set(calls[0]["params"]),expected)
                    if command=="run":
                        for field in ("agent_name","scope","dev_shell"): self.assertIsNone(calls[0]["params"][field])
                    self.assertNotIn("options",calls[0]["params"])
                    self.assertEqual(calls[0]["params"]["configuration"],str(manifest))
                    self.assertEqual(calls[0]["params"].get("recreate"),None if command=="run" else "record-id")
                    self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(json.loads(cli.stdout),result)

    def test_permission_grant_cli_preserves_requests_closed_results_and_unknown_outcomes(self):
        good=dict(request="fixture-job-1",status="ready",message=None)
        for args,params in [(["request-package","hello","--reason","tests"],dict(kind="package",package="hello",reason="tests")),(["enable-docker","--reason","tests"],dict(kind="docker",reason="tests")),(["devshell","refresh","--reason","tests","--update","nixpkgs"],dict(kind="devshell",reason="tests",update=["nixpkgs"]))]:
            # Denial avoids a follow-up devshell restore/hook exchange.
            result={**good,"status":"denied","message":"no"}
            cli,call=self.sandbox_fake_cli(args,result,features=("docker-enable",))
            self.assertEqual(call["method"],"permissions.request");self.assertEqual(call["params"],params)
            self.assertEqual(cli.returncode,1,cli.stderr);self.assertEqual(json.loads(cli.stdout),result)
        for result in [good,{k:v for k,v in good.items() if k!="message"},["fixture-job-1","ready",None],dict(good,status="denied"),dict(good,status="error"),dict(good,status="failed"),dict(good,status="withdrawn"),dict(good,extra=True),["fixture-job-1","ready"],dict(good,request="bad id"),None,dict(good,message=7)]:
            cli,call=self.sandbox_fake_cli(["request-package","hello"],result)
            self.assertEqual(call["method"],"permissions.request")
            try:
                contract.validate_reply("sandbox","permissions.request",dict(jsonrpc="2.0",id=1,result=result),compatibility=True,client_profile="grant-cli")
                accepted=True
            except ValidationError:accepted=False
            if accepted:
                expected=contract.normalize_reply("permission-grant-fields",result)
                self.assertEqual(cli.returncode,0 if expected["status"]=="ready" else 1,cli.stderr)
                self.assertEqual(json.loads(cli.stdout),expected)
            else:
                self.assertEqual(cli.returncode,2,cli.stderr);self.assertEqual(cli.stdout,"")
                self.assertIn("outcome unknown; do not retry automatically",cli.stderr)

        error=dict(code=-32009,message="blocked",data={"preserve":True})
        cli,_=self.sandbox_fake_cli(["request-package","hello"],None,rpc_error=error)
        self.assertEqual(cli.returncode,1,cli.stderr)
        self.assertEqual(json.loads(cli.stdout),dict(status="error",message=error))

    def test_host_request_cli_preserves_command_omission_and_result_fallbacks(self):
        with tempfile.TemporaryDirectory(prefix="gc-host-cwd-") as cwd:
            args=["host","run","--name","Tests","--reason","tests","--cwd","sub","--env","A=b","--timeout","3","--","true"]
            result=dict(request="fixture-job-1",status="finished",message="exit code 0",code=0)
            cli,call=self.sandbox_fake_cli(args,result,cwd=cwd,features=("host-commands",))
            self.assertEqual(call["method"],"permissions.request")
            self.assertEqual(call["params"],dict(kind="host-command",reason="tests",host=dict(argv=["true"],cwd=str(Path(cwd)/"sub"),env=[["A","b"]],timeout=3,name="Tests")))
            self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(cli.stderr,"")
            for result in [dict(request="fixture-job-1",status="pending",extra=1),None,[],dict(weird=True)]:
                cli,call=self.sandbox_fake_cli(["host","run","--name","Tests","--reason","tests","--detach","--","true"],result,cwd=cwd,features=("host-commands",))
                self.assertTrue(call["params"]["host"]["detach"]);self.assertEqual(call["params"]["host"]["env"],[])
                self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(json.loads(cli.stdout),result)
            for result,code,quiet in [(dict(status="finished",code=0),0,True),(dict(status="finished",code=0.0),125,False),(dict(status="finished",code=2**32),0,False),(dict(status="cancelled",code=143,message="x\x1b"),143,False),(dict(status="failed",code=127),127,False),(dict(status="future",code=0),125,False),(None,125,False)]:
                cli,_=self.sandbox_fake_cli(["host","run","--name","Tests","--reason","tests","--","true"],result,cwd=cwd,features=("host-commands",))
                self.assertEqual(cli.returncode,code,cli.stderr)
                self.assertEqual(cli.stderr=="",quiet)
                self.assertNotIn("\x1b",cli.stderr)

        cli,_=self.sandbox_fake_cli(["host","run","--name","Tests","--reason","tests","--","true"],None,features=("host-commands",),rpc_error=dict(code=-32009,message="refused"))
        self.assertEqual(cli.returncode,125,cli.stderr);self.assertIn("host command refused: refused",cli.stderr)

    def test_sandbox_editor_cli_preserves_paths_params_and_legacy_results(self):
        with tempfile.TemporaryDirectory(prefix="gc-editor-") as root:
            cwd=Path(root)/"work";cwd.mkdir()
            cli,call=self.sandbox_fake_cli(["editor","open","--no-wait","+3:4","../other/./file","/tmp/msg","+9","last"],dict(code=0),cwd=cwd,features=("editor-open",))
            self.assertEqual(cli.returncode,0,cli.stderr)
            self.assertEqual(call["method"],"editor.open")
            self.assertEqual(call["params"],dict(files=[dict(path=str(Path(root)/"other/file"),line=3,column=4),dict(path="/tmp/msg"),dict(path=str(cwd/"last"),line=9)],wait=False))
        for result,code,message in [(None,1,None),([],1,None),({},1,None),(dict(code=True,message=1),1,None),(dict(code=1.0),1,None),(dict(code=4294967296),0,None),(dict(code=2**63-1),255,None),(dict(code=7,message="refused\x1b[2J"),7,"refused\\u001b[2J")]:
            cli,call=self.sandbox_fake_cli(["editor","open","/tmp/msg"],result,features=("editor-open",))
            self.assertEqual(cli.returncode,code,cli.stderr);self.assertEqual(call["params"],dict(files=[dict(path="/tmp/msg")],wait=True))
            if message:self.assertIn(message,cli.stderr)
            self.assertNotIn("\x1b",cli.stderr)

    def test_sandbox_devshell_diff_uses_generated_contract_and_legacy_display(self):
        fixtures=contract.read("fixtures.json")
        for args,params in [([],{}),(["--lock"],dict(lock=True)),(["--update"],dict(update=[])),(["--update","nixpkgs","a/b"],dict(update=["nixpkgs","a/b"]))]:
            cli,call=self.sandbox_fake_cli(["devshell","diff",*args],fixtures["devshell_preview"],features=("devshell-diff",))
            self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(call["method"],"devshell.diff");self.assertEqual(call["params"],params)
            for expected in ("generation 1 → 2","hello 2.12 → 2.13","image.png +? -?","long value; see source diff","1.5 MiB"):
                self.assertIn(expected,cli.stdout)
        cli,_=self.sandbox_fake_cli(["devshell","diff"],fixtures["devshell_preview_legacy"][1],features=("devshell-diff",))
        self.assertEqual(cli.returncode,0,cli.stderr)
        for expected in ("generation ? → ?","? derivation(s) build on this machine","x.nix +? -?","?, ?, ?, 1 →","? to fetch · ? to build"):
            self.assertIn(expected,cli.stdout)
        self.assertNotIn("\x1b",cli.stdout);self.assertNotIn("\u202e",cli.stdout)
        for nonobject in (None,[],1):
            cli,_=self.sandbox_fake_cli(["devshell","diff"],nonobject,features=("devshell-diff",))
            self.assertNotEqual(cli.returncode,0);self.assertIn("daemon returned an invalid preview",cli.stderr)

    def test_sandbox_trusted_lock_and_flake_cli_preserve_contracts_and_fallbacks(self):
        import shutil
        with tempfile.TemporaryDirectory(prefix="gc-lock-") as root:
            path=Path(root)/"flake.lock"
            for result,generation,lock in [
                (dict(path=str(path),generation=3,lock={"nodes":{}}),3,{"nodes":{}}),
                (dict(path=str(path),generation="bad"),0,None),
                (dict(path=str(path),generation=2**64-1,lock=["opaque"]),2**64-1,["opaque"]),
            ]:
                cli,call=self.sandbox_fake_cli(["devshell","restore-lock"],result)
                self.assertEqual(cli.returncode,0,cli.stderr)
                self.assertEqual(call["method"],"devshell.lock");self.assertEqual(call["params"],{})
                self.assertIn(f"generation {generation}",cli.stdout)
                self.assertEqual(json.loads(path.read_text()),lock)
            for result in (None,[],dict(path=7)):
                cli,call=self.sandbox_fake_cli(["devshell","restore-lock"],result)
                self.assertNotEqual(cli.returncode,0);self.assertIn("missing lock path",cli.stderr)
        with tempfile.TemporaryDirectory(prefix="gc-flake-") as root:
            other=Path(root)/"other";other.mkdir()
            cli,call=self.sandbox_fake_cli(["flake","run",str(other)+"#hello"],dict(path=str(Path(root)/"flake.lock"),lock={},generation=1))
            self.assertEqual(call["method"],"devshell.lock")
            self.assertNotEqual(cli.returncode,0);self.assertIn("serves only this sandbox's dev shell flake",cli.stderr)
        for result,diagnostic in [(None,"missing program"),({},"missing program"),(dict(program=1),"missing program"),(dict(program="/tmp/not-a-store-program"),"invalid program")]:
            cli,call=self.sandbox_fake_cli(["flake","run","#hello"],result,features=("flake-run",))
            self.assertEqual(call["method"],"flake.run");self.assertEqual(call["params"],dict(app="hello"))
            self.assertNotEqual(cli.returncode,0);self.assertIn(diagnostic,cli.stderr)
        program=shutil.which("true")
        self.assertTrue(program.startswith("/nix/store/"),program)
        cli,call=self.sandbox_fake_cli(["flake","run","#"],dict(program=program,generation="ignored",extension=True),features=("flake-run",))
        self.assertEqual(cli.returncode,0,cli.stderr);self.assertEqual(call["params"],dict(app="default"))

    def sandbox_fake_cli(self, arguments, result, cwd=None, features=(), rpc_error=None):
        import errno
        import shlex
        import sys
        target=Path(os.environ.get("CARGO_TARGET_DIR",contract.ROOT/"target"))
        binary=(target/"debug"/"goblins-client").resolve()
        fixtures=contract.read("fixtures.json")
        with tempfile.TemporaryDirectory(prefix="gc-member-") as root:
            root=Path(root);library=root/"redirect.so"
            compile=subprocess.run([*shlex.split(os.environ.get("CC","cc")),"-shared","-fPIC",str(contract.ROOT/"tests/request_socket_redirect.c"),"-o",str(library)],text=True,capture_output=True)
            self.assertEqual(compile.returncode,0,compile.stderr)
            import ctypes
            guard=ctypes.CDLL(str(library),use_errno=True)
            block_connect=guard.block_connect_syscall;block_connect.argtypes=[];block_connect.restype=ctypes.c_int
            def isolate():
                if block_connect()!=0: raise OSError(ctypes.get_errno(),"install connect denial before exec")
            # Prove the guard independently of the CLI and any daemon: a
            # private listener is reachable normally, but the guarded child
            # must receive EACCES without creating a queued connection.
            sentinel=root/"sentinel.sock"
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(sentinel));listener.listen()
                with socket.socket(socket.AF_UNIX) as reachable:
                    reachable.connect(str(sentinel));accepted,_=listener.accept();accepted.close()
                probe="import socket,sys; s=socket.socket(socket.AF_UNIX); print(s.connect_ex(sys.argv[1]))"
                checked=subprocess.run([sys.executable,"-c",probe,str(sentinel)],env={**os.environ,"LD_PRELOAD":""},preexec_fn=isolate,text=True,capture_output=True,timeout=10)
                self.assertEqual(checked.returncode,0,checked.stderr)
                self.assertEqual(checked.stdout.strip(),str(errno.EACCES))
                listener.setblocking(False)
                with self.assertRaises(BlockingIOError): listener.accept()
            # No preload means the real connect syscall is denied, with no RPC
            # bytes reaching even the already-connected fake peer.
            negative=root/"no-hook.sock"
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(negative));listener.listen()
                with socket.socket(socket.AF_UNIX) as fake:
                    fake.connect(str(negative));peer,_=listener.accept()
                    with peer:
                        cli=subprocess.run([str(binary),"run","shell"],env={**os.environ,"LD_PRELOAD":""},preexec_fn=isolate,pass_fds=(fake.fileno(),),text=True,capture_output=True,timeout=10)
                        self.assertNotEqual(cli.returncode,0)
                        self.assertIn("(os error 13)",cli.stderr)
                        fake.close();peer.settimeout(2);self.assertEqual(peer.recv(1),b"")
            path=root/"request.sock";calls=[];errors=[]
            with socket.socket(socket.AF_UNIX) as listener:
                listener.settimeout(5);listener.bind(str(path));listener.listen()
                def serve():
                    try:
                        peer,_=listener.accept()
                        with peer:
                            peer.settimeout(5);init=receive(peer)
                            peer.sendall(frame(dict(jsonrpc="2.0",id=init["id"],result={**fixtures["sandbox_initialize"],"features":[*fixtures["sandbox_initialize"]["features"],*features]})))
                            call=receive(peer);calls.append(call)
                            body={"result":result} if rpc_error is None else {"error":rpc_error}
                            peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],**body)))
                    except Exception as error: errors.append(error)
                server=threading.Thread(target=serve,daemon=True)
                preload=str(library)+(":"+os.environ["LD_PRELOAD"] if os.environ.get("LD_PRELOAD") else "")
                fake=socket.socket(socket.AF_UNIX);fake.connect(str(path))
                env={**os.environ,"LD_PRELOAD":preload,"GOBLINS_TEST_FAKE_REQUEST_FD":str(fake.fileno())}
                args=[str(binary),*arguments]
                # Install the pre-exec guard before creating the server
                # thread, so no Python lock can be inherited from it.
                child=subprocess.Popen(args,cwd=cwd,env=env,pass_fds=(fake.fileno(),),preexec_fn=isolate,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                server.start()
                try:
                    output,error=child.communicate(timeout=10)
                    cli=subprocess.CompletedProcess(args,child.returncode,output,error)
                finally:
                    if child.poll() is None:
                        child.kill();child.communicate()
                    fake.close()
                server.join(timeout=6);self.assertFalse(server.is_alive());self.assertEqual(errors,[])
                return cli,calls[0]

    def test_sandbox_run_keeps_exact_named_and_unset_request_fields(self):
        result=contract.read("fixtures.json")["forward_compatibility"]["member_launch"]
        for named in (False,True):
            args=["run","shell"]+(["--name","named","--worktree","branch","-o","model=fast"] if named else [])
            cli,call=self.sandbox_fake_cli(args,result)
            self.assertEqual(cli.returncode,0,cli.stderr)
            self.assertEqual(call["method"],"sessions.start")
            params=call["params"]
            self.assertEqual(set(params),{"key","name","agent_name","detached","options"}|({"worktree"} if named else set()))
            self.assertEqual(params["name"],"shell");self.assertEqual(params["agent_name"],"named" if named else None)
            self.assertIs(params["detached"],True);self.assertEqual(params["options"],{"model":"fast"} if named else {})
            if named:self.assertEqual(params["worktree"],"branch")
            self.assertEqual(json.loads(cli.stdout),result)

    def test_sandbox_terminal_cli_preserves_params_relative_cwd_and_legacy_results(self):
        with tempfile.TemporaryDirectory(prefix="gc-terminal-cwd-") as cwd:
            cases=[
                (["create"],{"env":[]},{"id":"t1"},"t1\n"),
                (["create","--name","app","--cwd","relative","--notify","--env","A=b","--size","30x90","--","sh"],
                 {"env":[["A","b"]],"name":"app","cwd":str(Path(cwd)/"relative"),"notify":True,"rows":30,"cols":90,"argv":["sh"]},{"id":7},"?\n"),
                (["create"],{"env":[]},None,"?\n"),
                (["list"],{},{"terminals":[{"id":7,"name":"odd","origin":"remote","rows":"wide","cols":None,"attached":"false","label":"bad"},None]},'-\todd\tremote\t"wide"xnull\tdetached\tbad\n-\t-\t-\tnullxnull\tdetached\t-\n'),
                (["list"],{},None,""),
                (["close","app"],{"terminal":"app"},[],""),
            ]
            for args,params,result,expected in cases:
                with self.subTest(args=args,result=result):
                    cli,call=self.sandbox_fake_cli(["terminal",*args],result,cwd=cwd,features=("terminals",))
                    self.assertEqual(cli.returncode,0,cli.stderr)
                    self.assertEqual(cli.stdout,expected);self.assertEqual(cli.stderr,"")
                    self.assertEqual(call["method"],"terminals."+args[0]);self.assertEqual(call["params"],params)

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

    def test_permission_decision_binding_at_live_host_boundary(self):
        peer=self.raw()
        self.assertEqual(self.exchange(peer,"permissions.decide",{},1)["error"]["code"],-32001)
        self.exchange(peer,"initialize",dict(api=1),2)
        for index,case in enumerate(contract.read("fixtures.json")["permission_decision_parameter_cases"],3):
            reply=self.exchange(peer,"permissions.decide",case["params"],index)
            self.assertEqual(reply["error"]["code"],-32004 if case["valid"] else -32602,str(case))

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

    def test_self_reporting_uses_current_contracts_at_the_sandbox_boundary(self):
        launch = self.d.start()
        self.d.wait(lambda: self.d.get(launch["session"])["state"] == "running")
        path = self.d.state / launch["session"] / "resources/request.sock"
        def invoke(method, params):
            rpc = RPC(path)
            try:
                return rpc.call(method, params)
            finally:
                rpc.close()
        status = invoke("sessions.status", [])
        self.assertEqual(status["id"], launch["session"])
        self.assertNotIn("identity", status)
        self.assertNotIn("configuration", status)
        self.assertEqual(invoke("sessions.set-state", ["working"]), {"accepted":True})
        self.assertEqual(invoke("sessions.status", {})["agent_state"], "working")
        result = invoke("sessions.set-variables", [{"session_id":"contract-test"}])
        self.assertEqual(result, {"accepted":True, "recorded":True})
        self.assertEqual(invoke("sessions.set-worktree", {}), {"worktree":None})
        self.assertEqual(invoke("sessions.set-worktree", {"name":None}), {"worktree":None})
        for method, params in [("sessions.set-state", {"state":"running"}), ("sessions.set-variables", {"variables":{"Bad":"x"}}), ("sessions.set-worktree", {"name":"../escape"}), ("sessions.set-worktree", [])]:
            with self.assertRaises(ValueError) as error:
                invoke(method, params)
            self.assertEqual(error.exception.args[0]["code"], -32602)

    def test_stop_acceptance_closes_the_connection(self):
        peer = self.raw()
        self.exchange(peer, "initialize", {"api": 1})
        self.assertEqual(self.exchange(peer, "server.stop", [], 2)["result"], {"accepted": True})
        self.assertEqual(peer.recv(1), b"")
