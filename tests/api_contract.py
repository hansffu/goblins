"""Offline OpenRPC/schema checks and response validation for migrated methods."""
import argparse
import copy
from functools import lru_cache
import json
import re
from pathlib import Path

from jsonschema import Draft7Validator, ValidationError, validators
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT7
from referencing.exceptions import Unresolvable

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = ROOT / "libs/api/schema"
REFERENCE = ROOT / "docs/API-REFERENCE.org"
ENDPOINTS = ("host", "sandbox", "attach")
NORMALIZATIONS = {
    "editor-open-fields": "Read signed i64 code and string message independently; absent/malformed fields and non-objects retain exit code 1 and no message as fallbacks.",
    "devshell-preview-display": "Normalize recognized preview fields independently while preserving CLI/TUI numeric display fallbacks and object/array side presence; reject non-object diff results.",
    "devshell-lock-fields": "Read string path or absent, unsigned u64 generation or zero, arbitrary lock JSON or null.",
    "flake-run-fields": "Read string program or absent; other fields are ignored.",
    "exec-field-fallback": "Read code/signal independently as signed 64-bit JSON integers and message as a string; other values become null. A non-object result has no recognized fields.",
    "terminal-create-fields": "Read a string id independently; all other input becomes an absent id, preserving the CLI's ? fallback.",
    "terminal-list-display": "Iterate terminals only when it is an array. Each item independently retains string identity/display fields, boolean attached and JSON-rendered rows/cols (missing means null).",
    "terminal-close-fields": "The CLI ignores close results; normalize only a boolean accepted when present.",
    "terminal-read-fields": "Read a string text independently, otherwise absent (printed as an empty line).",
    "terminal-wait-fields": "Read string status/line/text and signed 64-bit integer code/signal independently. Missing or malformed fields are absent; an unknown/missing status retains the timeout report.",
}


def normalize_reply(profile, result):
    fields = result if isinstance(result, dict) else {}
    text = lambda name: fields.get(name) if isinstance(fields.get(name), str) else None
    if profile == "devshell-preview-display":
        def field(v,key):return v.get(key) if isinstance(v,dict) else None
        def text(v,key):
            value=field(v,key)
            return value if isinstance(value,str) else ""
        def optional(v,key):
            value=field(v,key)
            return value if isinstance(value,str) else None
        def items(v,key):
            value=field(v,key)
            return value if isinstance(value,list) else []
        def number(v,key):
            value=field(v,key)
            return dict(json=json.dumps(value,ensure_ascii=False,separators=(",",":"),sort_keys=True),unsigned=value if type(value) is int and 0<=value<2**64 else None)
        def locked(v,key):
            value=field(v,key)
            return dict(source=optional(value,"source"),id=text(value,"id"),date=optional(value,"date")) if isinstance(value,dict) else None
        def versions(v,key):
            value=field(v,key)
            return [s if isinstance(s,str) else "" for s in value] if isinstance(value,list) else None
        return dict(reference=text(fields,"reference"),**{"from":number(fields,"from")},to=number(fields,"to"),
            attention=[dict(**{key:text(n,key) for key in ("kind","input","source","old","new")},count=number(n,"count")) for n in items(fields,"attention")],
            files=[dict(path=text(f,"path"),added=number(f,"added"),removed=number(f,"removed")) for f in items(fields,"files")],
            inputs=[dict(name=text(c,"name"),change=text(c,"change"),old=locked(c,"old"),new=locked(c,"new")) for c in items(fields,"inputs")],
            packages=[dict(name=text(c,"name"),change=text(c,"change"),old=versions(c,"old"),new=versions(c,"new")) for c in items(fields,"packages")],
            env=[dict(name=text(c,"name"),change=text(c,"change"),old=optional(c,"old"),new=optional(c,"new"),long=field(c,"long") is True) for c in items(fields,"env")],
            fetch=number(fields,"fetch"),build=number(fields,"build"),download=optional(fields,"download"),
            incoming=[dict(action=text(c,"action"),name=text(c,"name"),versions=versions(c,"versions")) for c in items(fields,"incoming")],diff=text(fields,"diff"))
    if profile == "flake-run-fields": return dict(program=text("program"))
    if profile == "devshell-lock-fields":
        generation=fields.get("generation")
        return dict(path=text("path"), lock=fields.get("lock"), generation=generation if type(generation) is int and 0 <= generation < 2**64 else 0)
    if profile == "editor-open-fields":
        code=fields.get("code")
        return dict(code=code if type(code) is int and -(2**63)<=code<2**63 else None,message=text("message"))
    if profile == "exec-field-fallback":
        normalized = {name: (value if type(value) is int and -(2**63) <= value < 2**63 else None) for name in ("code", "signal") for value in [fields.get(name)]}
        return dict(normalized, message=text("message"))
    if profile == "terminal-create-fields": return dict(id=text("id"))
    if profile == "terminal-close-fields": return dict(accepted=fields.get("accepted") if type(fields.get("accepted")) is bool else None)
    if profile == "terminal-read-fields": return dict(text=text("text"))
    if profile == "terminal-wait-fields":
        normalized={name:text(name) for name in ("status","line","text")}
        normalized.update({name:value if type(value) is int and -(2**63)<=value<2**63 else None for name in ("code","signal") for value in [fields.get(name)]})
        return normalized
    if profile == "terminal-list-display":
        terminals=[]
        for item in fields.get("terminals", []) if isinstance(fields.get("terminals"), list) else []:
            item=item if isinstance(item, dict) else {}
            row={key: item.get(key) if isinstance(item.get(key), str) else None for key in ("id","name","label","origin")}
            row.update(attached=item.get("attached") if type(item.get("attached")) is bool else None)
            row.update({key:json.dumps(item.get(key),ensure_ascii=False,sort_keys=True,separators=(",",":")) for key in ("rows","cols")})
            terminals.append(row)
        return dict(terminals=terminals)
    raise ValueError("unknown client normalization")


