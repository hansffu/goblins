"""Feature coverage, overlap, conservative routing and build-free planning."""
from collections import Counter
from io import StringIO
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from contextlib import redirect_stdout

from run import unique_suite
from tasks import ROOT, cases, changed_paths, check_manifest, commands, load_manifest, load_suite, make_plan, recommend


class FeatureTests(unittest.TestCase):
    def test_every_discovered_test_has_a_feature_and_selectors_are_valid(self):
        data = load_manifest()
        self.assertEqual(check_manifest(data), len(list(cases(load_suite()))))

    def test_cross_feature_tests_are_selected_once(self):
        data = load_manifest()
        plan = make_plan(("docker", "devshell", "raids", "docker"), data)
        suite = unique_suite(load_suite(plan.selectors))
        ids = [test.id() for test in cases(suite)]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(sum(name.startswith("test_devshell.DevShellDockerTests.") for name in ids), 2)
        self.assertEqual(sum(name.startswith("test_run_targets.RunTargetDockerTests.") for name in ids), 1)
        self.assertEqual(plan.features, ("docker", "devshell", "raids"))

    def test_deduplication_keeps_class_setup_together(self):
        setups = Counter()

        class First(unittest.TestCase):
            @classmethod
            def setUpClass(cls):
                setups["first"] += 1

            def test_one(self):
                pass

            def test_two(self):
                pass

        class Second(First):
            @classmethod
            def setUpClass(cls):
                setups["second"] += 1

        suite = unique_suite(unittest.TestSuite([
            First("test_one"), Second("test_one"), First("test_two"), First("test_one")]))
        result = unittest.TextTestRunner(stream=StringIO()).run(suite)
        self.assertTrue(result.wasSuccessful())
        self.assertEqual(result.testsRun, 3)
        self.assertEqual(setups, {"first": 1, "second": 1})

    def test_missing_and_stale_mapping_are_errors(self):
        data = load_manifest()
        data["features"]["infrastructure"]["python"].remove("test_features")
        with self.assertRaisesRegex(ValueError, "Unmapped tests:.*test_features"):
            check_manifest(data)
        data = load_manifest()
        data["features"]["exec"]["python"].append("test_exec.NoSuchClass")
        with self.assertRaisesRegex(ValueError, "Invalid test selection"):
            check_manifest(data)

    def test_path_recommendations_include_overlap_and_fall_back_to_full(self):
        data = load_manifest()
        names, _ = recommend(["crates/client/src/preview.rs"], data)
        self.assertEqual(set(names), {"devshell", "tui"})
        for paths in (["crates/protocol/src/rpc.rs"], ["tests/daemon_support.py"],
                      ["new/backend.rs"], ["README.org", "new/backend.rs"]):
            with self.subTest(paths=paths):
                self.assertEqual(recommend(paths, data)[0], ("full",))
        self.assertEqual(recommend(["docs/USAGE.org"], data)[0], ("docs",))
        self.assertIn("exec", recommend(["tests/test_exec.py"], data)[0])

    def test_full_plan_includes_every_suite_and_enables_codex_checks(self):
        data = load_manifest()
        plan = make_plan(("exec", "full"), data)
        self.assertIsNone(plan.selectors)
        self.assertEqual([label for label, _ in commands(plan, 4, data)],
                         ["Rust", "Python", "emacs", "nix", "Packaged smoke"])
        self.assertEqual(plan.env["GOBLINS_TEST_CODEX"], "1")
        self.assertEqual(make_plan(("codex",), data).env["GOBLINS_TEST_CODEX"], "1")
        with self.assertRaisesRegex(ValueError, "Unknown features"):
            make_plan(("exce",), data)

    def test_planning_never_builds_or_creates_the_test_cache(self):
        with tempfile.TemporaryDirectory(prefix="goblins-plan-") as folder:
            cache = Path(folder) / "cache"
            env = {**os.environ, "CARGO": "/nonexistent-cargo", "PATH": "",
                   "GOBLINS_TEST_CACHE": str(cache)}
            for args in (("--plan", "exec", "history"), ("--check",), ("--list",)):
                with self.subTest(args=args):
                    result = subprocess.run([sys.executable, str(ROOT / "tests/tasks.py"), *args],
                                            env=env, capture_output=True, text=True, timeout=20)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertFalse(cache.exists())

    def test_failed_stage_stops_the_task_and_reports_failure(self):
        from tasks import main
        with patch("tasks.subprocess.run", return_value=subprocess.CompletedProcess([], 7)) as run:
            with redirect_stdout(StringIO()):
                self.assertEqual(main(["exec"]), 1)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0], ["just", "test-rust"])

    def test_changed_paths_include_branch_worktree_untracked_and_both_sides_of_rename(self):
        with tempfile.TemporaryDirectory(prefix="goblins-changes-") as folder:
            root = Path(folder)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.STDOUT, text=True).strip()

            git("init", "-b", "main")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            git("config", "commit.gpgsign", "false")
            git("config", "core.hooksPath", "/dev/null")
            (root / "original").write_text("base\n")
            git("add", ".")
            git("commit", "-m", "base")
            git("checkout", "-b", "feature")
            (root / "committed").write_text("change\n")
            git("add", ".")
            git("commit", "-m", "feature")
            git("mv", "original", "renamed")
            (root / "committed").write_text("unstaged\n")
            (root / "untracked").write_text("new\n")
            self.assertEqual(set(changed_paths("main", root)),
                             {"original", "renamed", "committed", "untracked"})
