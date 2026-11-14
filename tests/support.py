"""Shared helpers for the Python suites: workflow steps, Cargo artifacts, the
ignored Rust host driver test_scope_cleanup uses, and pinned test flakes."""
import contextlib
import json
import os
from pathlib import Path
import subprocess
import unittest

HERE = Path(__file__).resolve().parents[1]
# The repository as a flake: tracked files only (untracked ones need git add),
# so builds never copy target/ or other ignored files into the store again.
FLAKE = "git+file://" + str(HERE)


@contextlib.contextmanager
def step(name):
    """Name the workflow step in a failure, so one long test still says where it broke."""
    try:
        yield
    except AssertionError as error:
        raise AssertionError(f"step '{name}': {error}") from error


def cases(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from cases(test)
        else:
            yield test


def cargo_artifacts(*args):
    # Keep machine-readable artifact paths on stdout and compiler diagnostics
    # on inherited stderr, including when check_output raises on a failed build.
    output = subprocess.check_output([
        os.environ.get("CARGO", "cargo"), *args, "--message-format=json-render-diagnostics",
    ], cwd=HERE, text=True)
    return [json.loads(line) for line in output.splitlines()]


def rust_driver():
    if "GOBLINS_TEST_DRIVER" not in os.environ:
        # Match test-rust's workspace feature selection so Cargo reuses artifacts.
        for artifact in cargo_artifacts("test", "--locked", "--workspace", "--no-run"):
            if (artifact.get("reason") == "compiler-artifact" and artifact.get("executable")
                    and artifact["target"]["name"] == "goblins_supervisor"):
                os.environ["GOBLINS_TEST_DRIVER"] = artifact["executable"]
        if "GOBLINS_TEST_DRIVER" not in os.environ:
            raise RuntimeError("Cargo did not produce the Rust host test driver")
    return os.environ["GOBLINS_TEST_DRIVER"]


def command(args):
    result = subprocess.run([str(x) for x in args], text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {args}\n{result.stderr[-12000:]}")
    return result.stdout.strip()



def pinned_nixpkgs():
    """This repository's locked nixpkgs: the flake URL naming it and its lock node."""
    lock = json.loads((HERE / "flake.lock").read_text())
    node = lock["nodes"][lock["nodes"][lock["root"]]["inputs"]["nixpkgs"]]
    original = node["original"]
    return f"github:{original['owner']}/{original['repo']}/{original['rev']}", node


def lock_flake(directory):
    """Lock a test flake whose input `nixpkgs` is pinned_nixpkgs()'s URL.

    Its lock node is copied from this repository's flake.lock, so Nix finds
    the pinned source in the store by its hash, offline and without hashing
    the tree on every command as it does for a path: input. Nix locks any
    other inputs."""
    _, node = pinned_nixpkgs()
    (Path(directory) / "flake.lock").write_text(json.dumps({
        "nodes": {"nixpkgs": {"locked": node["locked"], "original": node["original"]},
                  "root": {"inputs": {"nixpkgs": "nixpkgs"}}},
        "root": "root", "version": 7}, indent=2) + "\n")
    command(["nix", "flake", "lock", "--offline", f"path:{directory}"])
