"""The Claude mod: Claude Code validates it and runs its own tests (the claude-mod check)."""
import unittest

from build_support import fixture_path
from support import FLAKE


class ClaudeModTests(unittest.TestCase):
    def test_mod_validates_and_its_tests_pass(self):
        # The check fails the build unless every test passes.
        fixture_path(FLAKE + "#checks.x86_64-linux.claude-mod")


if __name__ == "__main__":
    unittest.main()
