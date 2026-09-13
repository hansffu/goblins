"""Working-directory regression through the real CLI and sandbox."""
from pathlib import Path
import tempfile
import unittest

from daemon_support import Daemon
from terminal_support import Terminal


class WorkspaceTests(unittest.TestCase):
    def test_each_cli_run_keeps_its_directory_and_live_descendants(self):
        with tempfile.TemporaryDirectory(prefix="goblin-cwd-") as root:
            daemon = Daemon()
            self.addCleanup(daemon.close)
            for name in ("first project", "second-project"):
                cwd = Path(root) / name
                (cwd / "deep/nested").mkdir(parents=True)
                (cwd / "deep/nested/input").write_text("initial\n")
                terminal = Terminal([str(daemon.app), "--state-dir", str(daemon.state),
                                     "run", "shell"], cwd=cwd)
                self.addCleanup(terminal.close)
                terminal.send("printf 'CWD=%s\\n' $PWD; cat deep/nested/input; echo written > deep/nested/output\n")
                actual = terminal.expect(r"(?:^|\n)CWD=([^\n]+)\n").group(1).decode()
                self.assertEqual(actual, str(cwd))
                terminal.expect(r"(?:^|\n)initial\n")
                daemon.wait(lambda: (cwd / "deep/nested/output").exists())
                self.assertEqual((cwd / "deep/nested/output").read_text(), "written\n")
                (cwd / "deep/nested/input").write_text("host-update\n")
                terminal.send("cat deep/nested/input\n")
                terminal.expect(r"(?:^|\n)host-update\n")
            self.assertEqual(len(daemon.rpc.call("sessions.list", {})), 2)

    def test_cli_retains_explicit_daemon_snapshot(self):
        with tempfile.TemporaryDirectory(prefix="goblin-cwd-") as root:
            cwd = Path(root)
            (cwd / "input").write_text("original\n")
            daemon = Daemon(workspace=cwd)
            self.addCleanup(daemon.close)
            terminal = Terminal([str(daemon.app), "--state-dir", str(daemon.state),
                                 "run", "shell"], cwd=cwd)
            self.addCleanup(terminal.close)
            terminal.send("printf 'CWD=%s\\n' $PWD; echo changed > input; cat input\n")
            terminal.expect(r"(?:^|\n)CWD=/workspace\n")
            terminal.expect(r"(?:^|\n)changed\n")
            self.assertEqual((cwd / "input").read_text(), "original\n")


if __name__ == "__main__":
    unittest.main()
