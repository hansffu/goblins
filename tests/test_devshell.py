"""Host-launched flake dev shells (ADR 0007) through the real CLI and sandbox."""
from pathlib import Path
import tempfile
import unittest
import uuid

from daemon_support import Daemon, ROOT
from support import command
from terminal_support import Terminal

FLAKE = """{
  inputs.nixpkgs.url = "path:NIXPKGS";
  outputs = { nixpkgs, ... }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {
      devShells.x86_64-linux.default = pkgs.mkShellNoCC {
        packages = [ pkgs.hello ];
        DEV_MARKER = "@MARKER@";
        shellHook = "echo hook-ran";
      };
    };
}
"""


class DevShellTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.nixpkgs = command(["nix", "eval", "--impure", "--raw", "--expr",
                               f'(builtins.getFlake "path:{ROOT}").inputs.nixpkgs.outPath'])

    def setUp(self):
        root = tempfile.TemporaryDirectory(prefix="goblin-devshell-")
        self.addCleanup(root.cleanup)
        self.project = Path(root.name)
        self.write_flake("original")
        command(["nix", "flake", "lock", f"path:{self.project}"])
        # A Git flake is the common case: only tracked files reach the store.
        command(["git", "-C", str(self.project), "init", "-q"])
        command(["git", "-C", str(self.project), "add", "flake.nix", "flake.lock"])
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def write_flake(self, marker):
        (self.project / "flake.nix").write_text(
            FLAKE.replace("NIXPKGS", self.nixpkgs).replace("@MARKER@", marker))

    def run_cli(self, *args):
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "run", "shell", *args],
                            cwd=self.project)
        self.addCleanup(terminal.close)
        return terminal

    def test_launch_exports_environment_and_children_inherit_the_snapshot(self):
        terminal = self.run_cli("--dev-shell", ".", "--name", "dev")
        terminal.send("printf 'M=%s\\n' $DEV_MARKER; hello; printf 'R=%s\\n' $GOBLINS_DEV_SHELL\n")
        terminal.expect(r"(?:^|\n)M=original\n")
        terminal.expect(r"(?:^|\n)Hello, world!\n")
        terminal.expect(rf"(?:^|\n)R={self.project}\n")
        terminal.send("bash -c '. /run/goblins/devshell/env.sh; printf \"S=%s\\n\" \"$DEV_MARKER\"'\n")
        terminal.expect(r"(?:^|\n)hook-ran\n")
        terminal.expect(r"(?:^|\n)S=original\n")
        record = next(r for r in self.d.rpc.call("sessions.list", {}) if r["agent_name"] == "dev")
        self.assertEqual(record["dev_shell"], str(self.project))

        # Later workspace edits never reach the running generation or children.
        self.write_flake("edited")
        child = self.run_cli("--parent", "dev")
        child.send("printf 'M=%s\\n' $DEV_MARKER\n")
        child.expect(r"(?:^|\n)M=original\n")

    def test_children_cannot_choose_a_dev_shell(self):
        parent = self.d.start()
        with self.assertRaisesRegex(Exception, "inherit their parent's dev shell"):
            self.d.rpc.call("sessions.start", {
                "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
                "parent": parent["session"], "dev_shell": str(self.project), "rows": 24, "cols": 80})

    def test_launch_requires_a_lock_file(self):
        command(["git", "-C", str(self.project), "rm", "-qf", "flake.lock"])
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn("flake.lock", record["detail"])


if __name__ == "__main__":
    unittest.main()
