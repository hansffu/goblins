"""Fast checks for the local OpenRPC documentation adapter and server."""
from functools import partial
from http.server import ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import urlopen

from jsonschema import Draft7Validator
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT7

import api_contract as contract
import api_playground as playground


class PlaygroundTests(unittest.TestCase):
    def test_every_protocol_bundles_references_and_labels_notifications(self):
        resource = Resource.from_contents(contract.read("vendor/json-schema-tools.json"), default_specification=DRAFT7)
        registry = Registry().with_resources((uri, resource) for uri in ("https://meta.json-schema.tools/", "https://meta.json-schema.tools"))
        validator = Draft7Validator(contract.read("vendor/openrpc-1.3.2.json"), registry=registry)
        for endpoint in playground.ENDPOINTS:
            with self.subTest(endpoint=endpoint):
                source = contract.document(endpoint)
                doc = playground.document(endpoint)
                validator.validate(doc)
                rows = {row["name"]: row for row in doc["methods"]}
                self.assertEqual(set(rows), {row["name"] for row in source["methods"] + source.get("x-notifications", [])})
                for notification in source.get("x-notifications", []):
                    row = rows[notification["name"]]
                    self.assertNotIn("result", row)
                    self.assertIn(notification["direction"], row["summary"])
                    self.assertIn(notification["connection-state"], row["description"])
                for completion in source.get("x-completions", []):
                    self.assertNotIn(completion["name"], rows)
                    self.assertIn(completion["name"], doc["info"]["description"])
                    self.assertIn(str(completion["id"]), doc["info"]["description"])

                def check(value):
                    if isinstance(value, list):
                        for child in value:
                            check(child)
                    elif isinstance(value, dict):
                        if "$ref" in value:
                            reference = value["$ref"]
                            self.assertTrue(reference.startswith("#/"), reference)
                            target = doc
                            for part in reference[2:].split("/"):
                                target = target[part.replace("~1", "/").replace("~0", "~")]
                        for child in value.values():
                            check(child)
                check(doc)

    def test_editor_documentation_uses_wire_parameters(self):
        doc = playground.document("sandbox")
        row = next(row for row in doc["methods"] if row["name"] == "editor.open")
        files = next(param["schema"] for param in row["params"] if param["name"] == "files")
        wire = {**files, "components": doc["components"]}
        validator = Draft7Validator(wire)
        validator.validate([["/a"]])
        validator.validate([{"path": "/a"}])
        normalized = contract.resolve(contract.method("sandbox", "editor.open")["x-params-schema"])
        self.assertFalse(contract.schema_validator(normalized).is_valid({"files": [["/a"]], "wait": True}))

    def test_local_http_serves_ui_assets_and_live_protocol_documents(self):
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            (assets / "index.html").write_text("<title>Upstream Playground</title>")
            server = ThreadingHTTPServer(("127.0.0.1", 0), partial(playground.Handler, assets=assets))
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                base = f"http://127.0.0.1:{server.server_port}"
                with urlopen(base + "/?endpoint=scope") as response:
                    page = response.read().decode()
                    self.assertIn("OpenRPC Playground", page)
                    self.assertIn("scope.json", page)
                with urlopen(base + "/playground/") as response:
                    self.assertIn(b"Upstream Playground", response.read())
                with urlopen(base + "/schemas/guardian.json") as response:
                    doc = json.load(response)
                    self.assertEqual({row["name"] for row in doc["methods"]}, {"Retain", "Retained"})
                    self.assertEqual(response.headers["Cache-Control"], "no-store")
                for path in ("/?endpoint=missing", "/schemas/missing.json"):
                    with self.assertRaises(HTTPError) as error:
                        urlopen(base + path)
                    self.assertEqual(error.exception.code, 404)
            finally:
                server.shutdown()
                server.server_close()
                worker.join()