def read(name):
    return json.loads((SCHEMA / name).read_text())


def byte_limit(validator, maximum, instance, schema):
    if isinstance(instance, str) and len(instance.encode("utf-8")) > maximum:
        yield ValidationError(f"text exceeds {maximum} UTF-8 bytes")


def integer_representation(validator, enabled, instance, schema):
    if enabled and type(instance) is not int:
        yield ValidationError("expected a JSON integer, not a floating-point value")


Validator = validators.extend(Draft7Validator, {"x-max-bytes": byte_limit, "x-json-integer": integer_representation})


@lru_cache
def common():
    return read("common.json")


@lru_cache
def document(endpoint):
    return read(f"{endpoint}.openrpc.json")


def resolve(schema):
    """Resolve only the pinned shared file; never fetch a network reference."""
    if "$ref" not in schema:
        return schema
    ref = schema["$ref"]
    prefix = "common.json#/"
    if not ref.startswith(prefix):
        raise ValueError(f"unexpected external reference {ref}")
    value = common()
    for part in ref[len(prefix):].split("/"):
        value = value[part]
    return value


def schema_validator(schema):
    return Validator({**resolve(schema), "definitions": common()["definitions"], "x-envelope-schemas": common()["x-envelope-schemas"], "x-positional-schemas": common()["x-positional-schemas"]})


def method(endpoint, name):
    return next((m for m in document(endpoint)["methods"] if m["name"] == name), None)


def validate_params(endpoint, name, params):
    entry=method(endpoint,name)
    if entry is None:return False
    if params is None and entry.get("x-null-params-as-empty"):params={}
    schema_validator(entry["x-positional-schema"] if isinstance(params,list) else entry.get("x-wire-params-schema",entry["x-params-schema"])).validate(params)
    if isinstance(params,list):params={p["name"]:value for p,value in zip(entry["params"],params)}
    params=copy.deepcopy(params)
    for nested in entry.get("x-nested-positional-params",[]):
        params[nested["field"]]=[dict(zip(nested["order"],record)) if isinstance(record,list) else record for record in params[nested["field"]]]
    if "x-request-constraints" in entry:schema_validator(entry["x-request-constraints"]).validate(params)
    return True


def validate_notification(endpoint, direction, notification, compatibility=False):
    if not isinstance(notification,dict):return False
    entry = next((n for n in document(endpoint).get("x-notifications", []) if n["name"] == notification.get("method") and n["direction"] == direction), None)
    if entry is None: return False
    if compatibility and entry.get("x-client-normalization") == "attach-waiting-legacy":
        params=notification.get("params",{})
        missing=params.get("missing") if isinstance(params,dict) else None
        schema_validator(entry["x-client-params-schema"]).validate(dict(has_missing=isinstance(missing,list) and bool(missing)))
        return True
    if set(notification) - {"jsonrpc", "method", "params"} or notification.get("jsonrpc") != "2.0":
        raise ValidationError("invalid notification envelope")
    params = notification.get("params", {})
    schema_validator(entry["x-positional-schema"] if isinstance(params, list) else entry["x-params-schema"]).validate(params)
    if isinstance(params,list):params={p["name"]:value for p,value in zip(entry["params"],params)}
    if "x-request-constraints" in entry:schema_validator(entry["x-request-constraints"]).validate(params)
    return True


