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
ELISP_READERS = ROOT / "emacs/goblins-api-generated.el"
ENDPOINTS = ("host", "sandbox", "attach")
PRIVATE_ENDPOINTS = ("supervisor",)
NORMALIZATIONS = {
    "raid-members-display": "Read raid/member display strings independently with - fallback, booleans strictly, members only when an array, created_by string presence and existing escaped/sorted option rendering; accept non-object legacy results.",
    "state-subscribe-fields": "Project string subscription, u64 sequence and raw snapshot independently; preserve validation only after nonblocking setup in the Rust consumer, including original missing-field diagnostics and initial cross-instance acceptance.",
    "history-list-fields": "Read records only as an array (otherwise empty) and truncated only as true; retain every raw record and additive field.",
    "history-prune-fields": "Require failed array and removed string array, decoding failed first; retain raw failed rows for independent dash fallback and ignore result dry_run.",
    "communications-page-fields": "Read events only as an array, cursor/latest only as u64 JSON integers, and session only as a string; malformed fields and nonobjects remain absent so the CLI retains its original printing and diagnostic order.",
    "ignore-successful-body": "Ignore the successful body, including non-objects; retain the original validated JSON for printing.",
    "permission-record-fields": "Map original positional record order, default missing kind to package, retain required string fields, nullable options and arbitrary preview JSON, ignoring additive record fields.",
    "permission-list-records": "Apply the compatible permission-record reader element-wise to the result list.",
    "permission-request-fields": "Independently read string request/status/message and signed i64 code; other values and non-objects become absent, preserving the host CLI fallbacks and original detached JSON output.",
    "permission-grant-fields": "Preserve the closed grant CLI result decoder, optional named message and exact positional request/status/message tuple, followed by identifier and ready/denied/error checks.",
    "permission-decision-fields": "Independently read boolean accepted/approved and string request for the typed projection; successful bodies remain otherwise ignored, and the plain frontend prints the original validated JSON.",
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
    if profile == "state-subscribe-fields":
        fields=result if isinstance(result,dict) else {}
        seq=fields.get("sequence")
        return dict(subscription=fields.get("subscription") if isinstance(fields.get("subscription"),str) else None,
                    sequence=seq if type(seq) is int and 0<=seq<2**64 else None,snapshot=fields.get("snapshot"))
    if profile == "history-list-fields":
        fields=result if isinstance(result,dict) else {}
        return dict(records=fields["records"] if isinstance(fields.get("records"),list) else [],truncated=fields.get("truncated") is True)
    if profile == "history-prune-fields":
        return dict(removed=result["removed"],failed=result["failed"])
    if profile == "communications-page-fields":
        obj=result if isinstance(result,dict) else {}
        integer=lambda key:obj.get(key) if type(obj.get(key)) is int and 0<=obj[key]<=2**64-1 else None
        return dict(events=obj.get("events") if isinstance(obj.get("events"),list) else None,cursor=integer("cursor"),latest=integer("latest"),session=obj.get("session") if isinstance(obj.get("session"),str) else None)
    if profile == "ignore-successful-body":return {}
    if profile == "raid-members-display":
        fields=result if isinstance(result,dict) else {}
        def field(value,key):return value.get(key) if isinstance(value,dict) else None
        def text(value,key):
            value=field(value,key)
            return value if isinstance(value,str) else "-"
        def plain(value):return "".join(f"\\u{ord(c):04x}" if ord(c)<32 or 127<=ord(c)<160 or ord(c) in (0x2028,0x2029) or 0x202a<=ord(c)<=0x202e or 0x2066<=ord(c)<=0x2069 else c for c in value)
        members=[]
        values=fields.get("members")
        for member in values if isinstance(values,list) else []:
            options=field(member,"options")
            choices=", ".join(plain(f"{key}={value if isinstance(value,str) else '?'}") for key,value in sorted(options.items())) if isinstance(options,dict) else ""
            members.append(dict(**{key:text(member,key) for key in ("id","agent_name","name","role","state")},owner=field(member,"owner") is True,created=isinstance(field(member,"created_by"),str),options=choices.replace(", ",",") if choices else "-"))
        return dict(**{key:text(fields,key) for key in ("id","name","former_owner")},paused=fields.get("paused") is True,members=members)
    if profile == "permission-list-records":
        return [normalize_reply("permission-record-fields",v) for v in result]
    if profile == "permission-record-fields":
        order=("kind","id","session","agent_name","approval","package","reason","state","approved","preview","message","name","started_at","log")
        fields=dict(zip(order,result)) if isinstance(result,list) else result.copy()
        fields.setdefault("kind","package")
        for key in ("approved","preview","message"):fields.setdefault(key,None)
        return {key:value for key,value in fields.items() if key in order}
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
    if profile == "permission-decision-fields":
        return dict(accepted=fields.get("accepted") if type(fields.get("accepted")) is bool else None,approved=fields.get("approved") if type(fields.get("approved")) is bool else None,request=text("request"))
    if profile == "permission-grant-fields":
        if isinstance(result,list):
            result=dict(zip(("request","status","message"),result))
        return dict(request=result["request"],status=result["status"],message=result.get("message"))
    if profile in ("permission-request-fields", "editor-open-fields"):
        code=fields.get("code")
        normalized=dict(code=code if type(code) is int and -(2**63)<=code<2**63 else None,message=text("message"))
        if profile == "permission-request-fields":normalized.update(request=text("request"),status=text("status"))
        return normalized
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


def host_command_bytes(validator, maximum, instance, schema):
    if not isinstance(instance,dict):return
    strings=instance.get("argv",[])+[instance.get("cwd","")]+[v for pair in instance.get("env",[]) for v in pair]
    if all(isinstance(v,str) for v in strings) and sum(len(v.encode("utf-8")) for v in strings)>maximum:
        yield ValidationError(f"host command exceeds {maximum} UTF-8 bytes")


def equal_fields(validator, pointers, instance, schema):
    """Require equal values at the declared JSON object pointers.

    Draft 7 cannot compare two instance values. The canonical schema uses
    this extension in the nonempty inbox.next branch; DTO/legacy decoding
    and runtime authority remain unchanged.
    """
    if not isinstance(instance,dict):return
    missing=object()
    def value_at(pointer):
        value=instance
        for field in pointer.split("/")[1:]:
            field=field.replace("~1","/").replace("~0","~")
            if not isinstance(value,dict) or field not in value:return missing
            value=value[field]
        return value
    values=[value_at(pointer) for pointer in pointers]
    if any(value is missing for value in values) or any(value!=values[0] for value in values[1:]):
        yield ValidationError("correlated fields must be present and equal: "+", ".join(pointers))


Validator = validators.extend(Draft7Validator, {"x-max-bytes": byte_limit, "x-json-integer": integer_representation,"x-host-command-bytes":host_command_bytes,"x-equal-fields":equal_fields})


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
    return Validator({**resolve(schema), "definitions": common()["definitions"], "x-envelope-schemas": common()["x-envelope-schemas"], "x-positional-schemas": common()["x-positional-schemas"], "x-client-result-schemas": common()["x-client-result-schemas"]})


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
        field=nested["field"]
        if field not in params:continue
        normalize=lambda record:dict(zip(nested["order"],record)) if isinstance(record,list) else record
        params[field]=[normalize(record) for record in params[field]] if nested.get("array-items",True) else normalize(params[field])
    if "x-request-constraints" in entry:schema_validator(entry["x-request-constraints"]).validate(params)
    return True


def validate_notification(endpoint, direction, notification, compatibility=False):
    if not isinstance(notification,dict):return False
    private=endpoint in PRIVATE_ENDPOINTS
    concrete=("daemon-to-supervisor","supervisor-to-daemon")
    entry = next((n for n in document(endpoint).get("x-notifications", [])
                  if n["name"] == notification.get("method") and
                  (n["direction"] == direction or
                   (private and n["direction"] == "bidirectional" and direction in concrete))), None)
    if entry is None: return False
    if compatibility and entry.get("x-client-normalization") == "host-submitted-legacy":
        params=notification.get("params",{})
        request=params.get("request") if isinstance(params,dict) else None
        schema_validator(entry["x-client-params-schema"]).validate(dict(request=request if isinstance(request,str) else ""))
        return True
    if compatibility and entry.get("x-client-normalization") == "attach-waiting-legacy":
        params=notification.get("params",{})
        missing=params.get("missing") if isinstance(params,dict) else None
        schema_validator(entry["x-client-params-schema"]).validate(dict(has_missing=isinstance(missing,list) and bool(missing)))
        return True
    if set(notification) - {"jsonrpc", "method", "params"} or notification.get("jsonrpc") != "2.0":
        raise ValidationError("invalid notification envelope")
    if private and "params" not in notification:
        raise ValidationError("missing control field params")
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


def validate_reply(endpoint, name, reply, compatibility=False, client_profile=None):
    """Unmigrated methods remain pending; covered methods must match exactly."""
    entry = method(endpoint, name)
    if entry is None:
        return False
    if client_profile is not None:
        if not compatibility:raise ValueError("client profile requires compatibility validation")
        entry=next(p for p in entry["x-client-result-profiles"] if p["name"]==client_profile)
    shape = "Failure" if "error" in reply else "Success"
    schema_validator(common()["x-envelope-schemas"][shape]).validate(reply)
    if "error" in reply:
        error = reply["error"]
        codes = document(endpoint)["x-common-errors"] + [e["code"] for e in method(endpoint,name)["errors"]]
        if error["code"] not in codes:
            raise ValidationError(f"unlisted error for {endpoint}/{name}: {error['code']}")
    else:
        result = reply["result"]
        if compatibility and "x-client-result-normalization" in entry:
            schema_validator(entry["x-client-wire-result-schema"]).validate(result)
            result = normalize_reply(entry["x-client-result-normalization"],result)
        schema = entry["x-client-result-schema"] if compatibility and "x-client-result-schema" in entry else entry["result"]["schema"]
        schema_validator(schema).validate(result)
        if name == "initialize" and (result["api"] != common()["x-api-version"] or result["role"] != endpoint):
            raise ValidationError("incompatible initialization result")
        if name == "initialize" and endpoint == "host" and result.get("configuration") is not None:
            raise ValidationError("host configuration must be null")
    return True


def reference():
    lines = ["#+title: Generated JSON-RPC reference (covered surfaces)", "", "Generated by =just api-reference=. Do not edit. Unmigrated surfaces remain in [[file:API.org][API.org]] and =libs/api/schema/coverage.json=.", ""]
    for endpoint in ENDPOINTS + PRIVATE_ENDPOINTS:
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
            for profile in entry.get("x-client-param-profiles",[]):
                lines.extend([f"Client parameter profile ={profile['name']}=:","",profile["description"],"","#+begin_src json",json.dumps(resolve(profile["x-params-schema"]),indent=2),"#+end_src",""])
            for profile in entry.get("x-client-result-profiles",[]):
                lines.extend([f"Client result profile ={profile['name']}=:","",profile["description"],"","Accepted wire result:","","#+begin_src json",json.dumps(resolve(profile["x-client-wire-result-schema"]),indent=2),"#+end_src","","Normalized generated client DTO:","","#+begin_src json",json.dumps(resolve(profile["x-client-result-schema"]),indent=2),"#+end_src",""])
            if "x-request-constraints" in entry:
                lines.extend(["Cross-field request constraints (existing semantic validation):","","#+begin_src json",json.dumps(resolve(entry["x-request-constraints"]),indent=2),"#+end_src",""])
        for entry in document(endpoint).get("x-notifications", []):
            lines.extend([f"** Notification ={entry['name']}=", "", entry["description"], "", f"Direction: ={entry['direction']}=. Sender: {entry['sender']}. Receiver: {entry['receiver']}. State: {entry['connection-state']}.", "", "Named parameters:", "", "#+begin_src json", json.dumps(resolve(entry.get("x-wire-params-schema", entry["x-params-schema"])), indent=2), "#+end_src", "", "Positional parameters:", "", "#+begin_src json", json.dumps(resolve(entry["x-positional-schema"]), indent=2), "#+end_src", ""])
            if "x-client-normalization" in entry:
                behavior="host-submitted-legacy-reader" if entry["x-client-normalization"]=="host-submitted-legacy" else "attach-client-compatibility"
                lines.extend([f"Legacy client classification follows ={behavior}=; the normalized DTO is:","","#+begin_src json",json.dumps(resolve(entry["x-client-params-schema"]),indent=2),"#+end_src",""])
        for entry in document(endpoint).get("x-completions",[]):
            lines.extend([f"** Completion exchange ={entry['name']}=","",entry["description"],"",f"Socket: ={entry['socket']}=. Direction: ={entry['direction']}=. Fixed response ID: ={entry['id']}=.","","#+begin_src json",json.dumps(resolve(entry["result"]["schema"]),indent=2),"#+end_src","","Errors: "+", ".join(str(e["code"]) for e in entry["errors"])+".",""])
    for endpoint in PRIVATE_ENDPOINTS:
        phases=document(endpoint).get("x-phases",[])
        if phases:
            lines.extend([f"* {endpoint} actor method phases", "", "These gates inspect method identity before logical decoding. Pending identities name an existing gate without claiming migration of its payload.", "", "#+begin_src json", json.dumps(phases,indent=2), "#+end_src", ""])
    for endpoint in PRIVATE_ENDPOINTS:
        pending=document(endpoint).get("x-pending-notifications",{})
        if pending:
            lines.extend([f"* {endpoint} remaining notification chunks", "", "These existing messages remain assigned to later migration slices; their payloads are not claimed as generated yet.", "", "#+begin_src json", json.dumps(pending,indent=2), "#+end_src", ""])
    if common().get("x-rust-serde-records"):
        lines.extend(["* Generated Serde reader details","","Original Serde container names, field order, required nullable sequence slots and legacy named aliases are generated from:","","#+begin_src json",json.dumps(common()["x-rust-serde-records"],indent=2),"#+end_src",""])
    if common().get("x-supervisor-metadata-fallbacks"):
        lines.extend(["* Supervisor generation boundaries", ""])
        lines.extend(f"- ={name}= :: {reason}" for name,reason in common()["x-supervisor-metadata-fallbacks"].items())
        lines.append("")
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
class RustToken:
    __slots__ = ("value", "begin", "finish", "literal")

    def __init__(self, value, begin, finish, literal=False):
        self.value, self.begin, self.finish, self.literal = value, begin, finish, literal

    def group(self):
        return self.value

    def start(self):
        return self.begin

    def end(self):
        return self.finish


RUST_RAW = re.compile(r'(?:br|cr|r)(#*)"')
RUST_STRING = re.compile(r'(?:b|c)?"')
RUST_CHAR = re.compile(r"(?:b)?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\])'")
RUST_IDENT = re.compile(r'[A-Za-z_][A-Za-z0-9_]*')


@lru_cache(maxsize=16)
def rust_lex(source):
    """Small shared lexer for current routing/construction syntax, not rustc."""
    tokens=[];i=0
    while i<len(source):
        if source[i].isspace():i+=1;continue
        if source.startswith('//',i):
            end=source.find('\n',i);i=len(source) if end<0 else end+1;continue
        if source.startswith('/*',i):
            depth=1;i+=2
            while depth and i<len(source):
                if source.startswith('/*',i):depth+=1;i+=2
                elif source.startswith('*/',i):depth-=1;i+=2
                else:i+=1
            if depth:raise ValueError('unclosed Rust comment')
            continue
        begin=i
        raw=RUST_RAW.match(source,i)
        if raw:
            close='"'+raw[1];end=source.find(close,raw.end())
            if end<0:raise ValueError('unclosed Rust raw string')
            i=end+len(close);tokens.append(RustToken('<literal>',begin,i,True));continue
        string=RUST_STRING.match(source,i)
        if string:
            i=string.end()
            while i<len(source) and source[i]!='"':i+=2 if source[i]=='\\' else 1
            if i>=len(source):raise ValueError('unclosed Rust string')
            i+=1
            value=source[begin:i] if source[begin]=='"' else '<literal>'
            tokens.append(RustToken(value,begin,i,True));continue
        char=RUST_CHAR.match(source,i)
        if char:
            i=char.end();tokens.append(RustToken('<literal>',begin,i,True));continue
        ident=RUST_IDENT.match(source,i)
        if ident:
            i=ident.end();tokens.append(RustToken(ident[0],begin,i));continue
        if source.startswith('::',i):
            i+=2;tokens.append(RustToken('::',begin,i));continue
        i+=1;tokens.append(RustToken(source[begin:i],begin,i))
    return tuple(tokens)


class RustTokens:
    def finditer(self, text, start=0):
        return (token for token in rust_lex(text) if token.start()>=start)


RUST_TOKEN = RustTokens()


@lru_cache(maxsize=16)
def rust_code_mask(text):
    """Keep code positions; blank literal contents/comments and test items."""
    tokens=rust_lex(text);values=[t.value for t in tokens]
    chars=['\n' if c=='\n' else ' ' for c in text]
    for token in tokens:
        if not token.literal:chars[token.start():token.end()]=text[token.start():token.end()]
    i=0
    while i<len(tokens):
        if values[i:i+7]!=['#','[','cfg','(','test',')',']']:
            i+=1;continue
        start=i;i+=7;depth=0
        while i<len(tokens):
            value=values[i]
            if value in ('(','['):depth+=1
            elif value in (')',']'):depth-=1
            elif not depth and value==';':break
            elif not depth and value=='{':
                i=rust_block_end(values,i);break
            i+=1
        if i>=len(tokens):raise ValueError('unterminated test item')
        for offset in range(tokens[start].start(),tokens[i].end()):
            if chars[offset]!='\n':chars[offset]=' '
        i+=1
    return ''.join(chars)




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
    signature = re.search(r'\bfn\s+' + re.escape(name) + r'\s*\(', rust_code_mask(text))
    if signature is None: raise ValueError(f"missing routing function {name}")
    return rust_block(text, signature.end())


def matches_patterns(text):
    """Read the pattern argument, including singleton and nested matches! calls."""
    for macro in re.finditer(r'\bmatches!\s*\(', rust_code_mask(text)):
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
    code = rust_code_mask(text)
    names = set()
    for endpoint, rust in re.findall(r'contract::(?:notification|notification_value)::<\s*contract::(host|sandbox|attach)_notifications::(\w+)\s*,?\s*>', code):
        matches = [n["name"] for n in document(endpoint).get("x-notifications", []) if n["x-rust-name"] == rust]
        if len(matches) != 1: raise ValueError("unassigned generated notification emitter")
        names.add(matches[0])
    for macro in re.finditer(r'\bjson!\s*\(\s*\{', code):
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


def generated_name(endpoint,rust,notification=False):
    entries=document(endpoint).get("x-notifications",[]) if notification else document(endpoint)["methods"]
    matches=[entry["name"] for entry in entries if entry["x-rust-name"]==rust]
    if len(matches)!=1:raise ValueError("unassigned generated "+("notification " if notification else "")+"identity "+endpoint+"/"+rust)
    return matches[0]


def method_names(text):
    # Routing patterns/comparisons and actual typed notification operations.
    # Test items, comments and literal contents never supply Rust identities.
    code=rust_code_mask(text);names=set()
    method_pattern=r'contract::(host|sandbox)::(\w+)::NAME'
    notification_pattern=r'contract::(host|sandbox|attach)_notifications::(\w+)::NAME'
    for pattern in matches_patterns(text):
        for token in RUST_TOKEN.finditer(pattern):
            match=re.fullmatch(r'"([a-z]+\.[a-z_-]+)"',token.group())
            if match:names.add(match[1])
        for endpoint,rust in re.findall(method_pattern,rust_code_mask(pattern)):
            names.add(generated_name(endpoint,rust))
        for endpoint,rust in re.findall(notification_pattern,rust_code_mask(pattern)):
            names.add(generated_name(endpoint,rust,True))
    tokens=list(RUST_TOKEN.finditer(text))
    for i,token in enumerate(tokens):
        if not code[token.start():token.end()].strip() and not token.literal:continue
        match=re.fullmatch(r'"([a-z]+\.[a-z_-]+)"',token.group())
        # Real string tokens inside excluded test items are blank in code;
        # their neighboring code is blank too. Filter by function/test mask.
        if match:
            before,after=code[:token.start()],code[token.end():]
            previous=[t.group() for t in tokens[max(0,i-2):i]]
            routing=(re.search(r'(?:==|!=|\|)\s*$',before) or
                     re.match(r'\s*(?:=>|\|)',after) or
                     previous==['"method"',':'] and code[tokens[i-1].start():tokens[i-1].end()]==':' or
                     text.strip()==token.group())
            if routing:names.add(match[1])
    for regex,is_notification in [(method_pattern,False),(notification_pattern,True)]:
        for match in re.finditer(regex,code):
            before,after=code[:match.start()],code[match.end():]
            if (re.search(r'(?:==|!=|\|)\s*(?:Some\(\s*)?$',before) or
                re.match(r'\s*(?:=>|\||==|!=)',after) or code.strip()==match.group()):
                names.add(generated_name(match[1],match[2],is_notification))
    for endpoint,rust in re.findall(r'contract::(?:decode_notification|notification|notification_value)::<\s*contract::(host|sandbox|attach)_notifications::(\w+)\s*,?\s*>',code):
        names.add(generated_name(endpoint,rust,True))
    return names


def completion_names(text):
    code=rust_code_mask(text)
    found=set()
    for rust in re.findall(r'contract::(?:decode_completion|completion_payload|completion_error)::<\s*contract::attach_exchanges::(\w+)\s*,?\s*>',code):
        matches=[e["name"] for e in document("attach")["x-completions"] if e["x-rust-name"]==rust]
        if len(matches)!=1:raise ValueError("unassigned generated completion identity")
        found.add(matches[0])
    return found


def dispatch_pattern_names(text, selector="method"):
    """Read only arm headers of the function-returning dispatch match."""
    code=rust_code_mask(text)
    candidates=[]
    for match in re.finditer(r'\bmatch\s+'+re.escape(selector)+r'\s*\{',code):
        prefix=code[:match.start()]
        if prefix.count('{')!=prefix.count('}'):continue
        body=rust_block(text,match.end()-1)
        end=match.end()+len(body)+1
        if code[end:].strip() not in ("",";"):continue
        candidates.append(body)
    if len(candidates)!=1:raise ValueError("missing unique returning method dispatch")
    body=candidates[0];tokens=list(RUST_TOKEN.finditer(body));values=[t.group() for t in tokens]
    names=set();i=0
    while i<len(tokens):
        while i<len(tokens) and values[i]==',':i+=1
        if i>=len(tokens):break
        start=i;depth=0
        while i+1<len(tokens):
            value=values[i]
            if not depth and values[i:i+2]==['=','>']:break
            if value in ('(','[','{'):depth+=1
            elif value in (')',']','}'):depth-=1
            i+=1
        else:raise ValueError("unrecognized method dispatch arm")
        names.update(method_names(body[tokens[start].start():tokens[i].start()]))
        i+=2
        if i>=len(tokens):raise ValueError("empty method dispatch arm")
        if values[i]=='{':
            i=rust_block_end(values,i)+1
        else:
            if values[i] in ('if','match','loop','unsafe'):
                raise ValueError("unsupported unbraced method dispatch body")
            depth=0
            while i<len(tokens):
                value=values[i]
                if not depth and value==',':i+=1;break
                if value in ('(','[','{'):depth+=1
                elif value in (')',']','}'):depth-=1
                i+=1
    return names


def require_state_update_reader(source):
    """Tie the notification identity to tick's actual rejection condition."""
    body=rust_function(source,"tick")
    tokens=rust_production_lex(body);values=[t.group() for t in tokens]
    comparison=['value','.','method','.','as_deref','(',')','!','=','Some','(',
                'contract','::','host_notifications','::','StateChanged','::','NAME',')']
    rejection=['return','Err','(','"subscription gap; reconnect for a fresh snapshot"']
    for start,value in enumerate(values):
        if value!='if':continue
        i=start+1;depth=0
        while i<len(tokens):
            value=values[i]
            if not depth and value=='{':break
            if value in ('(','[','{'):depth+=1
            elif value in (')',']','}'):depth-=1
            i+=1
        if i>=len(tokens):continue
        end=rust_block_end(values,i)
        if rust_contains(values[start+1:i],comparison) and rust_contains(values[i+1:end],rejection):
            return
    raise ValueError("missing notification peer in subscription tick rejection: state.changed")


def implementation_methods(sources):
    controller = sources["controller"]
    dispatch = rust_function(controller, "dispatch")
    dispatch_code=rust_code_mask(dispatch)
    start=dispatch_code.index("let messaging =")
    messaging=dispatch[start:dispatch_code.index(";",start)]
    shared = method_names(rust_function(sources["gate"], "messaging"))
    if method_names(messaging) != shared | {"communications.list"}:
        raise ValueError("host/sandbox messaging routing changed")
    roles = dispatch[dispatch_code.rindex("match &c.role"):]
    roles_code=rust_code_mask(roles)
    split=roles_code.find("Role::Sandbox(session) =>")
    if split<0:raise ValueError("missing sandbox dispatch branch")
    host_role,separator=roles[:split],"Role::Sandbox(session) =>"
    sandbox_role = rust_block(roles, len(host_role) + len(separator))
    host = (dispatch_pattern_names(rust_function(controller, "host")) |
            dispatch_pattern_names(rust_function(sources["raids"], "host_raids")) |
            dispatch_pattern_names(rust_function(sources["terminals"], "terminals")) |
            method_names(host_role) | method_names(messaging) | {"initialize"})
    refused = {"sessions.attach"}
    sandbox = (method_names(sandbox_role) |
               dispatch_pattern_names(rust_function(controller, "sandbox_control")) |
               dispatch_pattern_names(rust_function(sources["raids"], "sandbox_raids")) |
               method_names(rust_function(sources["terminals"], "sandbox_terminals")) |
               shared | {"initialize"}) - refused
    gate = rust_function(sources["gate"], "check")
    allowed = re.search(r'if\s+!matches!\(\s*method\s*,(.*?)\)\s*\{', rust_code_mask(gate), re.S)
    if allowed is None: raise ValueError("missing sandbox method allowlist")
    admitted = method_names(gate[allowed.start(1):allowed.end(1)]) | shared | {"initialize"}
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
    if "contract::host_submitted(" not in rust_code_mask(sources["sandbox_host_client"]):raise ValueError("missing notification peer: host.submitted")
    require_state_update_reader(sources["host_client"])
    for source, name in [("exec_client", "exec.signal")]:
        if name not in method_names(sources[source]): raise ValueError(f"missing notification peer: {name}")
    if "attach.hello" not in method_names(rust_function(sources["attach_server"],"accept")) or "contract::attach_waiting(" not in rust_code_mask(sources["attach_client"]):
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

    if registry["pending"] or registry["notifications"] or registry["exchanges"]:
        raise ValueError("public coverage has pending contract assignments")


def check_nested_params(entry,params,wire):
    if set(params["properties"])!=set(wire["properties"]) or params["required"]!=wire["required"]:raise ValueError("wire and generated parameter fields differ")
    expected=copy.deepcopy(params)
    seen=set()
    for row in entry.get("x-nested-positional-params",[]):
        field,schema,order,minimum=(row[k] for k in ("field","schema","order","minimum"))
        if field in seen or field not in params["properties"]:raise ValueError("invalid nested positional field")
        seen.add(field)
        record=common()["definitions"][schema]
        array_items=row.get("array-items",True)
        if type(array_items) is not bool:raise ValueError("invalid nested positional mode")
        if array_items:
            collection=resolve({"$ref":"common.json"+params["properties"][field]["$ref"]})
            if collection.get("type")!="array" or collection["items"]!={"$ref":"#/definitions/"+schema}:raise ValueError("nested positional field is not an array of its record")
        elif params["properties"][field]!={"anyOf":[{"$ref":"#/definitions/"+schema},{"type":"null"}]}:
            raise ValueError("nested positional field is not its optional record")
        if len(order)!=len(set(order)) or set(order)!=set(record["properties"]) or minimum!=max((i+1 for i,name in enumerate(order) if name in record["required"]),default=0):raise ValueError("nested positional order/minimum differs")
        positional={"type":"array","items":[record["properties"][name] for name in order],"minItems":minimum,"maxItems":len(order)}
        if common()["x-positional-schemas"][schema]!=positional:raise ValueError("nested positional schema differs")
        if array_items:
            expected["properties"][field]={**collection,"items":{"anyOf":[collection["items"],{"$ref":"#/x-positional-schemas/"+schema}]}}
        else:
            expected["properties"][field]={"anyOf":[{"$ref":"#/definitions/"+schema},{"$ref":"#/x-positional-schemas/"+schema},{"type":"null"}]}
    if expected!=wire:raise ValueError("wire parameters differ from declared nested normalization")


def check_references(documents=None):
    """Resolve every nested reference from its actual containing document.

    This check never injects definitions into OpenRPC documents or retrieves
    network resources. It is separate from standalone fixture validation.
    """
    if documents is None:
        documents={"common.json":common(), **{f"{endpoint}.openrpc.json":document(endpoint) for endpoint in ENDPOINTS + PRIVATE_ENDPOINTS}}
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


def elisp_readers():
    """Lazy record views preserve plist-get errors and field evaluation order."""
    lines=[";;; goblins-api-generated.el --- Generated API readers -*- lexical-binding: t; -*-",
           ";; Generated by just api-reference from libs/api/schema/common.json.",
           ";; Lazy field access preserves partial/historical payloads and plist errors.",
           "(require 'cl-lib)", ""]
    for row in common()["x-elisp-readers"]:
        name="goblins-api-"+row["name"]
        lines += [f"(cl-defstruct ({name} (:constructor {name} (raw))) raw)", ""]
        for field in row["fields"]:
            accessor=name+"-"+field.replace("_","-")
            lines += [f"(defun {accessor} (record)",
                      f'  "Read {field} from the compatible {row["name"]} view RECORD."',
                      f"  (plist-get ({name}-raw record) :{field}))", ""]
    for row in common().get("x-elisp-identities",[]):
        lines += [f'(defconst {row["name"]} \'{row["method"]})', ""]
    lines += ["(provide 'goblins-api-generated)", ";;; goblins-api-generated.el ends here", ""]
    return "\n".join(lines)


def rust_production_lex(source):
    """Keep literal tokens intact while excluding actual cfg(test) items."""
    tokens=rust_lex(source)
    values=[token.value for token in tokens]
    result=[];i=0
    while i<len(tokens):
        if values[i:i+7]==['#','[','cfg','(','test',')',']']:
            i+=7;depth=0
            while i<len(tokens):
                value=values[i]
                if value in ('(','['):depth+=1
                elif value in (')',']'):depth-=1
                elif not depth and value==';':i+=1;break
                elif not depth and value=='{':
                    i=rust_block_end(values,i)+1;break
                i+=1
        else:result.append(tokens[i]);i+=1
    return result


def rust_production_tokens(source):
    """Code tokens for the construction guard, using the shared literal lexer."""
    return ['<literal>' if token.literal else token.value
            for token in rust_production_lex(source)]


def rust_block_end(tokens,start):
    depth=0
    for i in range(start,len(tokens)):
        if tokens[i]=='{':depth+=1
        elif tokens[i]=='}':
            depth-=1
            if not depth:return i
    raise ValueError('unclosed Rust body')


def rust_functions(tokens):
    functions=[]
    for i,token in enumerate(tokens):
        if token!='fn' or i+1>=len(tokens) or not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*',tokens[i+1]):continue
        depth=0
        for j in range(i+2,len(tokens)):
            value=tokens[j]
            if value in ('(','['):depth+=1
            elif value in (')',']'):depth-=1
            elif not depth and value==';':break
            elif not depth and value=='{':
                functions.append((tokens[i+1],i,j,rust_block_end(tokens,j)));break
    return functions


def rust_contains(tokens,pattern):
    return any(tokens[i:i+len(pattern)]==pattern for i in range(len(tokens)-len(pattern)+1))


def communication_constructors(tokens):
    aliases={'communication_events'}
    for i,token in enumerate(tokens):
        if token=='use':
            try:end=tokens.index(';',i)
            except ValueError:raise ValueError('unterminated Rust import')
            path=tokens[i+1:end]
            if len(path)>=4 and path[-2]=='as' and path[:-2] in (
                ['contract','::','communication_events'],
                ['goblins_protocol','::','contract','::','communication_events'],
            ):aliases.add(path[-1])
    calls=[]
    for i,token in enumerate(tokens):
        if token=='new' and tokens[max(0,i-2):i]==['CommunicationPayload','::']:
            start=i+1
        elif token=='record_typed' and i>0 and tokens[i-1]=='.':start=i+1
        else:continue
        if tokens[start:start+2]!=['::','<']:raise ValueError('unrecognized communication construction')
        try:end=tokens.index('>',start+2)
        except ValueError:raise ValueError('unclosed communication construction')
        path=tokens[start+2:end]
        if path[-1:]==[',']:path=path[:-1]
        if tokens[end+1:end+2]!=['(']:raise ValueError('communication binding is not called')
        # K is the single generic constructor inside Mailbox.record_typed.
        if path==['K']:calls.append((i,None));continue
        if len(path)<3 or path[-2]!='::':raise ValueError('unresolved communication constructor')
        namespace=path[:-2]
        direct=namespace in (['contract','::','communication_events'],
                            ['goblins_protocol','::','contract','::','communication_events'])
        aliased=len(namespace)==1 and namespace[0] in aliases
        if not (direct or aliased):raise ValueError('unresolved communication constructor alias')
        calls.append((i,path[-1]))
    return calls


def check_communication_construction(catalog,read_source):
    bindings={row['x-rust-name'] for row in catalog['x-communications-payloads']}
    coverage=set();raw_calls=[];mailbox_functions=None;mailbox_tokens=None
    for row in catalog['x-communications-producers']:
        tokens=rust_production_tokens(read_source(row['source']));functions=rust_functions(tokens)
        expected=row['functions'];found={name:set() for name in expected}
        if set(row['bindings'])!=set().union(*(set(v) for v in expected.values())):raise ValueError('producer function catalog differs')
        for position,binding in communication_constructors(tokens):
            owners=[name for name,_,start,end in functions if start<position<end]
            if len(owners)!=1:raise ValueError('construction is outside a producer function')
            owner=owners[0]
            if binding is None:
                if not row['source'].endswith('/mailbox.rs') or owner!='record_typed':raise ValueError('unbound communication constructor')
            elif owner not in found:raise ValueError('construction is outside its publication path')
            else:found[owner].add(binding)
        if found!={name:set(v) for name,v in expected.items()}:raise ValueError('communication producer constructors differ: '+row['source'])
        for i in range(len(tokens)-2):
            if tokens[i] in ('.','::') and tokens[i+1:i+3]==['record','(']:
                owners=[name for name,_,start,end in functions if start<i<end]
                raw_calls.append((row['source'],owners))
        coverage.update(set().union(*found.values()))
        if row['source'].endswith('/mailbox.rs'):mailbox_functions=functions;mailbox_tokens=tokens
    if coverage!=bindings:raise ValueError('communication producer coverage differs from kind registry')
    if raw_calls!=[('packages/daemon/src/mailbox.rs',['record_typed'])]:raise ValueError('raw communication publication bypasses typed boundary')
    records=[item for item in mailbox_functions if item[0]=='record']
    if len(records)!=1:raise ValueError('raw communication publication boundary missing')
    _,decl,_,_=records[0]
    header=decl
    while header and mailbox_tokens[header-1] not in ('{','}',';'):header-=1
    if 'pub' in mailbox_tokens[header:decl]:raise ValueError('raw communication publication is public')
    wrappers=[item for item in mailbox_functions if item[0]=='record_typed']
    if len(wrappers)!=1:raise ValueError('typed communication wrapper missing')
    _,_,start,end=wrappers[0];body=mailbox_tokens[start+1:end]
    if not rust_contains(body,['contract','::','CommunicationPayload','::','new','::','<','K','>','(','data',')','.','into_parts','(',')']):raise ValueError('typed communication wrapper does not bind payload')
    if not rust_contains(body,['self','.','record','(','actor',',','kind',',','data',',','now',',','audit',')']):raise ValueError('typed communication wrapper does not publish its bound pair')



def check_communication_readers_and_producers(*, sources=None, schema=None):
    catalog = common() if schema is None else schema
    def read_source(path):
        return sources[path] if sources is not None and path in sources else (ROOT/path).read_text()
    defs=catalog["definitions"]
    payloads=catalog["x-communications-payloads"]
    check_communication_construction(catalog,read_source)
    for row in catalog["x-communication-string-readers"]:
        props=defs[row["record"]]["properties"]
        if set(row["fields"])!=set(props):raise ValueError("compatible reader fields differ")
        binding=next((v for v in payloads if v["x-rust-name"]==row["binding"]),None)
        if binding is None or row["kind"]!=binding["kind"]:raise ValueError("compatible reader kind differs from generated binding")
        for field in props:
            if props[field]!={"anyOf":[{"type":"string"},{"type":"null"}]}:raise ValueError("compatible string projection differs")
    names=set()
    for row in catalog["x-elisp-readers"]:
        if row["name"] in names or not re.fullmatch(r"[a-z]+(?:-[a-z]+)*",row["name"]):raise ValueError("invalid Elisp record name")
        names.add(row["name"])
        props=set().union(*(defs[name].get("properties",{}) for name in row["schemas"]))
        legacy=row.get("legacy-fields",{})
        if set(legacy)&props or any(not v for v in legacy.values()):raise ValueError("invalid historical reader field")
        maps=row.get("map-fields",{})
        if maps and (any(not v for v in maps.values()) or not any(isinstance(defs[name].get("additionalProperties"),dict) or defs[name].get("additionalProperties") is True for name in row["schemas"])):raise ValueError("invalid map reader field")
        if set(maps)&(props|set(legacy)):raise ValueError("duplicate map reader field")
        if len(row["fields"])!=len(set(row["fields"])) or not set(row["fields"])<=props|set(legacy)|set(maps):raise ValueError("Elisp reader fields differ from schema")
        if any(not re.fullmatch(r"[a-z]+(?:_[a-z]+)*",field) or field=="raw" for field in row["fields"]):raise ValueError("invalid Elisp field")


def check_generated_reader_hooks_and_identities():
    for name,row in common()["x-rust-serde-records"].items():
        hooks=row.get("deserialize-with",{})
        expected={"sessions":"crate::contract::snapshot_sessions","permissions":"crate::contract::snapshot_permissions","raids":"crate::contract::snapshot_raids"} if name=="StateSnapshotReply" else {}
        if hooks!=expected:raise ValueError("snapshot field reader hook differs")
    names=set()
    for row in common().get("x-elisp-identities",[]):
        if row["name"] in names or not re.fullmatch(r"goblins-api-[a-z]+(?:-[a-z]+)*",row["name"]):raise ValueError("invalid generated Elisp identity")
        names.add(row["name"])
        entries=document(row["endpoint"])["methods" if row["kind"]=="method" else "x-notifications"]
        matches=[entry for entry in entries if entry["name"]==row["method"]]
        if len(matches)!=1 or row.get("direction")!=matches[0].get("direction"):raise ValueError("generated Elisp identity differs")


def check_supervisor_source_stdio_metadata():
    """Bind wire-only generation metadata to the original nested codec shapes."""
    def ref(name):return {"$ref":"#/definitions/"+name}
    def optional(schema):return {"anyOf":[schema,{"type":"null"}]}
    def uint(bits):return {"type":"integer","minimum":0,"maximum":2**bits-1,"x-json-integer":True}
    string={"type":"string"};boolean={"type":"boolean"}
    path=ref("SupervisorPath");descriptor=ref("SupervisorDescriptor")
    strings={"type":"array","items":string};serde=["Serialize","Deserialize"]
    def check(name,expected):
        actual=common()["definitions"][name]
        if {key:value for key,value in actual.items() if key!="description"}!=expected:
            raise ValueError("private nested schema/Rust metadata differs: "+name)
    def record(name,rust,fields,derives,closed=True,defaults=()):
        props={field:{**schema,"x-rust-field-name":field,"x-rust-type":ty,**({"x-rust-default":True} if field in defaults else {})} for field,ty,schema in fields}
        check(name,{"type":"object","properties":props,"required":[field for field,ty,_ in fields if not ty.startswith("Option<")],"additionalProperties":not closed,"x-rust-name":rust,"x-rust-field-order":list(props),"x-rust-derives":derives})
    def enum(name,rust,variants,derives=None):
        branches=[]
        for tag,fields in variants:
            props={"kind":{"type":"string","enum":[tag]}}
            for field,ty,schema,default in fields:
                props[field]={**schema,"x-rust-type":ty,**({"x-rust-default":True} if default else {})}
            branches.append({"type":"object","properties":props,"required":["kind"]+[field for field,ty,_,_ in fields if not ty.startswith("Option<")],"additionalProperties":False,"x-rust-field-order":[field for field,_,_,_ in fields]})
        check(name,{"oneOf":branches,"x-rust-name":rust,"x-rust-derives":serde if derives is None else derives})
    def fields(*rows):return [(*row,False) for row in rows]
    record("SupervisorWorktree","Worktree",[("name","String",string),("path","::std::path::PathBuf",path)],["Clone","Debug","PartialEq","Eq"]+serde)
    record("SupervisorRecreate","Recreate",[("packages","Vec<String>",strings),("auto_approve","Vec<String>",strings),("docker","bool",boolean),("variables","::std::collections::BTreeMap<String, String>",{"type":"object","additionalProperties":string}),("worktree","Option<crate::contract::Worktree>",optional(ref("SupervisorWorktree")))],["Clone","Debug"]+serde,closed=False,defaults=("worktree",))
    enum("SupervisorStdio","WireStdio",[
        ("Fds",fields(("fds","[crate::contract::Descriptor; 3]",{"type":"array","items":descriptor,"minItems":3,"maxItems":3}))),
        ("Tty",fields(("slave","crate::contract::Descriptor",descriptor))),
        ("NewPty",fields(("rows","u16",uint(16)),("cols","u16",uint(16))))],["Clone","Debug"]+serde)
    enum("SupervisorSource","WireSource",[
        ("Host",fields(("configuration","::std::path::PathBuf",path),("name","String",string),("scope","Option<String>",{"type":["string","null"]}),("dev_shell","Option<String>",{"type":["string","null"]}),("workspace","Option<::std::path::PathBuf>",optional(path)),("cwd","Option<::std::path::PathBuf>",optional(path)),("state","::std::path::PathBuf",path),("history","Option<::std::path::PathBuf>",optional(path)),("recreate","Option<crate::contract::Recreate>",optional(ref("SupervisorRecreate"))),("sequence","u64",ref("SupervisorSequence")))),
        ("Member",fields(("scope","Option<crate::contract::Descriptor>",optional(descriptor)),("owner_scope","Option<String>",{"type":["string","null"]}),("manifest","::std::path::PathBuf",path),("configuration","String",string),("auto","::std::collections::BTreeMap<String, ::std::path::PathBuf>",{"type":"object","additionalProperties":path}),("state","::std::path::PathBuf",path),("history","Option<::std::path::PathBuf>",optional(path)))+[
            ("worktree","Option<::std::path::PathBuf>",optional(path),True),("arguments","Option<Vec<String>>",optional(strings),True)])],["Clone","Debug"]+serde)


def check_supervisor_handoff_metadata():
    """Check generated handoff records and canonical/read bootstrap boundaries."""
    defs=common()["definitions"]
    def ref(name):return {"$ref":"#/definitions/"+name}
    def optional(schema):return {"anyOf":[schema,{"type":"null"}]}
    def record(name,rust,fields):
        props={field:{**schema,"x-rust-field-name":field,"x-rust-type":ty} for field,ty,schema in fields}
        expected={"type":"object","properties":props,"required":[field for field,ty,_ in fields if not ty.startswith("Option<")],"additionalProperties":False,"x-rust-name":rust,"x-rust-field-order":list(props),"x-rust-derives":["Clone","Debug","Serialize","Deserialize"]}
        if {k:v for k,v in defs[name].items() if k!="description"}!=expected:
            raise ValueError("private handoff schema/Rust metadata differs: "+name)
    path=ref("SupervisorPath");descriptor=ref("SupervisorDescriptor")
    record("SupervisorDirectory","WireDirectory",[("fd","crate::contract::Descriptor",descriptor),("source","::std::path::PathBuf",path),("destination","::std::path::PathBuf",path)])
    record("SupervisorLease","WireLease",[("directory","::std::path::PathBuf",path),("lock","crate::contract::Descriptor",descriptor)])
    record("SupervisorHandoff","WireHandoff",[("transfer","u64",ref("SupervisorSequence")),("count","usize",ref("SupervisorLeaseCount")),("workspace","crate::contract::Descriptor",descriptor),("working_directory","Option<crate::contract::WireDirectory>",optional(ref("SupervisorDirectory"))),("dev_shell","String",{"type":"string"}),("docker","bool",{"type":"boolean"}),("leases","Vec<crate::contract::WireLease>",{"type":"array","items":ref("SupervisorLease")}),("more","bool",{"type":"boolean"})])
    if {k:v for k,v in defs["SupervisorLeaseCount"].items() if k!="description"}!={"type":"integer","minimum":0,"maximum":2**64-1,"x-json-integer":True}:
        raise ValueError("private native lease count differs")
    if defs["SupervisorSourceSlot"]!=ref("SupervisorSource") or defs["SupervisorHandoffSlot"]!=optional(ref("SupervisorHandoff")):
        raise ValueError("private canonical bootstrap nested links differ")
    canonical=defs["SupervisorStartParams"]
    for field,slot in [("source","SupervisorSourceSlot"),("handoff","SupervisorHandoffSlot")]:
        if canonical["properties"][field]!=ref(slot):raise ValueError("private canonical bootstrap field link differs")
    expected=copy.deepcopy(canonical)
    expected["properties"]["source"]=ref("SupervisorSourceRead")
    expected["properties"]["handoff"]=ref("SupervisorHandoffRead")
    if defs["SupervisorStartRead"]!=expected:raise ValueError("private fieldwise bootstrap read projection differs")
    for name,shape in [("SupervisorSourceRead",{"type":"object"}),("SupervisorHandoffRead",{"type":["object","null"]})]:
        if {k:v for k,v in defs[name].items() if k!="description"}!=shape:
            raise ValueError("private raw bootstrap read slot differs")
    for name in ("SupervisorSourceSlot","SupervisorHandoffSlot","SupervisorSource","SupervisorHandoff"):
        if "x-generator-pending" in defs[name]:raise ValueError("private bootstrap nested contract remains pending")


def check_supervisor_phases():
    """Pin existing actor method gates and narrowly scoped pending identities."""
    expected=[
        ("entry","Entry","daemon-to-supervisor",["Setup","Maintenance","Preflight","SupervisorStart"],"libs/supervisor/src/entry.rs:main",{"Setup":"C18"}),
        ("maintenance-first","MaintenanceFirst","daemon-to-supervisor",["Maintenance"],"libs/supervisor/src/maintenance.rs:start",{}),
        ("preflight-adopted","PreflightAdopted","daemon-to-supervisor",["Adopted"],"libs/supervisor/src/preflight.rs:start",{}),
        ("manifest-result","ManifestResult","supervisor-to-daemon",["Event.Manifest","PreflightFailed"],"packages/daemon/src/supervised/oneshot.rs:Expected::allows",{}),
        ("maintenance-result","MaintenanceResult","supervisor-to-daemon",["MaintenanceFinished"],"packages/daemon/src/supervised/oneshot.rs:Expected::allows",{}),
    ]
    doc=document("supervisor");rows=doc.get("x-phases",[])
    if [row.get("name") for row in rows]!=[row[0] for row in expected]:
        raise ValueError("private supervisor phase inventory differs")
    notifications={row["name"]:row for row in doc["x-notifications"]}
    for row,(name,rust,direction,allowed,consumer,pending) in zip(rows,expected):
        if (row.get("x-rust-name")!=rust or row.get("direction")!=direction or
            row.get("allowed")!=allowed or row.get("consumer")!=consumer):
            raise ValueError("private supervisor phase contract differs: "+name)
        if not isinstance(row.get("description"),str) or not row["description"]:
            raise ValueError("missing private supervisor phase description")
        variants={name:name for name in allowed} if name=="entry" else None
        if (variants is not None and row.get("x-dispatch-variants")!=variants) or (variants is None and "x-dispatch-variants" in row):
            raise ValueError("private supervisor entry classifier differs")
        actual=row.get("pending-identities",{})
        if set(actual)!=set(pending):raise ValueError("private phase identity fallback scope differs")
        for identity in allowed:
            if identity in pending:
                record=actual[identity]
                if (identity in notifications or set(record)!={"chunk","reason"} or
                    record.get("chunk")!=pending[identity] or not isinstance(record.get("reason"),str) or not record["reason"]):
                    raise ValueError("invalid private phase pending identity")
            elif identity not in notifications or notifications[identity]["direction"] not in (direction,"bidirectional"):
                raise ValueError("private phase identity has no matching directional contract")


def check_supervisor_handoff_roots():
    """Bind launch/export/manifest root metadata and its original serde slots."""
    defs=common()["definitions"]
    def ref(name):return {"$ref":"#/definitions/"+name}
    def obj(props):return {"type":"object","properties":props,"required":list(props),"additionalProperties":False}
    def optional(schema):return {"anyOf":[schema,{"type":"null"}]}
    expected={
        "SupervisorCommandStartParams":obj({"handoff":ref("SupervisorHandoff")}),
        "SupervisorEventExportedParams":obj({"member":{"type":"string"},"result":ref("SupervisorHandoffOutcome")}),
        "SupervisorEventManifestParams":obj({"sequence":ref("SupervisorSequence"),"path":ref("SupervisorPath"),"root":optional(ref("SupervisorPath")),"catalogue":{"type":"string"},"owner":ref("SupervisorOptionalDescriptor"),"transfer":ref("SupervisorSequence")}),
        "SupervisorHandoffOutcome":{"oneOf":[obj({"Ok":ref("SupervisorHandoff")}),obj({"Err":{"type":"string"}})]},
    }
    for name,schema in expected.items():
        if {k:v for k,v in defs[name].items() if k!="description"}!=schema:
            raise ValueError("private root/outcome contract differs: "+name)
    record=common()["x-rust-serde-records"].get("SupervisorEventManifestParams")
    if record!={"name":"SupervisorEventManifestParams","order":["sequence","path","root","catalogue","owner","transfer"],"required-nullable":["root","owner"]}:
        raise ValueError("private manifest generated serde contract differs")


def check_supervisor_process_metadata():
    """Pin the original process metadata shapes and external SpawnKind enum."""
    schema=common();defs=schema["definitions"]
    def ref(name):return {"$ref":"#/definitions/"+name}
    def obj(props):return {"type":"object","properties":props,"required":list(props),"additionalProperties":False}
    def opt(value):return {"anyOf":[value,{"type":"null"}]}
    terminal={"type":"object","properties":{"exit_report":{**opt(ref("SupervisorSequence")),"x-rust-type":"Option<u64>"}},"required":[],"additionalProperties":True,"x-rust-field-order":["exit_report"]}
    expected={
        "SupervisorI32":{"type":"integer","minimum":-2**31,"maximum":2**31-1,"x-json-integer":True},
        "SupervisorOptionalI32":opt(ref("SupervisorI32")),
        "SupervisorOptionalArguments":opt({"type":"array","items":{"type":"string"}}),
        "SupervisorOptionalDirectory":opt({"type":"string"}),
        "SupervisorEnvironment":{"type":"array","items":{"type":"array","items":[{"type":"string"},{"type":"string"}],"minItems":2,"maxItems":2}},
        "SupervisorSpawnKind":{"oneOf":[{"type":"string","enum":["Exec","Attach"]},obj({"Terminal":terminal})],"x-rust-name":"SpawnKind","x-rust-derives":["Clone","Copy","Debug","PartialEq","Eq","Serialize","Deserialize"],"x-rust-variants":[{"name":"Exec","kind":"unit"},{"name":"Attach","kind":"unit"},{"name":"Terminal","kind":"struct","schema-ref":"#/definitions/SupervisorSpawnKind/oneOf/1/properties/Terminal"}]},
        "SupervisorCommandSpawnParams":obj({"id":ref("SupervisorSequence"),"kind":ref("SupervisorSpawnKind"),"argv":ref("SupervisorOptionalArguments"),"cwd":ref("SupervisorOptionalDirectory"),"env":ref("SupervisorEnvironment"),"stdio":ref("SupervisorStdio")}),
        "SupervisorCommandSignalParams":obj({"id":ref("SupervisorSequence"),"signal":ref("SupervisorI32")}),
        "SupervisorCommandCancelProcessParams":obj({"id":ref("SupervisorSequence"),"everything":{"type":"boolean"}}),
        "SupervisorEventSpawnedParams":obj({"id":ref("SupervisorSequence"),"pid":ref("SupervisorI32")}),
        "SupervisorEventSpawnFailedParams":obj({"id":ref("SupervisorSequence"),"code":ref("SupervisorOptionalI32"),"message":{"type":"string"}}),
        "SupervisorEventExitedParams":obj({"id":ref("SupervisorSequence"),"code":ref("SupervisorOptionalI32"),"signal":ref("SupervisorOptionalI32")}),
    }
    variant_docs = [
        "`goblins exec`.",
        "An attach client or its `onDetach`.",
        "A terminal the supervisor emulates (`terminals.create`), checked as an exec. With `exit_report` the supervisor answers that request ID with the exit and the last lines of the screen, before it reports the exit itself: a wait made from the start, which no exit can overtake.",
    ]
    for variant, description in zip(expected["SupervisorSpawnKind"]["x-rust-variants"], variant_docs):
        variant["description"] = description
    if schema.get("x-rust-external-enums")!=["SupervisorSpawnKind"]:
        raise ValueError("private external enum registry differs")
    for name,value in expected.items():
        if {k:v for k,v in defs[name].items() if k!="description"}!=value:
            raise ValueError("private process schema/Rust metadata differs: "+name)
    for name,nullable in [("SupervisorCommandSpawnParams",["argv","cwd"]),("SupervisorEventSpawnFailedParams",["code"]),("SupervisorEventExitedParams",["code","signal"])]:
        if schema["x-rust-serde-records"].get(name)!={"name":name,"order":list(expected[name]["properties"]),"required-nullable":nullable}:
            raise ValueError("private process generated serde slots differ")


def check_supervisor_pending_inventory():
    """Retire covered methods from their original remaining family, without loss."""
    families={
        "C17b2-attachments-terminals":["Command.Attach","Command.Detach","Command.Resize","Command.TerminalCall","Command.TerminalCancel","Event.Terminal","Event.TerminalResult"],
        "C17c-approvals-devshell":["Command.Cancel","Command.Preview","Command.Decide","Command.Refresh","Command.Run","Command.Diff","Event.Preview","Event.Progress","Event.Refresh","Event.Call","Event.DevShell","Event.Granted"],
        "C17d-lifecycle-host-jobs":["Command.ScopeFailed","Command.ScopeMembership","Command.Export","Command.PrepareHostJob","Command.RunHostJob","Command.CancelHostJob","Command.Stop","Event.ScopeFailed","Event.ScopeNeeded","Event.Started","Event.Mode","Event.HostJobPrepared","Event.HostJobFailed","Event.HostJobFinished","Event.Stopped","Event.Failed","Event.InitLost","Event.PayloadExited"],
        "C17e-relay-logging":["Command.RecordEvent","Command.SandboxSend","Command.SandboxClose","Event.SandboxOpened","Event.SandboxCall","Event.SandboxClosed"],
    }
    doc=document("supervisor");declared={row["name"] for row in doc["x-notifications"]}
    expected={group:[name for name in names if name not in declared] for group,names in families.items()}
    expected={group:names for group,names in expected.items() if names}
    if doc.get("x-pending-notifications")!=expected:
        raise ValueError("private pending notification inventory differs from uncovered families")


def check_private_contracts():
    """Check the explicitly migrated private slice, separately from public RPCs."""
    catalog={
        "Preflight":("daemon-to-supervisor","SupervisorPreflightParams",["supervisor_api","path","directory","sequence"]),
        "PreflightFailed":("supervisor-to-daemon","SupervisorPreflightFailedParams",["error"]),
        "MaintenanceFinished":("supervisor-to-daemon","SupervisorMaintenanceFinishedParams",["result"]),
        "Stopping":("supervisor-to-daemon","SupervisorEmptyParams",[]),
        "Released":("supervisor-to-daemon","SupervisorEmptyParams",[]),
        "Adopted":("bidirectional","SupervisorAdoptedParams",["transfer"]),
        "SupervisorStart":("daemon-to-supervisor","SupervisorStartParams",["supervisor_api","source","directory","dimensions","detached","payload_nofile","legacy_listener","handoff"]),
        "Maintenance":("daemon-to-supervisor","SupervisorMaintenanceParams",["supervisor_api","operation","stdout","stderr"]),
        "Leases":("bidirectional","SupervisorLeasesParams",["transfer","leases","more"]),
        "Command.Start":("daemon-to-supervisor","SupervisorCommandStartParams",["handoff"]),
        "Event.Exported":("supervisor-to-daemon","SupervisorEventExportedParams",["member","result"]),
        "Event.Manifest":("supervisor-to-daemon","SupervisorEventManifestParams",["sequence","path","root","catalogue","owner","transfer"]),
        "Command.Spawn":("daemon-to-supervisor","SupervisorCommandSpawnParams",["id","kind","argv","cwd","env","stdio"]),
        "Command.Signal":("daemon-to-supervisor","SupervisorCommandSignalParams",["id","signal"]),
        "Command.CancelProcess":("daemon-to-supervisor","SupervisorCommandCancelProcessParams",["id","everything"]),
        "Command.KillAll":("daemon-to-supervisor","SupervisorEmptyParams",[]),
        "Event.Spawned":("supervisor-to-daemon","SupervisorEventSpawnedParams",["id","pid"]),
        "Event.SpawnFailed":("supervisor-to-daemon","SupervisorEventSpawnFailedParams",["id","code","message"]),
        "Event.Exited":("supervisor-to-daemon","SupervisorEventExitedParams",["id","code","signal"]),
    }
    for endpoint in PRIVATE_ENDPOINTS:
        doc=document(endpoint)
        Draft7Validator(read("vendor/openrpc-1.3.2.json")).validate(doc)
        rows=doc.get("x-notifications",[])
        if doc["methods"] or len(rows)!=len(catalog) or {row.get("name") for row in rows}!=set(catalog):
            raise ValueError("private notification inventory differs from migrated slice")
        names=[row.get("x-rust-name") for row in rows]
        if len(names)!=len(set(names)):raise ValueError("duplicate private Rust notification marker")
        for row in rows:
            name=row["name"];direction,record,order=catalog[name]
            rust={"Command.Start":"CommandStart","Event.Exported":"EventExported","Event.Manifest":"EventManifest","Command.Spawn":"CommandSpawn","Command.Signal":"CommandSignal","Command.CancelProcess":"CommandCancelProcess","Command.KillAll":"CommandKillAll","Event.Spawned":"EventSpawned","Event.SpawnFailed":"EventSpawnFailed","Event.Exited":"EventExited"}.get(name,name)
            if row.get("x-rust-name")!=rust or row.get("direction")!=direction:
                raise ValueError("private notification identity/direction differs")
            if any(not isinstance(row.get(key),str) or not row[key] for key in ("sender","receiver","connection-state","description")):
                raise ValueError("missing private notification peer/state description")
            pointer={"$ref":"common.json#/definitions/"+record}
            if row.get("x-params-schema")!=pointer or row.get("paramStructure")!="by-name" or row.get("x-positional-schema")!={"not":{}}:
                raise ValueError("private notification parameter strategy differs")
            schema=resolve(pointer)
            if schema.get("type")!="object" or schema.get("additionalProperties") is not False or list(schema.get("properties",{}))!=order or schema.get("required")!=order:
                raise ValueError("private record fields/order/required slots differ")
            params=[{"name":field,"required":True,"schema":{"$ref":f"common.json#/definitions/{record}/properties/{field}"}} for field in order]
            if row.get("params")!=params:raise ValueError("private parameter catalog differs from DTO")
            schema_validator(pointer).check_schema(schema)
    if common().get("x-rust-explicit-records")!=["SupervisorDescriptor","SupervisorRecreate","SupervisorWorktree","SupervisorDirectory","SupervisorLease","SupervisorHandoff"]:
        raise ValueError("private explicit record registry differs")
    descriptor=common()["definitions"]["SupervisorDescriptor"]
    expected_property={"$ref":"#/definitions/SupervisorDescriptorIndex","x-rust-type":"usize","x-rust-field-name":"index"}
    if (descriptor.get("x-rust-name")!="Descriptor" or
        descriptor.get("x-rust-field-order")!=["$fd"] or
        descriptor.get("x-rust-derives")!=["Debug","Clone","Copy","Serialize","Deserialize"] or
        descriptor.get("type")!="object" or descriptor.get("additionalProperties") is not False or
        descriptor.get("required")!=["$fd"] or descriptor.get("properties")!={"$fd":expected_property}):
        raise ValueError("private descriptor schema/Rust contract differs")
    index=common()["definitions"]["SupervisorDescriptorIndex"]
    if {key:value for key,value in index.items() if key!="description"}!={"type":"integer","minimum":0,"maximum":2**64-1,"x-json-integer":True}:
        raise ValueError("private descriptor index representation differs")
    if common().get("x-rust-tagged-enums")!=["SupervisorMaintenanceOperation","SupervisorStdio","SupervisorSource"]:
        raise ValueError("private tagged enum registry differs")
    enum=common()["definitions"]["SupervisorMaintenanceOperation"]
    if enum.get("x-rust-name")!="MaintenanceOperation":raise ValueError("private tagged enum Rust name differs")
    variants=enum.get("oneOf",[]);tags=[]
    rust_schemas={
        "bool":{"type":"boolean"},
        "String":{"type":"string"},
        "::std::path::PathBuf":{"$ref":"#/definitions/SupervisorPath"},
        "Option<::std::path::PathBuf>":{"anyOf":[{"$ref":"#/definitions/SupervisorPath"},{"type":"null"}]},
        "Option<String>":{"type":["string","null"]},
    }
    for variant in variants:
        props=variant.get("properties",{});kind=props.get("kind",{});tag=kind.get("enum",[])
        if variant.get("type")!="object" or variant.get("additionalProperties") is not False or kind.get("type")!="string" or len(tag)!=1 or not isinstance(tag[0],str) or not re.fullmatch(r'[A-Z][A-Za-z0-9]*',tag[0]):
            raise ValueError("invalid closed private tagged enum variant")
        tags.extend(tag);order=variant.get("x-rust-field-order",[])
        if len(order)!=len(set(order)) or set(order)!=set(props)-{"kind"}:
            raise ValueError("private tagged enum field order differs")
        required=["kind"]
        for field in order:
            if not re.fullmatch(r'[a-z][a-z0-9_]*',field):raise ValueError("invalid private tagged enum field")
            schema=props[field];rust=schema.get("x-rust-type")
            shape={key:value for key,value in schema.items() if key not in ("x-rust-type","description")}
            if rust not in rust_schemas or shape!=rust_schemas[rust]:
                raise ValueError("private tagged enum Rust type differs from schema")
            if not rust.startswith("Option<"):required.append(field)
        if variant.get("required")!=required:raise ValueError("private tagged enum required fields differ from Rust optionality")
    if tags!=["Gc","ClearScope"]:raise ValueError("private tagged enum variants differ")


def check():
    check_references()
    check_private_contracts()
    check_supervisor_phases()
    check_supervisor_source_stdio_metadata()
    check_supervisor_handoff_metadata()
    check_supervisor_handoff_roots()
    check_supervisor_process_metadata()
    check_supervisor_pending_inventory()
    check_generated_reader_hooks_and_identities()
    check_communication_readers_and_producers()
    for name, record in common().get("x-rust-serde-records",{}).items():
        props=common()["definitions"][name]["properties"]
        if "name" in record and (type(record["name"]) is not str or not record["name"]):raise ValueError("invalid generated Serde container name")
        if len(record["order"])!=len(set(record["order"])) or set(record["order"])!=set(props):raise ValueError("invalid generated Serde field order")
        for field in record.get("required-nullable",[]):
            if field not in props or not schema_validator({"$ref":"common.json#/definitions/"+name+"/properties/"+field}).is_valid(None):raise ValueError("invalid required nullable Serde field")
        aliases=[]
        for field,names in record.get("aliases",{}).items():
            if field not in props or not names or any(type(n) is not str or n in props for n in names):raise ValueError("invalid generated Serde alias")
            aliases.extend(names)
        if len(aliases)!=len(set(aliases)):raise ValueError("duplicate generated Serde alias")
    payloads=common().get("x-communications-payloads",[])
    if payloads:
        kinds=[row["kind"] for row in payloads];names=[row["x-rust-name"] for row in payloads]
        if len(kinds)!=len(set(kinds)) or len(names)!=len(set(names)):raise ValueError("duplicate communication payload binding")
        expected=[{"if":{"properties":{"kind":{"const":row["kind"]}},"required":["kind"]},"then":{"properties":{"data":row["schema"]}}} for row in payloads]
        if common()["definitions"]["CommunicationsEvent"]["allOf"]!=expected:raise ValueError("communication kind/data constraints differ from registry")
        for row in payloads:
            pointer={"$ref":"common.json"+row["schema"]["$ref"]}
            schema_validator(pointer).check_schema(resolve(pointer))
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
            direct=entry.get("x-direct-serde-params",False)
            expected_direct=endpoint=="host" and entry["name"] in ("history.list","history.prune","state.subscribe","state.unsubscribe","sessions.resize")
            if type(direct) is not bool or direct!=expected_direct:raise ValueError("direct Serde parameter strategy differs")
            if direct and (entry.get("paramStructure")!="either" or entry.get("x-null-params-as-empty") or entry.get("x-nested-positional-params") or entry.get("x-client-param-normalization")):raise ValueError("direct Serde cannot normalize parameters")

            for name in entry.get("x-client-omit-default-false", []):
                if params["properties"].get(name,{}).get("default") is not False: raise ValueError("invalid client false omission")
            schema_validator(entry["x-positional-schema"]).check_schema(resolve(entry["x-positional-schema"]))
            schema_validator(entry["x-params-schema"]).check_schema(params)
            if "result" in entry:
                schema_validator(entry["result"]["schema"]).check_schema(resolve(entry["result"]["schema"]))
            if "x-client-result-schema" in entry:
                schema_validator(entry["x-client-result-schema"]).check_schema(resolve(entry["x-client-result-schema"]))
            if "x-client-result-normalization" in entry:
                expected={"state.subscribe":"state-subscribe-fields","state.unsubscribe":"ignore-successful-body","history.list":"history-list-fields","history.prune":"history-prune-fields","communications.list":"communications-page-fields","raids.members":"raid-members-display",**{name:"ignore-successful-body" for name in ("messages.get","messages.send","messages.reply","inbox.next","inbox.complete","inbox.requeue","raids.create","raids.leave","raids.invite","raids.destroy","raids.set_owner","raids.kick","raids.set_role")},"host.list":"ignore-successful-body","host.cancel":"ignore-successful-body","host.status":"permission-request-fields","permissions.list":"permission-list-records","permissions.get":"permission-record-fields","permissions.request":"permission-request-fields","permissions.decide":"permission-decision-fields","editor.open":"editor-open-fields","devshell.diff":"devshell-preview-display","devshell.lock":"devshell-lock-fields","flake.run":"flake-run-fields","sessions.exec":"exec-field-fallback","terminals.create":"terminal-create-fields","terminals.list":"terminal-list-display","terminals.close":"terminal-close-fields","terminals.read":"terminal-read-fields","terminals.wait":"terminal-wait-fields",**{name:"terminal-close-fields" for name in ("terminals.send","terminals.detach","terminals.resize")}}
                if entry["x-client-result-normalization"] != expected.get(entry["name"]) or entry.get("x-client-wire-result-schema") != ({"$ref":"common.json#/x-client-result-schemas/PermissionListReply"} if entry["name"]=="permissions.list" else {"$ref":"common.json#/x-client-result-schemas/PermissionRecordReply"} if entry["name"]=="permissions.get" else {"type":"object"} if entry["name"]=="devshell.diff" else {"$ref":"common.json#/x-client-result-schemas/HistoryPruneReply"} if entry["name"]=="history.prune" else {}):
                    raise ValueError("unknown client result normalization")
            if "x-client-param-normalization" in entry:
                if entry["name"]!="permissions.request" or entry["x-client-param-normalization"]!="permission-request-fields":raise ValueError("unknown client parameter normalization")
            param_profiles=entry.get("x-client-param-profiles",[])
            if param_profiles:
                expected={"raids.set_owner":"RaidOwnerSetOwner", "raids.destroy":"RaidOwnerDestroy", "communications.list":"CommunicationsCli"}
                rust=expected.get(entry["name"])
                if endpoint!="host" or not rust or len(param_profiles)!=1:raise ValueError("unknown client parameter profile")
                profile=param_profiles[0]
                if (profile["name"],profile["x-rust-name"],profile["x-params-schema"])!=(("communications-cli" if entry["name"]=="communications.list" else "raid-owner-ui"),rust,{"$ref":"common.json#/definitions/"+rust+("Params" if entry["name"]=="communications.list" else "Wire")}):raise ValueError("invalid client parameter profile")
                schema_validator(profile["x-params-schema"]).check_schema(resolve(profile["x-params-schema"]))
            profiles=entry.get("x-client-result-profiles",[])
            if profiles:
                if entry["name"]!="permissions.request" or len(profiles)!=1:raise ValueError("unknown client result profile")
                profile=profiles[0]
                if (profile["name"],profile["x-rust-name"],profile["x-client-result-normalization"],profile["x-result-order"],profile["x-min-positional"])!=("grant-cli","PermissionsRequestGrant","permission-grant-fields",["request","status","message"],3):raise ValueError("invalid client result profile")
                normalized=resolve(profile["x-client-result-schema"])
                wire=resolve(profile["x-client-wire-result-schema"])
                named=copy.deepcopy(normalized);named["required"]=["request","status"]
                tuple_schema={"type":"array","items":[normalized["properties"][key] for key in profile["x-result-order"]],"minItems":profile["x-min-positional"],"maxItems":len(profile["x-result-order"])}
                if wire!={"oneOf":[named,tuple_schema]}:raise ValueError("client result profile wire differs")
                schema_validator(profile["x-client-wire-result-schema"]).check_schema(wire)
                schema_validator(profile["x-client-result-schema"]).check_schema(normalized)
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
    generated_elisp = elisp_readers()
    if args.write:
        ELISP_READERS.write_text(generated_elisp)
    elif not ELISP_READERS.exists() or ELISP_READERS.read_text() != generated_elisp:
        raise SystemExit("stale Emacs API readers; run just api-reference")
    rendered = reference()
    if args.write:
        REFERENCE.write_text(rendered)
    elif not REFERENCE.exists() or REFERENCE.read_text() != rendered:
        raise SystemExit("stale API reference; run just api-reference")


if __name__ == "__main__":
    main()
