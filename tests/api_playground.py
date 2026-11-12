#!/usr/bin/env python3
"""Serve the upstream OpenRPC Playground and live documentation views locally."""
import api_schema
import argparse
import copy
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlsplit

ROOT = Path(__file__).resolve().parents[1]
SCHEMAS = ROOT / "libs/api/schema"
ENDPOINTS = ("host", "sandbox", "attach", "supervisor", "scope", "guardian")


def document(endpoint):
    """Adapt repository extensions for display; canonical contracts stay untouched."""
    if endpoint not in ENDPOINTS:
        raise ValueError("unknown protocol")
    common = api_schema.expand_common(json.loads((SCHEMAS / "common.json").read_text()))
    doc = api_schema.expand_document(json.loads((SCHEMAS / f"{endpoint}.openrpc.json").read_text()), common)
    schemas = {}

    def bundle(value):
        if isinstance(value, list):
            return [bundle(v) for v in value]
        if not isinstance(value, dict):
            return value
        result = {}
        for key, item in value.items():
            if key.startswith("x-rust-"):
                continue
            if key == "$ref" and item.startswith(("common.json#/", "#/")):
                section, name, *tail = item.removeprefix("common.json").removeprefix("#/").split("/")
                exported = name if section == "definitions" else section.removeprefix("x-") + "_" + name
                if exported not in schemas:
                    schemas[exported] = {}  # Recursive references are allowed.
                    schemas[exported] = bundle(common[section][name])
                result[key] = "#/components/schemas/" + exported + "".join("/" + part for part in tail)
            else:
                result[key] = bundle(item)
        return result

    for notification in doc.get("x-notifications", []):
        row = copy.deepcopy(notification)
        row["summary"] = "Notification · " + row["direction"]
        row["description"] = (
            f"Notification: no JSON-RPC response. {row['sender']} → {row['receiver']}.\n\n"
            f"Connection state: {row['connection-state']}.\n\n" + row["description"]
        )
        for key in ("direction", "sender", "receiver", "connection-state"):
            row.pop(key)
        doc["methods"].append(row)
    for method in doc["methods"]:
        if "x-wire-params-schema" in method:
            pointer = method["x-wire-params-schema"]["$ref"].removeprefix("common.json#/")
            wire = common
            for part in pointer.split("/"):
                wire = wire[part]
            for param in method["params"]:
                param["schema"] = copy.deepcopy(wire["properties"][param["name"]])
    for reply in doc.get("x-completions", []):
        # Fixed-ID replies are not methods. Display them in the document overview.
        schema = common["definitions"][reply["result"]["schema"]["$ref"].split("/")[-1]]
        doc["info"]["description"] = doc["info"].get("description", "") + (
            f"\n\n### Completion reply: {reply['name']}\n\n{reply['description']}\n\n"
            f"Direction: {reply['direction']}. Fixed response ID: `{reply['id']}`.\n\n"
            f"Result:\n```json\n{json.dumps(schema, indent=2)}\n```\n\n"
            + "Errors: " + "; ".join(f"`{e['code']}` {e['message']}" for e in reply["errors"]) + "."
        )
    doc = bundle(doc)
    doc.setdefault("components", {})["schemas"] = schemas
    return doc


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, assets, endpoint="host", **kwargs):
        self.endpoint = endpoint
        super().__init__(*args, directory=str(assets), **kwargs)

    def do_GET(self):
        request = urlsplit(self.path)
        if request.path == "/":
            endpoint = parse_qs(request.query).get("endpoint", [self.endpoint])[0]
            if endpoint not in ENDPOINTS:
                self.send_error(404, "Unknown protocol")
                return
            options = "".join(f'<option value="{e}"{" selected" if e == endpoint else ""}>{e}</option>' for e in ENDPOINTS)
            query = urlencode({
                "schemaUrl": f"/schemas/{endpoint}.json",
                "uiSchema[appBar][ui:splitView]": "false",
                "uiSchema[appBar][ui:examplesDropdown]": "false",
                "uiSchema[appBar][ui:transports]": "false",
                "uiSchema[methods][ui:methodPlugins]": "false",
            })
            self.respond("text/html; charset=utf-8", f'''<!doctype html>
<html lang="en"><meta charset="utf-8"><title>Goblins API</title>
<style>body{{margin:0;font:16px system-ui;display:flex;flex-direction:column;height:100vh}}
header{{padding:10px 16px;display:flex;align-items:center;gap:16px}}select{{font:inherit}}
iframe{{flex:1;border:0;width:100%}}small{{color:#555}}</style>
<header><strong>Goblins API</strong><select aria-label="Protocol" onchange="location.search='endpoint='+this.value">{options}</select>
<a href="/schemas/{endpoint}.json">Viewer JSON</a><small>Documentation · Unix-socket RPC</small></header>
<iframe title="OpenRPC Playground" src="/playground/?{query}"></iframe></html>'''.encode())
        elif request.path.startswith("/schemas/"):
            endpoint = request.path.removeprefix("/schemas/").removesuffix(".json")
            if endpoint not in ENDPOINTS or not request.path.endswith(".json"):
                self.send_error(404, "Unknown protocol")
                return
            self.respond("application/json", json.dumps(document(endpoint)).encode())
        else:
            if request.path == "/playground/":
                self.path = "/index.html"
            super().do_GET()

    def respond(self, content_type, body):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--endpoint", choices=ENDPOINTS, default="host")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    if not (args.assets / "index.html").is_file():
        parser.error("Playground assets must contain index.html")
    handler = partial(Handler, assets=args.assets, endpoint=args.endpoint)
    with ThreadingHTTPServer(("127.0.0.1", args.port), handler) as server:
        print(f"Goblins API: http://127.0.0.1:{server.server_port}/", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
