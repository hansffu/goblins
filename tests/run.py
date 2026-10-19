#!/usr/bin/env python3
"""Run Python tests with an incremental Cargo controller and timing evidence."""
import argparse
from concurrent.futures import FIRST_COMPLETED, ProcessPoolExecutor, wait
import fcntl
import json
import multiprocessing
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import unittest

from build_support import cache_directory, fixture_timings
from support import HERE, rust_driver

# Keep engines/native frontends exclusive. Ordinary modules stay intact;
# dev-shell classes use private projects/daemons and may overlap, while their
# Docker class remains exclusive. Class fixtures always stay in one worker.
EXCLUSIVE_MODULES = {
    "test_docker", "test_scopes", "test_run_targets",
    "test_emacs", "test_codex_config", "test_codex_native",
}
EXCLUSIVE_CLASSES = {"test_devshell.DevShellDockerTests"}


def exclusive(group):
    return group.split(".", 1)[0] in EXCLUSIVE_MODULES or group in EXCLUSIVE_CLASSES


class TimedResult(unittest.TextTestResult):
    def startTest(self, test):
        self.started = time.monotonic()
        super().startTest(test)

    def stopTest(self, test):
        self.timings.append((time.monotonic() - self.started, test.id()))
        super().stopTest(test)

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.timings = []


