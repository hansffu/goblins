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
        # Runs inside the sandbox on entry; its exports reach the entry program.
        shellHook = ''
          echo "hook-ran in $PWD"
          export HOOK_VAR="from-hook-$DEV_MARKER"
        '';
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
        terminal.expect(rf"(?:^|\n)hook-ran in {self.project}\n")
        terminal.send("printf 'M=%s H=%s\\n' $DEV_MARKER $HOOK_VAR; hello; printf 'R=%s\\n' $GOBLINS_DEV_SHELL\n")
        terminal.expect(r"(?:^|\n)M=original H=from-hook-original\n")
        terminal.expect(r"(?:^|\n)Hello, world!\n")
        terminal.expect(rf"(?:^|\n)R={self.project}\n")
        terminal.send("bash -c '. /run/goblins/devshell/env.sh; printf \"S=%s\\n\" \"$DEV_MARKER\"'\n")
        terminal.expect(r"(?:^|\n)hook-ran in ")
        terminal.expect(r"(?:^|\n)S=original\n")
        record = next(r for r in self.d.rpc.call("sessions.list", {}) if r["agent_name"] == "dev")
        self.assertEqual(record["dev_shell"], str(self.project))

        # Later workspace edits never reach the running generation or children.
        self.write_flake("edited")
        child = self.run_cli("--parent", "dev")
        child.expect(r"(?:^|\n)hook-ran in ")
        child.send("printf 'M=%s H=%s\\n' $DEV_MARKER $HOOK_VAR\n")
        child.expect(r"(?:^|\n)M=original H=from-hook-original\n")

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

    def test_evaluation_cannot_reach_host_files_outside_the_flake(self):
        # Host Nix would read this repository; the evaluator sandbox cannot see it.
        outside = tempfile.TemporaryDirectory(prefix="goblin-devshell-outside-")
        self.addCleanup(outside.cleanup)
        repo = Path(outside.name)
        (repo / "token").write_text("host-secret\n")
        for args in (["init", "-q"], ["add", "token"],
                     ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "secret"]):
            command(["git", "-C", str(repo), *args])
        rev = command(["git", "-C", str(repo), "rev-parse", "HEAD"])
        flake = (self.project / "flake.nix").read_text().replace(
            'DEV_MARKER = "original";',
            f'DEV_MARKER = builtins.readFile "${{builtins.fetchGit {{ url = "file://{repo}"; rev = "{rev}"; }}}}/token";')
        (self.project / "flake.nix").write_text(flake)
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn(f'"{repo}" does not exist', record["detail"])


if __name__ == "__main__":
    unittest.main()
