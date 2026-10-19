#!/usr/bin/env python3
"""Run Python tests with an incremental Cargo controller and timing evidence."""
import argparse
import fcntl
import json
import os
import subprocess
import sys
import time
import unittest

from build_support import cache_directory, fixture_timings
from support import HERE


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-f", "--failfast", action="store_true", help="stop on the first failure")
    parser.add_argument("--packaged", action="store_true", help="use Nix-built controllers too")
    parser.add_argument("--durations", type=int, default=15, help="number of slow tests to report")
    parser.add_argument("tests", nargs="*", help="unittest module, class or method names")
    args = parser.parse_args()
    os.chdir(HERE)
    os.environ.setdefault("CARGO_BUILD_JOBS", "2")
    cache = cache_directory()
    cache.mkdir(parents=True, exist_ok=True)
    # These suites use global process markers and nested Docker/Nix; accidental
    # concurrent invocations make both test results and desktop I/O much worse.
    with (cache / "run.lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another test runner is using this checkout")
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
        result = unittest.TextTestRunner(verbosity=2, resultclass=TimedResult, failfast=args.failfast).run(suite)
        if args.durations > 0:
            print("\nSlowest tests (fixture/class setup is included in total suite time):", file=sys.stderr)
            for seconds, name in sorted(result.timings, reverse=True)[:args.durations]:
                print(f"{seconds:8.2f}s  {name}", file=sys.stderr)
            if fixture_timings:
                print("\nNix fixture resolution (including builds):", file=sys.stderr)
                for seconds, name in sorted(fixture_timings, reverse=True)[:args.durations]:
                    print(f"{seconds:8.2f}s  {name.rsplit('#', 1)[-1]}", file=sys.stderr)
        return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
