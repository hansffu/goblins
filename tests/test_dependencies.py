"""Workspace dependency boundaries, checked against Cargo's member paths."""
import json
from pathlib import Path
import subprocess
import unittest

from support import HERE


# Paths identify roles independently of Cargo package names. The daemon links
# only contracts/helpers; runtime implementation is a separate executable.
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
        if paths[source] == "packages/daemon":
            pending, seen = list(dependencies), set()
            while pending:
                dependency = pending.pop()
                if dependency in seen:
                    continue
                seen.add(dependency)
                if paths[dependency] in {"libs/supervisor", "libs/init"}:
                    raise ValueError(f"daemon links runtime implementation {paths[dependency]}")
                pending.extend(edges.get(dependency, ()))
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


def workspace_graph(metadata, root):
    """All declared workspace edges, including dev/build/optional/target edges."""
    members = set(metadata["workspace_members"])
    paths = {package["id"]: Path(package["manifest_path"]).parent.relative_to(root).as_posix()
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
    return paths, edges


class DependencyTests(unittest.TestCase):
    def test_workspace_obeys_dependency_boundaries(self):
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--locked", "--offline", "--format-version=1"],
            cwd=HERE, text=True))
        paths, edges = workspace_graph(metadata, HERE)
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

    def test_daemon_cannot_link_implementation_directly_or_transitively(self):
        paths = {"daemon": "packages/daemon", "runtime": "libs/runtime",
                 "supervisor": "libs/supervisor", "init": "libs/init"}
        for target in ("supervisor", "init"):
            for edges in ({"daemon": [target]},
                          {"daemon": ["runtime"], "runtime": [target]}):
                with self.subTest(target=target, edges=edges), self.assertRaisesRegex(
                        ValueError, "daemon links runtime implementation"):
                    check_dependencies(paths, edges)

    def test_final_daemon_contract_graph_is_allowed(self):
        paths = {"daemon": "packages/daemon", "runtime": "libs/runtime",
                 "api": "libs/api", "jsonrpc": "libs/jsonrpc", "unix": "libs/unix"}
        check_dependencies(paths, {"daemon": ["runtime", "api", "jsonrpc", "unix"],
                                   "runtime": ["api", "jsonrpc", "unix"],
                                   "api": ["jsonrpc"], "jsonrpc": [], "unix": []})

    def test_daemon_boundary_checks_every_declared_dependency_kind(self):
        root = Path("/goblins-dependency-test")
        for forbidden in ("supervisor", "init"):
            for kind in (None, "dev", "build"):
                for optional in (False, True):
                    for target in (None, 'cfg(target_os = "linux")'):
                        for indirect in (False, True):
                            def declaration(name):
                                return {"path": str(root / "libs" / name), "kind": kind,
                                        "optional": optional, "target": target}
                            metadata = {"workspace_members": ["daemon", "runtime", forbidden], "packages": [
                                {"id": "daemon", "manifest_path": str(root / "packages/daemon/Cargo.toml"),
                                 "dependencies": [declaration("runtime" if indirect else forbidden)]},
                                {"id": "runtime", "manifest_path": str(root / "libs/runtime/Cargo.toml"),
                                 "dependencies": [declaration(forbidden)] if indirect else []},
                                {"id": forbidden, "manifest_path": str(root / "libs" / forbidden / "Cargo.toml"),
                                 "dependencies": []}]}
                            with self.subTest(forbidden=forbidden, kind=kind, optional=optional,
                                              target=target, indirect=indirect), self.assertRaisesRegex(
                                    ValueError, "daemon links runtime implementation"):
                                check_dependencies(*workspace_graph(metadata, root))