def validate_completion(name, reply, compatibility=False):
    entry=next((e for e in document("attach")["x-completions"] if e["name"]==name),None)
    if entry is None:return False
    if not isinstance(reply,dict):raise ValidationError("invalid completion envelope")
    shape="Failure" if "error" in reply else "Success"
    schema_validator(common()["x-envelope-schemas"][shape]).validate(reply)
    if type(reply.get("id")) is not int or reply["id"] != entry["id"]:raise ValidationError("invalid completion id")
    if "error" in reply:
        if not compatibility and reply["error"]["code"] not in [e["code"] for e in entry["errors"]]:raise ValidationError("unlisted attachment error")
    else:schema_validator(entry["result"]["schema"]).validate(reply["result"])
    return True


def validate_reply(endpoint, name, reply, compatibility=False):
    """Unmigrated methods remain pending; covered methods must match exactly."""
    entry = method(endpoint, name)
    if entry is None:
        return False
    shape = "Failure" if "error" in reply else "Success"
    schema_validator(common()["x-envelope-schemas"][shape]).validate(reply)
    if "error" in reply:
        error = reply["error"]
        codes = document(endpoint)["x-common-errors"] + [e["code"] for e in entry["errors"]]
        if error["code"] not in codes:
            raise ValidationError(f"unlisted error for {endpoint}/{name}: {error['code']}")
    else:
        result = reply["result"]
        if compatibility and "x-client-result-normalization" in entry:
            schema_validator(entry["x-client-wire-result-schema"]).validate(result)
            result = normalize_reply(entry["x-client-result-normalization"],result)
        schema = entry.get("x-client-result-schema", entry["result"]["schema"]) if compatibility else entry["result"]["schema"]
        schema_validator(schema).validate(result)
        if name == "initialize" and (result["api"] != common()["x-api-version"] or result["role"] != endpoint):
            raise ValidationError("incompatible initialization result")
        if name == "initialize" and endpoint == "host" and result.get("configuration") is not None:
            raise ValidationError("host configuration must be null")
    return True


def reference():
    lines = ["#+title: Generated JSON-RPC reference (covered surfaces)", "", "Generated by =just api-reference=. Do not edit. Unmigrated surfaces remain in [[file:API.org][API.org]] and =libs/api/schema/coverage.json=.", ""]
    for endpoint in ENDPOINTS:
        lines.extend([f"* {endpoint}", ""])
        for entry in document(endpoint)["methods"]:
            lines.extend([f"** ={entry['name']}=", "", entry["description"], "", "Named parameters:", "", "#+begin_src json", json.dumps(resolve(entry.get("x-wire-params-schema", entry["x-params-schema"])), indent=2), "#+end_src", "", "Positional parameters:", "", "#+begin_src json", json.dumps(resolve(entry["x-positional-schema"]), indent=2), "#+end_src", "", "Result:", "", "#+begin_src json", json.dumps(resolve(entry["result"]["schema"]), indent=2), "#+end_src", "", "Method errors: " + ", ".join(str(e["code"]) for e in entry["errors"]) + ". Common envelope/connection errors also apply.", ""])
            if "x-wire-params-schema" in entry:
                lines.extend(["Normalized generated request DTO (after nested positional mapping):", "", "#+begin_src json", json.dumps(resolve(entry["x-params-schema"]), indent=2), "#+end_src", ""])
            if "x-client-result-schema" in entry:
                if "x-client-result-normalization" in entry:
                    profile=entry["x-client-result-normalization"]
                    lines.extend(["Compatible client wire result (legacy field fallback):", "", "#+begin_src json",json.dumps(resolve(entry["x-client-wire-result-schema"]),indent=2),"#+end_src","",f"The ={profile}= adapter: {NORMALIZATIONS[profile]} The generated normalized client DTO is:",""])
                else:
                    lines.extend(["Compatible API-1 client reply (additive information may be absent):", ""])
                lines.extend(["#+begin_src json", json.dumps(resolve(entry["x-client-result-schema"]), indent=2), "#+end_src", ""])
            if "x-request-constraints" in entry:
                lines.extend(["Cross-field request constraints (existing semantic validation):","","#+begin_src json",json.dumps(resolve(entry["x-request-constraints"]),indent=2),"#+end_src",""])
        for entry in document(endpoint).get("x-notifications", []):
            lines.extend([f"** Notification ={entry['name']}=", "", entry["description"], "", f"Direction: ={entry['direction']}=. Sender: {entry['sender']}. Receiver: {entry['receiver']}. State: {entry['connection-state']}.", "", "Named parameters:", "", "#+begin_src json", json.dumps(resolve(entry.get("x-wire-params-schema", entry["x-params-schema"])), indent=2), "#+end_src", "", "Positional parameters:", "", "#+begin_src json", json.dumps(resolve(entry["x-positional-schema"]), indent=2), "#+end_src", ""])
            if "x-client-normalization" in entry:
                lines.extend(["Legacy client classification follows =attach-client-compatibility=; the normalized DTO is:","","#+begin_src json",json.dumps(resolve(entry["x-client-params-schema"]),indent=2),"#+end_src",""])
        for entry in document(endpoint).get("x-completions",[]):
            lines.extend([f"** Completion exchange ={entry['name']}=","",entry["description"],"",f"Socket: ={entry['socket']}=. Direction: ={entry['direction']}=. Fixed response ID: ={entry['id']}=.","","#+begin_src json",json.dumps(resolve(entry["result"]["schema"]),indent=2),"#+end_src","","Errors: "+", ".join(str(e["code"]) for e in entry["errors"])+".",""])
    lines.extend(["* Envelopes (handwritten transport, schema checked)", "", "#+begin_src json", json.dumps(common()["x-envelope-schemas"], indent=2), "#+end_src", "", "* Shared records", ""])
    for name, schema in common()["definitions"].items():
        lines.extend([f"** ={name}=", "", "#+begin_src json", json.dumps(schema, indent=2), "#+end_src", ""])
    lines.extend(["* Protocol behavior", ""])
    for name, text in common()["x-protocol"].items():
        lines.extend([f"- ={name}= :: {text}"])
    lines.extend(["", "* Common errors", ""])
    lines.extend(f"- ={code}= :: {text}" for code, text in common()["x-errors"].items())
    lines.extend(["", "* Generation exceptions", ""])
    lines.extend(f"- ={name}= :: {text}" for name, text in read("exceptions.json").items())
    return "\n".join(lines) + "\n"


