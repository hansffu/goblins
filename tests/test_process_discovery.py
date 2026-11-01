"""Classify same-executable supervisors by held scope lifetime descriptors."""
from pathlib import Path
import tempfile
import unittest

from daemon_support import supervisor_children


class ProcessDiscoveryTests(unittest.TestCase):
    def test_same_executable_scope_and_goblin_children_are_distinguished(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc, state = root / "proc", root / "state"
            children = proc / "10/task/10/children"
            children.parent.mkdir(parents=True)
            # Goblin supervisors deliberately precede the scope children.
            children.write_text("11 12 13 14 15 16")
            for pid in range(11, 17):
                directory = proc / str(pid)
                (directory / "fd").mkdir(parents=True)
                (directory / "exe").symlink_to(root / (
                    "other-program" if pid == 15 else "goblins-supervisor-bwrap"))
            (proc / "13/fd/5").symlink_to(state / "scopes/scratch/lock")
            (proc / "14/fd/7").symlink_to(state / "scopes/work/lock")
            # Unrelated locks and non-supervisor children never create scopes.
            (proc / "11/fd/5").symlink_to(state / "11/resources/.gc-lease")
            (proc / "15/fd/5").symlink_to(state / "scopes/unrelated/lock")
            (proc / "16/exe").unlink()  # Already exited child.
            self.assertEqual(supervisor_children(10, state, proc),
                             ({"scratch": 13, "work": 14}, {11, 12}))

    def test_duplicate_scope_owners_are_reported(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc, state = root / "proc", root / "state"
            children = proc / "10/task/10/children"
            children.parent.mkdir(parents=True)
            children.write_text("11 12")
            for pid in (11, 12):
                directory = proc / str(pid)
                (directory / "fd").mkdir(parents=True)
                (directory / "exe").symlink_to(root / "goblins-supervisor-bwrap")
                (directory / "fd/5").symlink_to(state / "scopes/work/lock")
            with self.assertRaisesRegex(ValueError, "multiple supervisors hold scope work"):
                supervisor_children(10, state, proc)
