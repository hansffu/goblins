"""Build the real actor fixture, then run Rust tests without implementation links.

With --host, run the host stage instead: the ignored tests of `host` modules,
which need user namespaces and the host's Nix store and so cannot run in the
Nix build sandbox. Their Nix fixtures are built first and passed by path. The
other ignored tests are subprocess helpers and never run on their own."""
import os
from pathlib import Path
import subprocess
import sys

from support import FLAKE, HERE, cargo_artifacts

# What host tests read, and the fixture each names.
HOST_FIXTURES = {
    "GOBLINS_TEST_RUNTIME": "runtime",
    "GOBLINS_TEST_JQ": "jq^bin",
    "GOBLINS_TEST_SYMLINK_TOOL": "symlink-tool",
}


def supervisor_executable(artifacts):
    candidates = [Path(artifact["executable"]) for artifact in artifacts
                  if artifact.get("reason") == "compiler-artifact"
                  and artifact.get("executable")
                  and artifact.get("target", {}).get("name") == "goblins-supervisor-bwrap"
                  and artifact.get("target", {}).get("kind") == ["bin"]
                  and artifact.get("profile", {}).get("test") is False]
    if len(candidates) != 1:
        raise ValueError("Cargo must report exactly one production supervisor executable")
    executable = candidates[0]
    if not executable.is_absolute() or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("Cargo supervisor artifact must be an absolute executable file")
    return executable


def build_supervisor():
    return supervisor_executable(cargo_artifacts(
        "build", "--locked", "-j", "2", "-p", "goblins-supervisor-bwrap",
        "--bin", "goblins-supervisor-bwrap"))


def host(args):
    from build_support import fixture_path
    environment = {**os.environ, **{variable: str(fixture_path(f"{FLAKE}#{installable}"))
                                    for variable, installable in HOST_FIXTURES.items()},
                   "GOBLINS_TEST_SUPERVISOR_EXECUTABLE": str(build_supervisor())}
    return subprocess.call([os.environ.get("CARGO", "cargo"), "test", "--locked",
                            *(args or ["--workspace", "-j", "2"]), "--", "--ignored", "host::",
                            "--test-threads=4"], cwd=HERE, env=environment)


def main(args=None):
    args = sys.argv[1:] if args is None else args
    if args[:1] == ["--host"]:
        return host(args[1:])
    environment = {**os.environ, "GOBLINS_TEST_SUPERVISOR_EXECUTABLE": str(build_supervisor())}
    return subprocess.call([os.environ.get("CARGO", "cargo"), "test", "--locked",
                            *(args or ["--workspace", "-j", "2", "--", "--test-threads=4"])],
                           cwd=HERE, env=environment)


if __name__ == "__main__":
    raise SystemExit(main())