def cases(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from cases(test)
        else:
            yield test


def unique_suite(suite):
    """Run overlapping selections once, retaining contiguous class fixtures."""
    seen, classes = set(), {}
    for test in cases(suite):
        if test.id() not in seen:
            seen.add(test.id())
            classes.setdefault(type(test), []).append(test)
    return unittest.TestSuite(test for group in classes.values() for test in group)


def run_group(group, names, log_path, failfast):
    """Executed in a spawned process; native child output goes to its log too."""
    fixture_timings.clear()
    with open(log_path, "w", buffering=1) as log:
        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(log.fileno(), 1)
        os.dup2(log.fileno(), 2)
        suite = unittest.defaultTestLoader.loadTestsFromNames(names)
        started = time.monotonic()
        result = unittest.TextTestRunner(stream=log, verbosity=2, resultclass=TimedResult,
                                        failfast=failfast).run(suite)
        sys.stdout.flush()
        sys.stderr.flush()
        return {"module": group, "tests": result.testsRun, "ok": result.wasSuccessful(),
                "failures": len(result.failures) + len(result.unexpectedSuccesses),
                "errors": len(result.errors), "skipped": len(result.skipped),
                "elapsed": time.monotonic() - started, "timings": result.timings,
                "fixtures": list(fixture_timings), "log": log_path}


def parallel_suite(suite, jobs, cache, failfast):
    groups = {}
    for test in cases(suite):
        module = type(test).__module__
        group = module + "." + type(test).__qualname__ if module == "test_devshell" else module
        groups.setdefault(group, []).append(test.id())
    logs = cache / "last-run"
    if logs.exists():
        shutil.rmtree(logs)
    logs.mkdir()
    print(f"Running test groups with {jobs} workers; engine/frontend suites run exclusively. Logs: {logs}", flush=True)
    results = []
    stopped = False

    def report(result):
        nonlocal stopped
        results.append(result)
        print(f"[{'PASS' if result['ok'] else 'FAIL'}] {result['module']}: "
              f"{result['tests']} tests, {result['elapsed']:.2f}s", flush=True)
        if not result["ok"]:
            print(Path(result["log"]).read_text(errors="replace"), file=sys.stderr, flush=True)
            stopped = failfast

    with ProcessPoolExecutor(max_workers=jobs, mp_context=multiprocessing.get_context("spawn")) as pool:
        pending = {}
        ordinary = iter((module, names) for module, names in groups.items()
                        if not exclusive(module))

        def submit_next():
            item = next(ordinary, None)
            if item is None:
                return False
            module, names = item
            future = pool.submit(run_group, module, names, str(logs / (module + ".log")), failfast)
            pending[future] = module
            return True

        for _ in range(jobs):
            if not submit_next():
                break
        while pending:
            done, _ = wait(pending, return_when=FIRST_COMPLETED)
            # Observe every completed result before scheduling more, so a
            # concurrent failure cannot enqueue another module under failfast.
            for future in done:
                pending.pop(future)
                report(future.result())
            if not stopped:
                for _ in done:
                    submit_next()
        # Nothing else runs alongside these groups, including Docker engines
        # started by scope/dev-shell/run-target tests and native frontends.
        for module, names in groups.items():
            if stopped:
                break
            if exclusive(module):
                report(pool.submit(run_group, module, names,
                                   str(logs / (module + ".log")), failfast).result())
    return results


def print_durations(timings, fixtures, count):
    if count <= 0:
        return
    print("\nSlowest tests (fixture/class setup is included in total suite time):", file=sys.stderr)
    for seconds, name in sorted(timings, reverse=True)[:count]:
        print(f"{seconds:8.2f}s  {name}", file=sys.stderr)
    if fixtures:
        print("\nNix fixture resolution (including builds):", file=sys.stderr)
        for seconds, name in sorted(fixtures, reverse=True)[:count]:
            print(f"{seconds:8.2f}s  {name.rsplit('#', 1)[-1]}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-f", "--failfast", action="store_true", help="stop on the first failure")
    parser.add_argument("--packaged", action="store_true", help="use Nix-built controllers too")
    parser.add_argument("-j", "--jobs", type=int, choices=(1, 2, 4), default=2,
                        help="test workers (default: 2); use 1 for serial verbose execution")
    parser.add_argument("--clean-cache", action="store_true",
                        help="release fixture roots and remove cached launchers/logs; never run Nix GC")
    parser.add_argument("--durations", type=int, default=15, help="number of slow tests to report")
    parser.add_argument("tests", nargs="*", help="unittest module, class or method names")
    args = parser.parse_args()
    os.chdir(HERE)
    os.environ.setdefault("CARGO_BUILD_JOBS", "2")
    cache = cache_directory()
    cache.mkdir(parents=True, exist_ok=True)
    # One scheduler owns this checkout/cache, including explicit cache cleanup.
    with (cache / "run.lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another test runner is using this checkout")
        if args.clean_cache:
            if args.tests:
                parser.error("--clean-cache cannot be combined with test selectors")
            for name in ("roots", "apps", "last-run"):
                directory = cache / name
                if directory.is_symlink():
                    directory.unlink()
                elif directory.exists():
                    shutil.rmtree(directory)
            # Leave the lock inode in place so a competing invocation cannot
            # acquire a newly-created lock while this one is still held.
            print(f"Released fixture roots and removed launchers/logs in {cache}. "
                  "Store data follows your normal Nix GC policy.")
            return 0
        if args.packaged:
            os.environ.pop("GOBLINS_LOCAL_CONTROLLER", None)
        else:
            print("Building the Rust workspace incrementally with Cargo...", flush=True)
            build = subprocess.check_output([
                os.environ.get("CARGO", "cargo"), "build", "--locked", "--workspace",
                "--message-format=json",
            ], text=True)
            artifacts = [json.loads(line) for line in build.splitlines()]
            executable = next(a["executable"] for a in artifacts
                              if a.get("reason") == "compiler-artifact" and a.get("executable")
                              and a["target"]["name"] == "goblins")
            os.environ["GOBLINS_LOCAL_CONTROLLER"] = executable
            # A launcher inherited from an older run must not hide new code.
            os.environ.pop("GOBLINS_APP", None)
        loader = unittest.defaultTestLoader
        suite = (loader.loadTestsFromNames(args.tests) if args.tests else
                 loader.discover(str(HERE / "tests")))
        suite = unique_suite(suite)
        if args.jobs > 1 and not loader.errors:
            # Build the ignored Session test driver before workers start, so
            # they neither contend for Cargo nor each launch another build.
            if any(type(test).__module__ == "test_integration" for test in cases(suite)):
                rust_driver()
            started = time.monotonic()
            results = parallel_suite(suite, args.jobs, cache, args.failfast)
            tests = sum(r["tests"] for r in results)
            skipped = sum(r["skipped"] for r in results)
            ok = all(r["ok"] for r in results)
            print(f"\nRan {tests} tests in {time.monotonic()-started:.3f}s\n"
                  f"{'OK' if ok else 'FAILED'} (skipped={skipped})", flush=True)
            print_durations([t for r in results for t in r["timings"]],
                            [t for r in results for t in r["fixtures"]], args.durations)
            return 0 if ok else 1
        result = unittest.TextTestRunner(verbosity=2, resultclass=TimedResult, failfast=args.failfast).run(suite)
        print_durations(result.timings, fixture_timings, args.durations)
        return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
