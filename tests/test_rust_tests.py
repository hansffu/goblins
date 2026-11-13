"""Rust test actor fixture discovery must use fresh Cargo production artifacts."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import rust_tests


def artifact(path, **updates):
    value = {"reason": "compiler-artifact", "executable": str(path),
             "target": {"name": "goblins-supervisor-bwrap", "kind": ["bin"]},
             "profile": {"test": False}}
    return {**value, **updates}


class RustRunnerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="goblins-rust-runner-")
        self.addCleanup(temporary.cleanup)
        self.executable = Path(temporary.name) / "supervisor"
        self.executable.write_text("fixture artifact; never executed\n")
        self.executable.chmod(0o700)

    def test_only_the_production_binary_artifact_is_selected(self):
        valid = artifact(self.executable)
        self.assertEqual(rust_tests.supervisor_executable([
            {"reason": "build-finished", "success": True},
            artifact(self.executable, profile={"test": True}),
            artifact(self.executable, target={"name": "other", "kind": ["bin"]}),
            artifact(self.executable, target={"name": "goblins-supervisor-bwrap", "kind": ["lib"]}),
            artifact(self.executable, executable=None), valid]), self.executable)

    def test_missing_or_ambiguous_production_artifacts_are_errors(self):
        for artifacts in ([], [artifact(self.executable), artifact(self.executable)]):
            with self.subTest(artifacts=artifacts), self.assertRaisesRegex(ValueError, "exactly one"):
                rust_tests.supervisor_executable(artifacts)

    def test_relative_missing_or_nonexecutable_artifacts_are_errors(self):
        for path in (Path("relative-supervisor"), self.executable.with_name("missing")):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "absolute executable"):
                rust_tests.supervisor_executable([artifact(path)])
        self.executable.chmod(0o600)
        with self.assertRaisesRegex(ValueError, "absolute executable"):
            rust_tests.supervisor_executable([artifact(self.executable)])

    def test_build_finishes_before_tests_and_overrides_any_stale_fixture_pin(self):
        for args in ([], ["-p", "goblins-controller", "member", "--", "--test-threads=2"]):
            sequence = []
            with self.subTest(args=args), \
                    mock.patch.dict(os.environ, {"GOBLINS_TEST_SUPERVISOR_EXECUTABLE": "/stale"}), \
                    mock.patch.object(rust_tests, "cargo_artifacts", side_effect=lambda *args: sequence.append("build") or [artifact(self.executable)]) as build, \
                    mock.patch.object(rust_tests.subprocess, "call", side_effect=lambda *args, **kwargs: sequence.append("test") or 7) as test:
                self.assertEqual(rust_tests.main(args), 7)
                self.assertEqual(sequence, ["build", "test"])
                build.assert_called_once_with("build", "--locked", "-j", "2", "-p",
                                              "goblins-supervisor-bwrap", "--bin", "goblins-supervisor-bwrap")
                command = test.call_args.args[0]
                self.assertEqual(command[1:3], ["test", "--locked"])
                self.assertEqual(command[3:], args or ["--workspace", "-j", "2", "--", "--test-threads=4"])
                self.assertEqual(test.call_args.kwargs["env"]["GOBLINS_TEST_SUPERVISOR_EXECUTABLE"],
                                 str(self.executable))

    def test_failed_fixture_build_cannot_run_tests_with_an_old_pin(self):
        with mock.patch.object(rust_tests, "cargo_artifacts", side_effect=RuntimeError("build failed")), \
                mock.patch.object(rust_tests.subprocess, "call") as test:
            with self.assertRaisesRegex(RuntimeError, "build failed"):
                rust_tests.main([])
            test.assert_not_called()
