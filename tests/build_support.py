"""Rooted Nix fixtures shared within a run, with an incremental host controller.

Only the host executable may come from Cargo. Sandbox executables and their
closures remain real, immutable Nix outputs, including in local-controller mode.
"""
from functools import cache
import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import time

from support import FLAKE, HERE, command

fixture_timings = []


def cache_directory():
    return Path(os.environ.get("GOBLINS_TEST_CACHE", HERE / "target/test-fixtures")).resolve()


@cache
def fixture_path(installable):
    """Build once per process, retaining an indirect GC root between runs."""
    root = cache_directory() / "roots" / hashlib.sha256(installable.encode()).hexdigest()[:20]
    root.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    output = command(["nix", "build", "--no-write-lock-file", "--print-out-paths",
                      "--max-jobs", "1", "--cores", "2", "--out-link", root, installable])
    fixture_timings.append((time.monotonic() - started, installable))
    return Path(output)


@cache
def fixture_app(check="demo-goblins", *, packaged=False):
    """Use a fixture's manifest with Cargo's controller, or its Nix package.

    `packaged=True` always exercises the actual installed package. Without the
    runner's environment this also preserves direct unittest invocations.
    """
    installable = check if "#" in check else FLAKE + "#checks.x86_64-linux." + check
    controller = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
    if not controller or packaged:
        return fixture_path(installable) / "bin/goblins"
    manifest = fixture_path(installable + ".config")
    root = cache_directory() / "apps" / hashlib.sha256(installable.encode()).hexdigest()[:20]
    application = root / "bin/goblins"
    application.parent.mkdir(parents=True, exist_ok=True)
    application.write_text(f'#!/bin/sh\nexec {shlex.quote(controller)} --runtime {shlex.quote(str(manifest))} "$@"\n')
    application.chmod(0o755)
    # Use the same completion generator as the Nix package. The installation
    # test requests packaged=True separately to verify Nix's install phase too.
    for shell, relative in {
        "bash": "share/bash-completion/completions/goblins",
        "fish": "share/fish/vendor_completions.d/goblins.fish",
        "zsh": "share/zsh/site-functions/_goblins",
    }.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(subprocess.check_output([application, "completions", shell], text=True))
    return application
