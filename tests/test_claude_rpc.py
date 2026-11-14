"""The Claude integration's sandbox RPC readers: the mod's message projection and rpc.sh, against the contract fixtures. No daemon is started."""
import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import api_contract as contract


class ClaudeRpcTests(unittest.TestCase):
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
        cases = [(good, 0), (extended, 0), (fixtures["host_initialize"], 1), (bad, 1)]
        for field in ("configuration", "limits"):
            missing = copy.deepcopy(good); del missing[field]
            cases.append((missing, 1))
        for value, expected in cases:
            result = subprocess.run(["jq", "-e", predicate], input=json.dumps(value), text=True, capture_output=True)
            self.assertEqual(result.returncode, expected, result.stderr)


if __name__ == "__main__":
    unittest.main()
