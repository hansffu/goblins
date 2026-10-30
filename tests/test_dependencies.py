"""Workspace dependency boundaries, checked against Cargo's member paths."""
import json
from pathlib import Path
import subprocess
import unittest

from support import HERE


# Paths identify roles independently of Cargo package names. The daemon may
# link supervisor while Local exists; phase 6 removes that transitional edge.
LIBRARIES = {
    "api": {"jsonrpc", "unix"},
    "jsonrpc": {"unix"},
    "init": {"api", "jsonrpc", "unix"},
    "runtime": {"api", "jsonrpc", "unix"},
    "supervisor": {"runtime", "api", "jsonrpc", "init", "unix"},
    "unix": set(),
}
SANDBOX = {"sandbox-cli", "adapter-codex", "adapter-claude"}


def check_dependencies(paths, edges):
    """Check workspace edges and sandbox transitive reachability."""
    for source, dependencies in edges.items():
        kind, name = Path(paths[source]).parts
        for target in dependencies:
            target_kind, target_name = Path(paths[target]).parts
            if target_kind != "libs":
                raise ValueError(f"{paths[source]} depends on package {paths[target]}")
            if kind == "libs" and target_name not in LIBRARIES[name]:
                raise ValueError(f"{paths[source]} may not depend on {paths[target]}")
        if kind == "packages" and name in SANDBOX:
            pending, seen = list(dependencies), set()
            while pending:
                dependency = pending.pop()
                if dependency in seen:
                    continue
                seen.add(dependency)
                if paths[dependency] in {"libs/runtime", "libs/supervisor"}:
                    raise ValueError(f"{paths[source]} links host authority {paths[dependency]}")
                pending.extend(edges.get(dependency, ()))


class DependencyTests(unittest.TestCase):
    def test_workspace_obeys_dependency_boundaries(self):
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--locked", "--offline", "--format-version=1"],
            cwd=HERE, text=True))
        members = set(metadata["workspace_members"])
        paths = {package["id"]: Path(package["manifest_path"]).parent.relative_to(HERE).as_posix()
                 for package in metadata["packages"] if package["id"] in members}
        by_directory = {Path(package["manifest_path"]).parent.resolve(): package["id"]
                        for package in metadata["packages"] if package["id"] in members}
        # Member declarations include optional, target-specific, build and dev
        # dependencies, without downloading unrelated registry packages.
        edges = {package["id"]: [by_directory[directory]
                 for dependency in package["dependencies"]
                 if dependency.get("path")
                 and (directory := Path(dependency["path"]).resolve()) in by_directory]
                 for package in metadata["packages"] if package["id"] in members}
        check_dependencies(paths, edges)

    def test_packages_and_libraries_cannot_depend_on_packages(self):
        paths = {"daemon": "packages/daemon", "cli": "packages/sandbox-cli", "api": "libs/api"}
        for source in ("daemon", "api"):
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, "depends on package"):
                check_dependencies(paths, {source: ["cli"]})

    def test_sandbox_cannot_link_host_authority_even_indirectly(self):
        paths = {"cli": "packages/sandbox-cli", "api": "libs/api", "runtime": "libs/runtime"}
        for edges in ({"cli": ["runtime"]}, {"cli": ["api"], "api": ["runtime"]}):
            with self.subTest(edges=edges), self.assertRaisesRegex(ValueError, "links host authority"):
                check_dependencies(paths, edges)

    def test_public_libraries_and_unix_keep_their_layers(self):
        paths = {"api": "libs/api", "init": "libs/init", "unix": "libs/unix"}
        for source, target in (("api", "init"), ("unix", "api")):
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, "may not depend"):
                check_dependencies(paths, {source: [target]})