def implementation_sources():
    paths = {
        "controller": "packages/daemon/src/controller.rs",
        "gate": "libs/runtime/src/sandbox.rs",
        "raids": "packages/daemon/src/controller/raids.rs",
        "terminals": "packages/daemon/src/controller/terminals.rs",
        "exec_input": "packages/daemon/src/controller/exec.rs",
        "exec_client": "packages/daemon/src/exec.rs",
        "host_client": "packages/daemon/src/host.rs",
        "sandbox_host_client": "packages/sandbox-cli/src/host.rs",
        "attach_server": "packages/daemon/src/controller/attach.rs",
        "attach_client": "packages/daemon/src/attachment.rs",
        "legacy_terminal": "libs/supervisor/src/terminal.rs",
        "terminal_runtime": "libs/supervisor/src/terminals.rs",
    }
    return {key: (ROOT / path).read_text() for key, path in paths.items()}


# This scanner is deliberately limited to the current routing structure. Missing
# functions, role branches or allowlists fail closed when routing is refactored.
RUST_TOKEN = re.compile(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|[{}\[\](),:]', re.S)


def rust_block(text, start):
    depth = 0
    begin = None
    for token in RUST_TOKEN.finditer(text, start):
        if token.group() == "{":
            if begin is None: begin = token.end()
            depth += 1
        elif token.group() == "}":
            depth -= 1
            if begin is not None and depth == 0:
                return text[begin:token.start()]
    raise ValueError("unterminated routing block")


def rust_function(text, name):
    signature = re.search(r'\bfn\s+' + re.escape(name) + r'\s*\(', text)
    if signature is None: raise ValueError(f"missing routing function {name}")
    return rust_block(text, signature.end())


def matches_patterns(text):
    """Read the pattern argument, including singleton and nested matches! calls."""
    tokens = list(RUST_TOKEN.finditer(text))
    for macro in re.finditer(r'\bmatches!\s*\(', text):
        if any(token.start() <= macro.start() < token.end() for token in tokens):
            continue  # A macro spelling inside a comment or string is not code.
        depth, begin = 1, None
        for token in RUST_TOKEN.finditer(text, macro.end()):
            value = token.group()
            if value == "(": depth += 1
            elif value == ")":
                depth -= 1
                if depth == 0:
                    if begin is None: raise ValueError("missing matches! pattern")
                    yield text[begin:token.start()]
                    break
            elif value == "," and depth == 1 and begin is None:
                begin = token.end()
        else:
            raise ValueError("unterminated matches! routing expression")


def notification_names(text):
    """Discover notification objects; nested params.id is not a request ID."""
    text = re.split(r'#\[cfg\(test\)\]\s*mod\s+tests\b', text, maxsplit=1)[0]
    names = set()
    for endpoint, rust in re.findall(r'contract::notification::<\s*contract::(host|sandbox|attach)_notifications::(\w+)\s*,?\s*>', text):
        matches = [n["name"] for n in document(endpoint).get("x-notifications", []) if n["x-rust-name"] == rust]
        if len(matches) != 1: raise ValueError("unassigned generated notification emitter")
        names.add(matches[0])
    for macro in re.finditer(r'\bjson!\s*\(\s*\{', text):
        body = rust_block(text, macro.end() - 1)
        tokens = [token.group() for token in RUST_TOKEN.finditer(body) if not token.group().startswith("/")]
        depth, fields = 0, {}
        for index, token in enumerate(tokens):
            if token in ("{", "[", "("): depth += 1
            elif token in ("}", "]", ")"): depth -= 1
            elif depth == 0 and token.startswith('"') and tokens[index + 1:index + 2] == [":"]:
                fields[json.loads(token)] = tokens[index + 2] if index + 2 < len(tokens) else None
        if fields.get("jsonrpc") == '"2.0"' and "method" in fields and "id" not in fields:
            name = fields["method"]
            if name is None or not name.startswith('"'):
                raise ValueError("notification method needs an explicit coverage source")
            names.add(json.loads(name))
    return names


def method_names(text):
    # Include OR patterns, equality checks and matches! lists, not only => arms.
    literals = set()
    for pattern in matches_patterns(text):
        for token in RUST_TOKEN.finditer(pattern):
            match = re.fullmatch(r'"([a-z]+\.[a-z_-]+)"', token.group())
            if match: literals.add(match[1])
    for token in RUST_TOKEN.finditer(text):
        match = re.fullmatch(r'"([a-z]+\.[a-z_-]+)"', token.group())
        if match:
            before, after = text[:token.start()], text[token.end():]
            routing = (re.search(r'(?:==|!=|\|)\s*$', before) or
                       re.match(r'\s*(?:=>|\|)', after) or
                       re.search(r'"method"\s*:\s*$', before) or
                       text.strip() == token.group())
            if routing: literals.add(match[1])
    for endpoint, rust in re.findall(r'contract::(host|sandbox)::(\w+)::NAME', text):
        matches = [entry["name"] for entry in document(endpoint)["methods"] if entry["x-rust-name"] == rust]
        if len(matches) != 1: raise ValueError(f"unassigned generated identity {endpoint}/{rust}")
        literals.add(matches[0])
    for endpoint, rust in re.findall(r'contract::(?:decode_notification|notification)::<\s*contract::(host|sandbox|attach)_notifications::(\w+)\s*,?\s*>', text):
        matches = [n["name"] for n in document(endpoint).get("x-notifications", []) if n["x-rust-name"] == rust]
        if len(matches) != 1: raise ValueError("unassigned generated notification identity")
        literals.add(matches[0])
    return literals


def completion_names(text):
    text=re.split(r'#\[cfg\(test\)\]\s*mod\s+tests\b',text,maxsplit=1)[0]
    found=set()
    for rust in re.findall(r'contract::(?:decode_completion|completion_payload|completion_error)::<\s*contract::attach_exchanges::(\w+)\s*,?\s*>',text):
        matches=[e["name"] for e in document("attach")["x-completions"] if e["x-rust-name"]==rust]
        if len(matches)!=1:raise ValueError("unassigned generated completion identity")
        found.add(matches[0])
    return found


def implementation_methods(sources):
    controller = sources["controller"]
    dispatch = rust_function(controller, "dispatch")
    messaging = dispatch[dispatch.index("let messaging ="):].split(";", 1)[0]
    shared = method_names(rust_function(sources["gate"], "messaging"))
    if method_names(messaging) != shared | {"communications.list"}:
        raise ValueError("host/sandbox messaging routing changed")
    roles = dispatch[dispatch.rindex("match &c.role"):]
    host_role, separator, _ = roles.partition("Role::Sandbox(session) =>")
    if not separator: raise ValueError("missing sandbox dispatch branch")
    sandbox_role = rust_block(roles, len(host_role) + len(separator))
    host = (method_names(rust_function(controller, "host")) |
            method_names(rust_function(sources["raids"], "host_raids")) |
            method_names(rust_function(sources["terminals"], "terminals")) |
            method_names(host_role) | method_names(messaging) | {"initialize"})
    refused = {"sessions.attach"}
    sandbox = (method_names(sandbox_role) |
               method_names(rust_function(controller, "sandbox_control")) |
               method_names(rust_function(sources["raids"], "sandbox_raids")) |
               method_names(rust_function(sources["terminals"], "sandbox_terminals")) |
               shared | {"initialize"}) - refused
    gate = rust_function(sources["gate"], "check")
    allowed = re.search(r'if\s+!matches!\(\s*method\s*,(.*?)\)\s*\{', gate, re.S)
    if allowed is None: raise ValueError("missing sandbox method allowlist")
    admitted = method_names(allowed.group(1)) | shared | {"initialize"}
    if sandbox != admitted - refused:
        raise ValueError(f"sandbox gate/controller disagree: {sandbox ^ (admitted - refused)}")
    return {"host": host, "sandbox": sandbox}


def check_coverage(registry=None, sources=None):
    registry = read("coverage.json") if registry is None else registry
    sources = implementation_sources() if sources is None else sources
    actual = implementation_methods(sources)
    for endpoint in ("host", "sandbox"):
        names = [entry["name"] for entry in document(endpoint)["methods"]]
        for chunk, endpoints in registry["pending"].items():
            if not re.fullmatch(r'C\d\d', chunk): raise ValueError(f"invalid chunk {chunk}")
            names.extend(endpoints.get(endpoint, []))
        if len(names) != len(set(names)): raise ValueError(f"duplicate assignment on {endpoint}")
        if set(names) != actual[endpoint]:
            raise ValueError(f"{endpoint} coverage differs: {set(names) ^ actual[endpoint]}")
    published = notification_names(rust_function(sources["controller"], "publish"))
    submitted = notification_names(rust_function(sources["controller"], "submit"))
    emitted = set().union(*(notification_names(sources[key]) for key in ("controller", "raids", "terminals", "exec_input")))
    if emitted != published | submitted:
        raise ValueError(f"unclassified daemon notification emitter: {emitted ^ (published | submitted)}")
    notifications = ({("host", name, "server-to-client") for name in published} |
                     {("sandbox", name, "server-to-client") for name in submitted} |
                     {("host", name, "client-to-server") for name in notification_names(sources["exec_client"])} |
                     {("attach", name, "client-to-server") for name in notification_names(sources["attach_client"])} |
                     {("attach", name, "server-to-client") for name in notification_names(sources["attach_server"])})
    assigned = {(row["endpoint"], row["name"], row["direction"]) for row in registry["notifications"]}
    covered = [(endpoint, row["name"], row["direction"]) for endpoint in ENDPOINTS for row in document(endpoint).get("x-notifications", [])]
    for row in registry["notifications"]:
        if not re.fullmatch(r'C\d\d', row["chunk"]): raise ValueError("invalid notification chunk")
    if assigned | set(covered) != notifications or len(assigned) != len(registry["notifications"]) or len(set(covered)) != len(covered) or assigned & set(covered):
        raise ValueError("notification endpoint/direction/chunk differs from its exchange")
    checks = [("controller", "submit", "host.submitted"), ("controller", "publish", "state.changed"), ("exec_input", "exec_input", "exec.signal")]
    for source, function, name in checks:
        if name not in method_names(rust_function(sources[source], function)):
            raise ValueError(f"notification exchange changed: {name}")
    for source, name in [("sandbox_host_client", "host.submitted"), ("host_client", "state.changed"), ("exec_client", "exec.signal")]:
        if name not in method_names(sources[source]): raise ValueError(f"missing notification peer: {name}")
    if "attach.hello" not in method_names(rust_function(sources["attach_server"],"accept")) or "contract::attach_waiting(" not in sources["attach_client"]:
        raise ValueError("attachment notification peer changed")
    completed=set().union(*(completion_names(sources[key]) for key in ("attach_client","attach_server","legacy_terminal","terminal_runtime")))
    if completion_names(sources["attach_client"])!={"attach.reply","legacy.attach"} or "legacy.attach" not in completion_names(sources["legacy_terminal"]) or "attach.reply" not in completion_names(sources["attach_server"]) or "attach.reply" not in completion_names(sources["terminal_runtime"]):
        raise ValueError("missing completion producer or consumer")
    assigned_exchanges=[(row["endpoint"],row["name"]) for row in registry["exchanges"]]+[("attach",row["name"]) for row in document("attach")["x-completions"]]
    if len(assigned_exchanges)!=len(set(assigned_exchanges)) or set(assigned_exchanges)!={("attach",name) for name in completed}:
        raise ValueError("completion exchange coverage differs")
    expected_refusals = {("sandbox", "sessions.attach", "C03"), ("host", "*", "C01"), ("sandbox", "*", "C01")}
    if {(row["endpoint"], row["name"], row["chunk"]) for row in registry["refusals"]} != expected_refusals:
        raise ValueError("refusal behavior coverage differs")


def check_nested_params(entry,params,wire):
    if set(params["properties"])!=set(wire["properties"]) or params["required"]!=wire["required"]:raise ValueError("wire and generated parameter fields differ")
    expected=copy.deepcopy(params)
    seen=set()
    for row in entry.get("x-nested-positional-params",[]):
        field,schema,order,minimum=(row[k] for k in ("field","schema","order","minimum"))
        if field in seen or field not in params["properties"]:raise ValueError("invalid nested positional field")
        seen.add(field)
        collection=resolve({"$ref":"common.json"+params["properties"][field]["$ref"]})
        record=common()["definitions"][schema]
        if collection.get("type")!="array" or collection["items"]!={"$ref":"#/definitions/"+schema}:raise ValueError("nested positional field is not an array of its record")
        if len(order)!=len(set(order)) or set(order)!=set(record["properties"]) or minimum!=max((i+1 for i,name in enumerate(order) if name in record["required"]),default=0):raise ValueError("nested positional order/minimum differs")
        positional={"type":"array","items":[record["properties"][name] for name in order],"minItems":minimum,"maxItems":len(order)}
        if common()["x-positional-schemas"][schema]!=positional:raise ValueError("nested positional schema differs")
        expected["properties"][field]={**collection,"items":{"anyOf":[collection["items"],{"$ref":"#/x-positional-schemas/"+schema}]}}
    if expected!=wire:raise ValueError("wire parameters differ from declared nested normalization")


def check_references(documents=None):
    """Resolve every nested reference from its actual containing document.

    This check never injects definitions into OpenRPC documents or retrieves
    network resources. It is separate from standalone fixture validation.
    """
    if documents is None:
        documents={"common.json":common(), **{f"{endpoint}.openrpc.json":document(endpoint) for endpoint in ENDPOINTS}}
    base="https://goblins.invalid/api/"
    registry=Registry().with_resources((base+name,Resource.from_contents(contents,default_specification=DRAFT7)) for name,contents in documents.items())
    def visit(value,resolver,location):
        if isinstance(value,dict):
            if "$ref" in value:
                try:resolver.lookup(value["$ref"])
                except Unresolvable as error:raise ValueError(f"dangling reference at {location}: {value['$ref']}") from error
            for key,child in value.items():visit(child,resolver,location+"/"+key)
        elif isinstance(value,list):
            for index,child in enumerate(value):visit(child,resolver,location+"/"+str(index))
    for name,contents in documents.items():visit(contents,registry.resolver(base+name),name)


def check():
    check_references()
    registry = Registry()
    for uri, path in (("https://meta.json-schema.tools/", "vendor/json-schema-tools.json"), ("https://meta.json-schema.tools", "vendor/json-schema-tools.json")):
        registry = registry.with_resource(uri, Resource.from_contents(read(path), default_specification=DRAFT7))
    meta = read("vendor/openrpc-1.3.2.json")
    for endpoint in ENDPOINTS:
        doc = document(endpoint)
        Draft7Validator(meta, registry=registry).validate(doc)
        names = [m["name"] for m in doc["methods"]]
        if endpoint=="attach" and names:raise ValueError("attachment exchanges are not callable methods")
        if len(names) != len(set(names)):
            raise ValueError(f"duplicate method: {endpoint}")
        for entry in doc["methods"] + doc.get("x-notifications", []):
            if entry in doc.get("x-notifications", []):
                if entry["name"] in names or entry["direction"] not in ("client-to-server", "server-to-client") or not all(entry.get(k) for k in ("sender", "receiver", "connection-state", "description")):
                    raise ValueError("invalid notification contract")
            params = resolve(entry["x-params-schema"])
            wire_params = resolve(entry.get("x-wire-params-schema",entry["x-params-schema"]))
            check_nested_params(entry,params,wire_params)
            if {p["name"] for p in entry["params"]} != set(params["properties"]):
                raise ValueError(f"parameter descriptors disagree: {entry['name']}")
            for parameter in entry["params"]:
                if resolve(parameter["schema"]) != wire_params["properties"][parameter["name"]] or parameter["required"] != (parameter["name"] in params["required"]):
                    raise ValueError("parameter descriptor differs from generated params")
            for name in entry.get("x-client-omit-empty-objects", []):
                field=params["properties"].get(name)
                if field is None or field.get("default") != {} or not schema_validator({"$ref":"common.json#/definitions/" + entry["x-params-schema"]["$ref"].rsplit("/",1)[1] + "/properties/" + name}).is_valid({}):
                    raise ValueError("invalid client empty-object omission")
            presence=entry.get("x-client-param-presence", {})
            if presence.get("switch") is not None and presence["switch"] not in params["properties"]: raise ValueError("unknown presence switch")
            for key, sample in [("absent-null-fields",None),("present-empty-strings","")]:
                for name in presence.get(key,[]):
                    if name not in params["properties"]: raise ValueError("unknown presence field")
                    field={"$ref":entry["x-params-schema"]["$ref"]+"/properties/"+name}
                    if not schema_validator(field).is_valid(sample): raise ValueError("invalid presence value")
            positional = resolve(entry["x-positional-schema"])
            order = [p["name"] for p in entry["params"]]
            positional_required = set(entry.get("x-positional-required", []))
            if not positional_required <= set(order): raise ValueError("unknown positional field")
            minimum = max((i + 1 for i, name in enumerate(order) if name in params["required"] or name in positional_required), default=0)
            def parameter_shape(value):
                if isinstance(value, dict): return {k:parameter_shape(v) for k,v in value.items() if k != "format"}
                if isinstance(value, list): return [parameter_shape(v) for v in value]
                return value
            expected_items = [wire_params["properties"][name] for name in order] if order else False
            if entry.get("paramStructure") == "by-name":
                if positional != {"not":{}}: raise ValueError("object-only method permits positional parameters")
            elif positional.get("minItems") != minimum or positional.get("maxItems") != len(order) or parameter_shape(positional.get("items")) != parameter_shape(expected_items):
                raise ValueError(f"positional schema differs from method parameter order: {entry['name']}")
            if entry.get("x-null-params-as-empty") and entry.get("paramStructure") != "by-name": raise ValueError("invalid nullable-object contract")
            for name in entry.get("x-client-omit-default-false", []):
                if params["properties"].get(name,{}).get("default") is not False: raise ValueError("invalid client false omission")
            schema_validator(entry["x-positional-schema"]).check_schema(resolve(entry["x-positional-schema"]))
            schema_validator(entry["x-params-schema"]).check_schema(params)
            if "result" in entry:
                schema_validator(entry["result"]["schema"]).check_schema(resolve(entry["result"]["schema"]))
            if "x-client-result-schema" in entry:
                schema_validator(entry["x-client-result-schema"]).check_schema(resolve(entry["x-client-result-schema"]))
            if "x-client-result-normalization" in entry:
                expected={"editor.open":"editor-open-fields","devshell.diff":"devshell-preview-display","devshell.lock":"devshell-lock-fields","flake.run":"flake-run-fields","sessions.exec":"exec-field-fallback","terminals.create":"terminal-create-fields","terminals.list":"terminal-list-display","terminals.close":"terminal-close-fields","terminals.read":"terminal-read-fields","terminals.wait":"terminal-wait-fields",**{name:"terminal-close-fields" for name in ("terminals.send","terminals.detach","terminals.resize")}}
                if entry["x-client-result-normalization"] != expected.get(entry["name"]) or entry.get("x-client-wire-result-schema") != ({"type":"object"} if entry["name"]=="devshell.diff" else {}):
                    raise ValueError("unknown client result normalization")
            if "x-request-constraints" in entry:
                schema_validator(entry["x-request-constraints"]).check_schema(resolve(entry["x-request-constraints"]))
        for entry in doc.get("x-completions",[]):
            if type(entry["id"]) is not int or entry["id"]!=0 or entry["direction"]!="server-to-client" or entry["errors"]!=[{"code":-32009,"message":"Attachment refused"}]:raise ValueError("invalid completion contract")
            schema_validator(entry["result"]["schema"]).check_schema(resolve(entry["result"]["schema"]))
            if "x-shared-payload-with" in entry:
                peer=next((e for e in doc["x-completions"] if e["name"]==entry["x-shared-payload-with"]),None)
                if peer is None or peer["result"]!=entry["result"] or peer["id"]!=entry["id"]:raise ValueError("completion payload alias differs")
    Draft7Validator.check_schema(common())
    if common()["definitions"]["AttachVersion"]["enum"] != [common()["x-attach-version"]]:raise ValueError("attachment version metadata differs")
    check_coverage()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    check()
    rendered = reference()
    if args.write:
        REFERENCE.write_text(rendered)
    elif not REFERENCE.exists() or REFERENCE.read_text() != rendered:
        raise SystemExit("stale API reference; run just api-reference")


if __name__ == "__main__":
    main()
