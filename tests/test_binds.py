"""Startup mkSandbox bind options through the production Rust CLI."""
import os
from pathlib import Path
import shlex
import tempfile
import unittest

from support import command
from test_connected import Terminal


class BindTests(unittest.TestCase):
    def test_all_four_bind_options_and_live_host_changes(self):
        with tempfile.TemporaryDirectory(prefix="goblins-bind-test-") as directory:
            root = Path(directory)
            for name in ("readonly", "writable"):
                (root / name).mkdir()
                (root / name / "data").write_text("initial")
            for name in ("ro-file", "rw-file", "secret"):
                (root / name).write_text("initial")
            (root / "readonly/host-link").symlink_to(root / "secret")
            flake = "path:" + str(Path(__file__).resolve().parents[1])
            app = Path(command(["nix", "build", "--print-out-paths", "--out-link", root / "app",
                                flake + "#checks.x86_64-linux.bound-goblins"])) / "bin/goblins"
            state = root / "control"
            server = Terminal([str(app), "--state-dir", str(state), "serve"],
                              env={**os.environ, "GOBLINS_TEST_ROOT": str(root)})
            self.addCleanup(server.close)
            server.expect("Goblins serving")
            client = Terminal([str(app), "--state-dir", str(state), "run", "bound"])
            self.addCleanup(client.close)
            server.expect("bound connected")
            client.expect("bind-test>")
            for name in ("readonly/data", "ro-file", "writable/data", "rw-file"):
                path = shlex.quote(str(root / name))
                client.send(f"printf 'READ=%s\\n' \"$(cat {path})\"; echo updated > {path}; printf 'WRITE=%s\\n' \"$?\"\n")
                client.expect(r"(?:^|\n)READ=initial\n")
                readonly = name in ("readonly/data", "ro-file")
                client.expect(r"(?:^|\n)WRITE=" + ("1" if readonly else "0") + r"\n")
                self.assertEqual((root / name).read_text(), "initial" if readonly else "updated\n")
            (root / "readonly/data").write_text("host-update")
            client.send(f"printf 'HOST_CHANGE=%s\\n' \"$(cat {root}/readonly/data)\"; test -e {root}/readonly/host-link; printf 'HOST_LINK_HIDDEN=%s\\n' \"$?\"; test -e {state}/serve.sock; printf 'CONTROL_HIDDEN=%s\\n' \"$?\"\n")
            client.expect(r"(?:^|\n)HOST_CHANGE=host-update\n")
            client.expect(r"(?:^|\n)HOST_LINK_HIDDEN=1\n")
            client.expect(r"(?:^|\n)CONTROL_HIDDEN=1\n")
            client.send("exit\n")
            self.assertEqual(client.wait(), 0)
            server.expect("Goblin stopped")
            # A configured symlink may not expose protected sources, including
            # a custom host attachment directory outside /run.
            (root / "readonly").rename(root / "previous-readonly")
            for source in (state, Path("/nix/var/nix/daemon-socket"), Path("/proc")):
                (root / "readonly").symlink_to(source)
                rejected = Terminal([str(app), "--state-dir", str(state), "run", "bound"])
                self.addCleanup(rejected.close)
                rejected.expect("overlaps protected path")
                self.assertNotEqual(rejected.wait(), 0)
                server.expect("Goblin stopped")
                (root / "readonly").unlink()
            server.send("quit\n")
            self.assertEqual(server.wait(), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
