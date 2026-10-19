#!/usr/bin/env python3
"""Feature tasks, conservative path recommendations and the full merge gate."""
import argparse
from dataclasses import dataclass
import fnmatch
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = Path(__file__).with_name("features.toml")
CHECKS = ("rust", "emacs", "nix", "packaged")


def load_manifest(path=MANIFEST):
    with path.open("rb") as stream:
        data = tomllib.load(stream)
    for name, feature in data["features"].items():
        if name == "full" or not feature.get("description"):
            raise ValueError(f"invalid feature: {name}")
        for field in ("python", "paths", "checks"):
            if not isinstance(feature[field], list) or not all(isinstance(v, str) for v in feature[field]):
                raise ValueError(f"{name}.{field} must be a list of strings")
        if set(feature["checks"]) - set(CHECKS):
            raise ValueError(f"unknown checks in {name}")
    return data


def cases(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from cases(test)
        else:
            yield test


def load_suite(selectors=None):
    loader = unittest.TestLoader()
    suite = (loader.discover(str(ROOT / "tests")) if selectors is None else
             loader.loadTestsFromNames(selectors))
    if loader.errors:
        raise ValueError("Invalid test selection:\n" + "\n".join(loader.errors))
    return suite


def check_manifest(data):
    discovered = {test.id() for test in cases(load_suite())}
    selectors = [name for feature in data["features"].values() for name in feature["python"]]
    selected = {test.id() for test in cases(load_suite(selectors))}
    missing, extra = discovered - selected, selected - discovered
    if missing or extra:
        raise ValueError("Unmapped tests: " + ", ".join(sorted(missing)) +
                         "\nUndiscovered selections: " + ", ".join(sorted(extra)))
    return len(discovered)


def recommend(paths, data):
    """Return all matching features; unknown/shared paths recommend full."""
    features, reasons = [], []
    for raw in paths:
        path = Path(raw)
        path = (path.relative_to(ROOT).as_posix() if path.is_absolute() else path.as_posix())
        if path.startswith("../"):
            raise ValueError(f"path is outside the repository: {raw}")
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in data["routing"]["broad"]):
            features.append("full")
            reasons.append(f"{path}: shared code or test infrastructure")
            continue
        matches = []
        module = Path(path).stem if path.startswith("tests/test_") and path.endswith(".py") else None
        for name, feature in data["features"].items():
            if any(fnmatch.fnmatchcase(path, pattern) for pattern in feature["paths"]) or module and any(
                    selector == module or selector.startswith(module + ".") for selector in feature["python"]):
                matches.append(name)
        if not matches and any(fnmatch.fnmatchcase(path, pattern) for pattern in data["routing"]["docs"]):
            matches = ["docs"]
        if not matches:
            matches = ["full"]
            reasons.append(f"{path}: no source mapping; use full validation")
        features.extend(matches)
    names = tuple(dict.fromkeys(features)) or ("docs",)
    return (("full",) if "full" in names else names), reasons


def changed_paths(base, root=ROOT):
    ancestor = subprocess.check_output(["git", "merge-base", base, "HEAD"], cwd=root, text=True).strip()
    tracked = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", ancestor, "--"], cwd=root)
    untracked = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=root)
    return list(dict.fromkeys(os.fsdecode(path) for path in (tracked + untracked).split(b"\0") if path))


@dataclass
class Plan:
    features: tuple
    selectors: tuple | None
    checks: tuple
    env: dict


def make_plan(names, data):
    names = tuple(dict.fromkeys(names))
    unknown = set(names) - set(data["features"]) - {"full"}
    if unknown:
        raise ValueError("Unknown features: " + ", ".join(sorted(unknown)))
    if "full" in names:
        return Plan(("full",), None, tuple(data["merge"]["checks"]), dict(data["merge"]["env"]))
    selectors, checks, env = [], set(), {}
    for name in names:
        feature = data["features"][name]
        selectors.extend(feature["python"])
        checks.update(feature["checks"])
        for key, value in feature.get("env", {}).items():
            if key in env and env[key] != value:
                raise ValueError(f"conflicting environment for {key}")
            env[key] = value
    # Prefer a module/class over a redundant child selector. run.py also
    # deduplicates actual IDs, including overlapping explicit selectors.
    selectors = tuple(s for s in dict.fromkeys(selectors)
                      if not any(s.startswith(parent + ".") for parent in selectors if parent != s))
    return Plan(names, selectors, tuple(check for check in CHECKS if check in checks), env)


def commands(plan, jobs, data):
    steps = []
    if "rust" in plan.checks:
        steps.append(("Rust", ["just", "test-rust"]))
    if plan.selectors is None or plan.selectors:
        steps.append(("Python", [sys.executable, "tests/run.py", "-j", str(jobs), *(plan.selectors or ())]))
    for check in ("emacs", "nix"):
        if check in plan.checks:
            steps.append((check, ["just", "test-" + check]))
    if "packaged" in plan.checks:
        steps.append(("Packaged smoke", [sys.executable, "tests/run.py", "--packaged", "-j", str(jobs),
                                         *data["merge"]["packaged"]]))
    return steps


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("features", nargs="*")
    parser.add_argument("--list", action="store_true", help="list features without building or discovering tests")
    parser.add_argument("--check", action="store_true", help="check every test belongs to at least one feature")
    parser.add_argument("--plan", action="store_true", help="show checks without running or building anything")
    parser.add_argument("--paths", nargs="+", default=[], help="recommend tasks for repository-relative paths")
    parser.add_argument("--changed", metavar="BASE", help="include branch/worktree/untracked changes since merge-base with BASE")
    parser.add_argument("-j", "--jobs", type=int, choices=(1, 2, 4), default=2)
    args = parser.parse_args(argv)
    try:
        data = load_manifest()
        if args.list:
            print("full             Full merge gate: Rust, all Python, ERT, Nix and packaged smoke")
            for name, feature in data["features"].items():
                print(f"{name:16} {feature['description']}")
            return 0
        if args.check:
            print(f"Feature map covers all {check_manifest(data)} discovered Python tests")
            return 0
        paths = args.paths + (changed_paths(args.changed) if args.changed else [])
        inferred, reasons = recommend(paths, data) if args.paths or args.changed else ((), [])
        names = (*args.features, *inferred)
        if not names:
            parser.error("choose features, --paths or --changed BASE; use --list to see tasks")
        plan = make_plan(names, data)
        if plan.selectors is None:
            check_manifest(data)
        suite = load_suite(plan.selectors)
        count = len({test.id() for test in cases(suite)})
        print("Features: " + ", ".join(plan.features), flush=True)
        print(f"Python: {count} unique tests; additional checks: {', '.join(plan.checks) or 'none'}", flush=True)
        for reason in reasons:
            print(reason, flush=True)
        if plan.env:
            print("Environment: " + shlex.join(f"{k}={v}" for k, v in sorted(plan.env.items())), flush=True)
        steps = commands(plan, args.jobs, data)
        env = {**os.environ, **plan.env}
        for label, command in steps:
            print(f"{label}: {shlex.join(command)}", flush=True)
            if not args.plan:
                started = time.monotonic()
                result = subprocess.run(command, cwd=ROOT, env=env)
                print(f"{label}: {'PASS' if result.returncode == 0 else 'FAIL'} ({time.monotonic()-started:.2f}s)", flush=True)
                if result.returncode:
                    return 1
        if not steps:
            print("No runtime checks selected; review documentation. The full gate is still required before merge.")
        return 0
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    sys.exit(main())
