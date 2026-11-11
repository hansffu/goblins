"""Schema and real-boundary checks for generated contracts, including legacy parameter forms."""
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
        before = '"permissions.request"\n            | contract::sandbox::FlakeRun::NAME'
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
        assigned["pending"]["C99"] = {"host":["server.new"]}
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

    def sandbox_fake_cli(self, arguments, result, cwd=None, features=()):
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
                            peer.sendall(frame(dict(jsonrpc="2.0",id=call["id"],result=result)))
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
