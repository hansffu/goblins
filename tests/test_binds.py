"""Configured binds as a person sets them up: the fish configuration from the
home directory, read-only and read-write directories and files, live host
edits, and a bind that would expose the server's own state. Bind validation
and pinning are Rust unit tests; a source swapped after its check is a host
test (libs/supervisor/src/session/tests/host.rs)."""
import os
from pathlib import Path
import shlex
import tempfile
import unittest

from build_support import fixture_app, fixture_path
from daemon_support import Daemon
from support import FLAKE, step
from terminal_support import Terminal


class ConfiguredBindsWorkflow(unittest.TestCase):
    def test_configured_binds(self):
        temporary = tempfile.TemporaryDirectory(prefix="gb-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        home = root / "home"
        fish = home / ".config/fish"
        (fish / "functions").mkdir(parents=True)
        targets = fixture_path(FLAKE + "#checks.x86_64-linux.bind-targets")
        (fish / "config.fish").symlink_to(targets / "config.fish")
        (fish / "functions/config_probe.fish").symlink_to(targets / "functions/config_probe.fish")
        (home / "unselected-secret").write_text("private-home")
        granted = root / "granted"
        for name in ("readonly", "writable"):
            (granted / name).mkdir(parents=True)
            (granted / name / "data").write_text("initial")
        for name in ("ro-file", "rw-file", "secret"):
            (granted / name).write_text("initial")
        (granted / "readonly/host-link").symlink_to(granted / "secret")
        app = fixture_app("configured-shell")
        d = Daemon(app, env={**os.environ, "HOME": str(home), "GOBLINS_TEST_ROOT": str(granted)})
        self.addCleanup(d.close)
        client = Terminal([str(app), "--state-dir", str(d.state), "run", "shell"])
        self.addCleanup(client.close)
        client.expect("[>#]", timeout=60)

        with step("the fish configuration is loaded and stays read-only"):
            client.send("printf 'CONFIG=%s\\n' $GOBLIN_FISH_CONFIG; config_probe; "
                        "echo forbidden > $HOME/.config/fish/new-file; printf 'CONFIG_RO=%s\\n' $status\n")
            client.expect(r"(?:^|\n)CONFIG=loaded\n")
            client.expect(r"(?:^|\n)config-function-loaded\n")
            client.expect(r"(?:^|\n)CONFIG_RO=1\n")
            self.assertFalse((fish / "new-file").exists())

        with step("a startup package whose closure holds a symlink output works"):
            client.send("goblins-symlink-output-probe; printf 'SYMLINK_OUTPUT=%s\\n' $status\n")
            client.expect(r"(?:^|\n)SYMLINK_OUTPUT=0\n")

        with step("only the selected files are visible"):
            client.send(f"test -e {targets}/unselected-secret; printf 'STORE_SIBLING_HIDDEN=%s\\n' $status; "
                        "test -e $HOME/unselected-secret; printf 'HOME_PRIVATE=%s\\n' $status\n")
            client.expect(r"(?:^|\n)STORE_SIBLING_HIDDEN=1\n")
            client.expect(r"(?:^|\n)HOME_PRIVATE=1\n")

        with step("read-only and read-write directories and files"):
            for name in ("readonly/data", "ro-file", "writable/data", "rw-file"):
                path = shlex.quote(str(granted / name))
                client.send(f"printf 'READ=%s\\n' (cat {path}); echo updated > {path}; "
                            "printf 'WRITE=%s\\n' $status\n")
                client.expect(r"(?:^|\n)READ=initial\n")
                readonly = name in ("readonly/data", "ro-file")
                client.expect(r"(?:^|\n)WRITE=" + ("1" if readonly else "0") + r"\n")
                self.assertEqual((granted / name).read_text(), "initial" if readonly else "updated\n")

        with step("host edits show at once; a link to a secret and the server's state stay hidden"):
            (granted / "readonly/data").write_text("host-update")
            client.send(f"printf 'HOST_CHANGE=%s\\n' (cat {granted}/readonly/data); "
                        f"test -e {granted}/readonly/host-link; printf 'HOST_LINK_HIDDEN=%s\\n' $status; "
                        f"test -e {d.state}/host.sock; printf 'CONTROL_HIDDEN=%s\\n' $status\n")
            client.expect(r"(?:^|\n)HOST_CHANGE=host-update\n")
            client.expect(r"(?:^|\n)HOST_LINK_HIDDEN=1\n")
            client.expect(r"(?:^|\n)CONTROL_HIDDEN=1\n")
            client.send("exit\n")
            self.assertEqual(client.wait(), 0)

        with step("a configured bind that leads to the server's state refuses the launch"):
            (granted / "readonly").rename(granted / "previous-readonly")
            (granted / "readonly").symlink_to(d.state)
            refused = Terminal([str(app), "--state-dir", str(d.state), "run", "shell"])
            self.addCleanup(refused.close)
            refused.expect("overlaps protected path")
            self.assertNotEqual(refused.wait(), 0)


if __name__ == "__main__":
    unittest.main()
