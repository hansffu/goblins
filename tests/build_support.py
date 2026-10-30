"""Rooted Nix fixtures shared within a run, with an incremental host controller.

Only host executables may come from Cargo. Sandbox executables and their
closures remain real, immutable Nix outputs, including in local-controller mode.
"""
from functools import cache
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

from support import FLAKE, HERE, command

fixture_timings = []


@contextmanager
def fixture_lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def atomic_write(path, text, executable=False):
    temporary = path.with_name(f".{path.name}.{os.getpid()}")
    try:
        temporary.write_text(text)
        if executable:
            temporary.chmod(0o755)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def cache_directory():
    return Path(os.environ.get("GOBLINS_TEST_CACHE", HERE / "target/test-fixtures")).resolve()


def nix_bin():
    # pkgs.nix also installs its manual output by default. This caller needs
    # exactly the executable output, not newline-separated paths from nix build.
    return fixture_path(FLAKE + "#nix^out") / "bin"


def daemon_environment(env=None, *, tool_path=None):
    """Match packaged Nix selection; only explicit test barriers precede it."""
    env = dict(os.environ if env is None else env)
    env["PATH"] = os.pathsep.join(str(path) for path in
                                 (tool_path, nix_bin(), env.get("PATH")) if path)
    return env


@cache
def fixture_path(installable):
    """Build once per process, retaining an indirect GC root between runs."""
    root = cache_directory() / "roots" / hashlib.sha256(installable.encode()).hexdigest()[:20]
    root.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    # Worker processes share roots. Keep fixture builds serial even when test
    # execution overlaps, to bound Nix work and avoid competing out-link writes.
    with fixture_lock(cache_directory() / "fixture-build.lock"):
        output = command(["nix", "build", "--no-write-lock-file", "--print-out-paths",
                          "--max-jobs", "1", "--cores", "2", "--out-link", root, installable])
    if len(output.splitlines()) != 1:
        raise RuntimeError(f"expected one fixture output for {installable}; select an explicit output with ^out")
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
    with fixture_lock(root / "prepare.lock"):
        scope_runtime = os.environ.get("GOBLINS_LOCAL_SCOPE_RUNTIME")
        if scope_runtime:
            data = json.loads(manifest.read_text())
            def pin(configuration):
                configuration["scope_runtime"] = scope_runtime
                for variant in configuration.get("scopes", {}).values():
                    pin(variant)
            for configuration in data["goblins"].values():
                pin(configuration)
            manifest = root / "config.json"
            atomic_write(manifest, json.dumps(data))
        # Another worker may execute this launcher while we prepare it. Replace
        # files atomically rather than truncating executable/readable files.
        atomic_write(application,
                     f'#!/bin/sh\nexport PATH={shlex.quote(str(nix_bin()))}:"$PATH"\n'
                     f'exec {shlex.quote(controller)} --runtime {shlex.quote(str(manifest))} "$@"\n',
                     executable=True)
        # Keep the actual generator and exact installed completion contents.
        for shell, relative in {
            "bash": "share/bash-completion/completions/goblins",
            "fish": "share/fish/vendor_completions.d/goblins.fish",
            "zsh": "share/zsh/site-functions/_goblins",
        }.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            atomic_write(path, subprocess.check_output([application, "completions", shell], text=True))
    return application
