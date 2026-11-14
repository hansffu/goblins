"""A server started with --workspace gives its goblins a private snapshot of
that directory instead of the directory itself. What a snapshot copies and
skips is unit-tested in libs/supervisor/src/snapshot.rs."""
import os
from pathlib import Path
import socket
import tempfile
import unittest

from daemon_support import Daemon
from support import step
from terminal_support import Terminal


class SnapshotWorkspaceWorkflow(unittest.TestCase):
    def test_a_goblin_works_in_a_snapshot_of_the_workspace(self):
        with tempfile.TemporaryDirectory(prefix="goblin-snapshot-") as root:
            root = Path(root)
            secret = root / "secret"
            secret.write_text("host secret\n")
            source = root / "workspace"
            (source / "nested").mkdir(parents=True)
            (source / "input").write_text("original\n")
            (source / "nested/deeper").write_text("nested\n")
            (source / ".git").write_text("gitdir: /host/shared/git\n")
            (source / "escape").symlink_to(secret)
            os.mkfifo(source / "fifo")
            listener = socket.socket(socket.AF_UNIX)
            self.addCleanup(listener.close)
            listener.bind(str(source / "host.sock"))
            d = Daemon(workspace=source)
            self.addCleanup(d.close)
            terminal = Terminal([str(d.app), "--state-dir", str(d.state), "run", "shell"], cwd=source)
            self.addCleanup(terminal.close)

            with step("the goblin starts in the snapshot, which has the files only"):
                terminal.send("printf 'CWD=%s\\n' $PWD; printf 'ENTRY=%s\\n' (ls -A | sort); cat nested/deeper\n")
                terminal.expect(r"(?:^|\n)CWD=/workspace\n")
                entries = [terminal.expect(r"(?:^|\n)ENTRY=([^\n]*)\n").group(1).decode() for _ in range(3)]
                self.assertEqual(entries, ["escape", "input", "nested"])
                terminal.expect(r"(?:^|\n)nested\n")

            with step("a link out of the workspace leads nowhere"):
                terminal.send("test -e escape; printf 'ESCAPE=%s\\n' $status\n")
                terminal.expect(r"(?:^|\n)ESCAPE=1\n")

            with step("writes stay in the snapshot"):
                terminal.send("echo changed > input; cat input\n")
                terminal.expect(r"(?:^|\n)changed\n")
                self.assertEqual((source / "input").read_text(), "original\n")
                terminal.send("exit\n")
                self.assertEqual(terminal.wait(), 0)


if __name__ == "__main__":
    unittest.main()
