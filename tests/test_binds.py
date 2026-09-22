"""Startup mkSandbox bind options through the production Rust CLI."""
import os
from pathlib import Path
import shlex
import tempfile
import unittest

from support import command
from terminal_support import Terminal
from daemon_support import Daemon


class BindTests(unittest.TestCase):
    def test_fish_config_is_an_explicit_startup_bind(self):
        with tempfile.TemporaryDirectory(prefix="gb-") as directory:
            root = Path(directory)
            flake = "path:" + str(Path(__file__).resolve().parents[1])
            app = Path(command(["nix", "build", "--print-out-paths", "--out-link", root / "app",
                                flake + "#checks.x86_64-linux.configured-shell"])) / "bin/goblins"
            home = root / "home"
            fish = home / ".config/fish"
            (fish / "functions").mkdir(parents=True)
            targets = Path(command(["nix", "build", "--print-out-paths", "--out-link", root / "targets",
                                    flake + "#checks.x86_64-linux.bind-targets"]))
            (fish / "config.fish").symlink_to(targets / "config.fish")
            (fish / "functions/config_probe.fish").symlink_to(targets / "functions/config_probe.fish")
            (home / "unselected-secret").write_text("private-home")
            state = root / "control"
            server = Daemon(app, env={**os.environ, "HOME": str(home)})
            self.addCleanup(server.close)
            state = server.state
            client = Terminal([str(app), "--state-dir", str(state), "run", "shell"])
            self.addCleanup(client.close)
            client.expect("[>#]", timeout=60)
            client.send("printf 'CONFIG=%s\\n' $GOBLIN_FISH_CONFIG; config_probe; echo forbidden > $HOME/.config/fish/new-file; printf 'CONFIG_RO=%s\\n' $status\n")
            client.expect(r"(?:^|\n)CONFIG=loaded\n")
            client.expect(r"(?:^|\n)config-function-loaded\n")
            client.expect(r"(?:^|\n)CONFIG_RO=1\n")
            client.send("goblins-symlink-output-probe; printf 'SYMLINK_OUTPUT=%s\\n' $status\n")
            client.expect(r"(?:^|\n)SYMLINK_OUTPUT=0\n")
            client.send(f"test -e {targets}/unselected-secret; printf 'STORE_SIBLING_HIDDEN=%s\\n' $status; test -e $HOME/unselected-secret; printf 'HOME_PRIVATE=%s\\n' $status\n")
            client.expect(r"(?:^|\n)STORE_SIBLING_HIDDEN=1\n")
            client.expect(r"(?:^|\n)HOME_PRIVATE=1\n")
            self.assertFalse((fish / "new-file").exists())
            client.send("exit\n")
            self.assertEqual(client.wait(), 0)

    def test_all_four_bind_options_and_live_host_changes(self):
        with tempfile.TemporaryDirectory(prefix="gb-") as directory:
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
            server = Daemon(app, env={**os.environ, "GOBLINS_TEST_ROOT": str(root)})
            self.addCleanup(server.close)
            state = server.state
            client = Terminal([str(app), "--state-dir", str(state), "run", "bound"])
            self.addCleanup(client.close)
            client.expect("bind-test>")
            for name in ("readonly/data", "ro-file", "writable/data", "rw-file"):
                path = shlex.quote(str(root / name))
                client.send(f"printf 'READ=%s\\n' \"$(cat {path})\"; echo updated > {path}; printf 'WRITE=%s\\n' \"$?\"\n")
                client.expect(r"(?:^|\n)READ=initial\n")
                readonly = name in ("readonly/data", "ro-file")
                client.expect(r"(?:^|\n)WRITE=" + ("1" if readonly else "0") + r"\n")
                self.assertEqual((root / name).read_text(), "initial" if readonly else "updated\n")
            (root / "readonly/data").write_text("host-update")
            client.send(f"printf 'HOST_CHANGE=%s\\n' \"$(cat {root}/readonly/data)\"; test -e {root}/readonly/host-link; printf 'HOST_LINK_HIDDEN=%s\\n' \"$?\"; test -e {state}/host.sock; printf 'CONTROL_HIDDEN=%s\\n' \"$?\"\n")
            client.expect(r"(?:^|\n)HOST_CHANGE=host-update\n")
            client.expect(r"(?:^|\n)HOST_LINK_HIDDEN=1\n")
            client.expect(r"(?:^|\n)CONTROL_HIDDEN=1\n")
            client.send("exit\n")
            self.assertEqual(client.wait(), 0)
            # A configured symlink may not expose protected sources, including
            # a custom host attachment directory outside /run.
            (root / "readonly").rename(root / "previous-readonly")
            for source in (state, Path("/nix/var/nix/daemon-socket"), Path("/proc")):
                (root / "readonly").symlink_to(source)
                rejected = Terminal([str(app), "--state-dir", str(state), "run", "bound"])
                self.addCleanup(rejected.close)
                rejected.expect("overlaps protected path")
                self.assertNotEqual(rejected.wait(), 0)
                (root / "readonly").unlink()


if __name__ == "__main__":
    unittest.main(verbosity=2)
