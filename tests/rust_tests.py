"""Build the real actor fixture, then run Rust tests without implementation links."""
import os
from pathlib import Path
import subprocess
import sys

from support import HERE, cargo_artifacts


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


def main(args=None):
    args = sys.argv[1:] if args is None else args
    executable = supervisor_executable(cargo_artifacts(
        "build", "--locked", "-j", "2", "-p", "goblins-supervisor-bwrap",
        "--bin", "goblins-supervisor-bwrap"))
    environment = {**os.environ, "GOBLINS_TEST_SUPERVISOR_EXECUTABLE": str(executable)}
    return subprocess.call([os.environ.get("CARGO", "cargo"), "test", "--locked",
                            *(args or ["--workspace", "-j", "2", "--", "--test-threads=4"])],
                           cwd=HERE, env=environment)


if __name__ == "__main__":
    raise SystemExit(main())
