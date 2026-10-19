"""Exercise scheduling, failure propagation and cache ownership in subprocesses."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from support import HERE


class RunnerTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="goblins-runner-")
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        # Put the runner beside synthetic modules so its script directory
        # cannot shadow stand-ins such as the exclusive test_docker group.
        self.runner = self.directory / "run.py"
        self.runner.write_text((HERE / "tests/run.py").read_text())
        self.cache = self.directory / "cache"
        self.env = {**os.environ, "GOBLINS_TEST_CACHE": str(self.cache),
                    "PYTHONPATH": str(self.directory) + os.pathsep + str(HERE / "tests"),
                    "PROBE_DIRECTORY": str(self.directory)}
        # These tests exercise the scheduler, not compilation or Goblins. No
        # Nix fixtures are used and no nested runner touches the outer cache.
        cargo = self.directory / "cargo"
        cargo.write_text("#!" + sys.executable + "\nimport json\nprint(json.dumps(" + repr({
            "reason": "compiler-artifact", "target": {"name": "goblins"},
            "executable": "/bin/true",
        }) + "))\n")
        cargo.chmod(0o755)
        self.env["CARGO"] = str(cargo)

    def module(self, name, body):
        (self.directory / (name + ".py")).write_text(
            "import os,time,unittest\nfrom pathlib import Path\n"
            "root=Path(os.environ['PROBE_DIRECTORY'])\n"
            "class Probe(unittest.TestCase):\n    def test_probe(self):\n" +
            "\n".join("        " + line for line in body.splitlines()) + "\n")

    def run_runner(self, *args, code=0):
        result = subprocess.run([sys.executable, str(self.runner), "--durations", "0", *args],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        return result

    def test_two_workers_overlap_and_keep_complete_module_logs(self):
        for name, other in [("probe_a", "probe_b"), ("probe_b", "probe_a")]:
            self.module(name, f"(root/'{name}').touch()\n"
                        "deadline=time.monotonic()+10\n"
                        f"while not (root/'{other}').exists() and time.monotonic()<deadline: time.sleep(.01)\n"
                        f"self.assertTrue((root/'{other}').exists())\n"
                        f"print('{name}-child-output',flush=True)")
        result = self.run_runner("--jobs", "2", "probe_a", "probe_b")
        self.assertIn("Ran 2 tests", result.stdout)
        for name in ("probe_a", "probe_b"):
            log = (self.cache / "last-run" / (name + ".log")).read_text()
            self.assertIn(name + "-child-output", log)
            self.assertIn("OK", log)

    def test_parallel_failfast_does_not_schedule_another_module(self):
        self.module("probe_fail", "os.write(1,b'\\xff')\nself.fail('deliberate-worker-failure')")
        self.module("probe_later", "(root/'unexpected').touch()")
        for jobs in (2, 4):
            with self.subTest(jobs=jobs):
                waiting = [f"probe_wait_{i}" for i in range(jobs - 1)]
                for name in waiting:
                    self.module(name, "time.sleep(1)\nself.assertTrue(True)")
                result = self.run_runner("--jobs", str(jobs), "--failfast", "probe_fail",
                                         *waiting, "probe_later", code=1)
                self.assertIn("deliberate-worker-failure", result.stderr)
                self.assertFalse((self.directory / "unexpected").exists())
                self.assertIn("FAILED", result.stdout)

    def test_four_workers_overlap_before_exclusive_groups_run(self):
        names = [f"probe_{i}" for i in range(4)]
        for name in names:
            self.module(name, f"(root/'started_{name}').touch()\n"
                        "deadline=time.monotonic()+10\n"
                        "while len(list(root.glob('started_*'))) < 4 and time.monotonic()<deadline: time.sleep(.01)\n"
                        "self.assertEqual(len(list(root.glob('started_*'))),4)\n"
                        f"(root/'finished_{name}').touch()")
        # This synthetic module is classified as exclusive without using an
        # engine. Selecting it first verifies scheduling, not selector order.
        self.module("test_docker", "self.assertEqual(len(list(root.glob('finished_*'))),4)")
        result = self.run_runner("--jobs", "4", "test_docker", *names)
        self.assertIn("Ran 5 tests", result.stdout)
        self.assertIn("[PASS] test_docker", result.stdout)

    def test_cleanup_releases_only_fixture_cache_and_keeps_lock_inode(self):
        for name in ("roots", "apps", "last-run"):
            (self.cache / name).mkdir(parents=True)
            (self.cache / name / "entry").write_text("fixture")
        outside = self.directory / "retained-store-output"
        outside.write_text("keep")
        (self.cache / "roots" / "root").symlink_to(outside)
        lock = self.cache / "run.lock"
        lock.touch()
        inode = lock.stat().st_ino
        self.run_runner("--clean-cache")
        self.assertEqual(outside.read_text(), "keep")
        self.assertEqual(lock.stat().st_ino, inode)
        self.assertTrue(all(not (self.cache / name).exists() for name in ("roots", "apps", "last-run")))

    def test_overlapping_selectors_execute_each_case_once(self):
        for jobs in (1, 4):
            with self.subTest(jobs=jobs):
                name = f"probe_once_{jobs}"
                self.module(name, f"marker=root/'ran_{jobs}'\nself.assertFalse(marker.exists())\nmarker.touch()")
                result = self.run_runner("--jobs", str(jobs), name, name + ".Probe.test_probe")
                self.assertIn("Ran 1 test", result.stdout + result.stderr)
