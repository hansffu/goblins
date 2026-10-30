"""Exercise scheduling, failure propagation and cache ownership in subprocesses."""
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

from build_support import daemon_environment, fixture_app, nix_bin
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
        # Nested scheduler probes use synthetic Cargo output and private caches.
        # The diagnostic regression below deliberately compiles an invalid crate.
        cargo = self.directory / "cargo"
        artifacts = [{
            "reason": "compiler-artifact", "target": {"name": "goblins"},
            "executable": "/bin/true",
        }, {
            "reason": "compiler-artifact", "target": {"name": "goblins-codex"},
            "executable": str(self.directory / "built-adapter"),
        }, {
            "reason": "compiler-artifact", "target": {"name": "goblins-supervisor-bwrap"},
            "executable": str(self.directory / "built-scope-runtime"),
        }]
        cargo.write_text("#!" + sys.executable + "\nimport json\nfor artifact in " +
                         repr(artifacts) + ":\n    print(json.dumps(artifact))\n")
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

    def test_workers_use_the_adapter_reported_by_cargo(self):
        self.env["GOBLINS_CODEX_ADAPTER"] = "/stale/adapter"
        self.env["GOBLINS_LOCAL_SCOPE_RUNTIME"] = "/stale/scope-runtime"
        self.module("probe_adapter", "self.assertEqual(os.environ['GOBLINS_CODEX_ADAPTER'],str(root/'built-adapter'))\n"
                    "self.assertEqual(os.environ['GOBLINS_LOCAL_SCOPE_RUNTIME'],str(root/'built-scope-runtime'))")
        for jobs in (1, 2):
            with self.subTest(jobs=jobs):
                self.run_runner("--jobs", str(jobs), "probe_adapter")

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
        self.module("test_host_commands", "self.assertEqual(len(list(root.glob('finished_*'))),4)")
        host = self.directory / "test_host_commands.py"
        host.write_text(host.read_text().replace("class Probe(", "class HostDockerAcceptance("))
        result = self.run_runner("--jobs", "4", "test_docker", "test_host_commands", *names)
        self.assertIn("Ran 6 tests", result.stdout)
        self.assertIn("[PASS] test_docker", result.stdout)
        self.assertIn("[PASS] test_host_commands.HostDockerAcceptance", result.stdout)

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

    def test_compile_failures_show_rust_diagnostics_in_both_build_entry_points(self):
        crate = self.directory / "broken"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            '[package]\nname="diagnostic-probe"\nversion="0.0.0"\nedition="2021"\n')
        (crate / "src/main.rs").write_text('fn main() { missing_diagnostic_probe(); }\n')
        subprocess.run([shutil.which("cargo"), "generate-lockfile", "--offline", "--manifest-path",
                        str(crate / "Cargo.toml")], check=True, capture_output=True, text=True)
        cargo = Path(self.env["CARGO"])
        cargo.write_text("#!" + sys.executable + "\nimport os,sys\n" +
                         f"os.execv({shutil.which('cargo')!r}, [{shutil.which('cargo')!r}, *sys.argv[1:], "
                         f"'--manifest-path', {str(crate / 'Cargo.toml')!r}, '--offline', "
                         f"'--target-dir', {str(crate / 'target')!r}])\n")
        # The production entry points both consume JSON artifact paths. Compile
        # a real, dependency-free invalid crate through each, not a fake error.
        self.env.pop("GOBLINS_TEST_DRIVER", None)
        for args in ([str(self.runner), "-j", "1", "probe_unused"],
                     ["-c", "from support import rust_driver; rust_driver()"]):
            with self.subTest(args=args):
                result = subprocess.run([sys.executable, *args], env=self.env,
                                        capture_output=True, text=True, timeout=30)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("error[E0425]", result.stderr)
                self.assertIn("cannot find function `missing_diagnostic_probe`", result.stderr)

    def test_launcher_and_direct_daemon_pin_nix_with_an_explicit_wrapper_override(self):
        pinned, ambient, barrier = [self.directory / name for name in ("pinned/bin", "ambient", "barrier")]
        for directory in (pinned, ambient, barrier):
            directory.mkdir(parents=True)
            for tool in ("nix", "nix-store"):
                (directory / tool).write_text("#!/bin/sh\nexit 0\n")
                (directory / tool).chmod(0o755)
        controller = self.directory / "controller"
        controller.write_text("#!" + sys.executable + "\nimport json,shutil\n"
                              "print(json.dumps([shutil.which('nix'),shutil.which('nix-store')]))\n")
        controller.chmod(0o755)
        env = {**self.env, "GOBLINS_LOCAL_CONTROLLER": str(controller), "PATH": str(ambient)}
        env.pop("GOBLINS_LOCAL_SCOPE_RUNTIME", None)
        fixture_app.cache_clear()
        self.addCleanup(fixture_app.cache_clear)
        with patch.dict(os.environ, env, clear=True), patch("build_support.fixture_path", side_effect=lambda ref:
                pinned.parent if ref.endswith("#nix^out") else self.directory / "manifest.json"):
            app = fixture_app("pin-probe")
            expected = [str(pinned / tool) for tool in ("nix", "nix-store")]
            self.assertEqual(json.loads(subprocess.check_output([app], env=env, text=True)), expected)
            default = daemon_environment(env)
            self.assertEqual([shutil.which(tool, path=default["PATH"]) for tool in ("nix", "nix-store")], expected)
            injected = daemon_environment(env, tool_path=barrier)
            self.assertEqual(shutil.which("nix", path=injected["PATH"]), str(barrier / "nix"))
            self.assertEqual(env["PATH"], str(ambient))
        # Also resolve the real package: a multi-output derivation must not
        # turn into a newline-containing PATH that silently uses ambient tools.
        real = nix_bin()
        isolated = daemon_environment({**os.environ, "PATH": str(ambient)})
        for tool in ("nix", "nix-store"):
            self.assertTrue((real / tool).is_file(), real / tool)
            self.assertEqual(shutil.which(tool, path=isolated["PATH"]), str(real / tool))
            self.assertIn("(Nix)", subprocess.check_output([tool, "--version"], env=isolated, text=True))
