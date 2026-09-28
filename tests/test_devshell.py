"""Host-launched flake dev shells (ADR 0007) through the real CLI and sandbox."""
import json
from pathlib import Path
import tempfile
import time
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

    def test_launch_rejects_inputs_missing_from_the_lock(self):
        # Nix would silently fetch and use an input the lock does not mention.
        flake = (self.project / "flake.nix").read_text().replace(
            "inputs.nixpkgs.url",
            f'inputs.extra = {{ url = "path:{self.nixpkgs}"; flake = false; }};\n  inputs.nixpkgs.url')
        (self.project / "flake.nix").write_text(flake)
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn("does not lock everything flake.nix uses", record["detail"])

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


class DevShellRefreshTests(DevShellTests):
    """Refresh through the in-sandbox CLI with host approval (ADR 0007)."""

    # Inherited launch tests already ran in DevShellTests.
    test_launch_exports_environment_and_children_inherit_the_snapshot = None
    test_children_cannot_choose_a_dev_shell = None
    test_launch_requires_a_lock_file = None
    test_evaluation_cannot_reach_host_files_outside_the_flake = None
    test_launch_rejects_inputs_missing_from_the_lock = None

    def setUp(self):
        super().setUp()
        self.counter = 0
        self.terminal = self.run_cli("--dev-shell", ".", "--name", "dev")
        self.terminal.expect(r"(?:^|\n)hook-ran in ")
        self.session = next(r for r in self.d.rpc.call("sessions.list", {}) if r["agent_name"] == "dev")["id"]

    def send(self, terminal, script):
        """Start a command; return the markers that end() waits for."""
        self.counter += 1
        terminal.send(f"printf 'BEGIN%s\\n' {self.counter}; {script}; printf '\\nEND{self.counter}=%s\\n' $status\n")
        return self.counter

    def end(self, terminal, n, timeout=120):
        match = terminal.expect(rf"BEGIN{n}\n([\s\S]*?)\nEND{n}=(\d+)\n", timeout=timeout)
        return match.group(1).decode(), int(match.group(2))

    def run_in(self, script, terminal=None, timeout=120):
        terminal = terminal or self.terminal
        return self.end(terminal, self.send(terminal, script), timeout)

    def pending_refresh(self, session=None):
        session = session or self.session

        def ready():
            # Evaluation takes a while; poll slowly on fresh connections so a
            # long wait stays within the per-connection call limit.
            time.sleep(.25)
            rpc = self.d.host()
            try:
                return next((r for r in rpc.call("permissions.list", {"session": session})
                             if r["kind"] == "devshell" and r["state"] == "pending"
                             and r.get("preview")), None)
            finally:
                rpc.close()
        return self.d.wait(ready, timeout=180)

    def test_approved_refresh_enters_the_edited_flake_and_keeps_running_environment(self):
        self.write_flake("edited")
        n = self.send(self.terminal, "goblins devshell refresh --reason 'pick up flake.nix'")
        record = self.pending_refresh()
        description = record["preview"]["description"]
        self.assertIn("generation 1 → 2", description)
        self.assertRegex(description, r"Source +flake.nix \+1 −1")
        self.assertIn("Attention nothing unusual", description)
        self.assertIn("~ DEV_MARKER: original → edited", description)
        self.assertIn('-        DEV_MARKER = "original";', description)
        self.assertRegex(description, r"Inputs +unchanged")
        self.assertNotIn("flake.lock", description)
        self.assertEqual(record["package"], "refresh")
        self.d.decide(record, True)
        output, code = self.end(self.terminal, n)
        self.assertEqual(code, 0, output)
        self.assertIn("hook-ran in", output)
        self.assertIn("Dev shell generation 2 is active", output)
        # The running shell keeps its environment until the agent sources env.sh.
        output, _ = self.run_in("printf 'M=%s\\n' $DEV_MARKER; bash -c '. /run/goblins/devshell/env.sh >/dev/null; printf \"N=%s\\n\" $DEV_MARKER'")
        self.assertIn("M=original", output)
        self.assertIn("N=edited", output)
        # New children start in the new generation.
        child = self.run_cli("--parent", "dev")
        child.expect(r"(?:^|\n)hook-ran in ")
        output, _ = self.run_in("printf 'C=%s\\n' $DEV_MARKER", terminal=child)
        self.assertIn("C=edited", output)

    def test_child_refresh_to_an_inherited_generation_needs_no_approval(self):
        child = self.run_cli("--parent", "dev", "--name", "kid")
        child.expect(r"(?:^|\n)hook-ran in ")
        output, code = self.run_in("goblins devshell refresh", terminal=child)
        self.assertEqual(code, 0, output)
        self.assertIn("Dev shell generation 2 is active", output)
        records = [r for r in self.d.permissions() if r["kind"] == "devshell"]
        self.assertEqual([r["state"] for r in records], ["ready"])
        self.assertEqual(records[0]["approved"], True)

    def test_hand_edited_lock_is_rejected_until_restored(self):
        lock = self.project / "flake.lock"
        original = lock.read_text()
        # Formatting alone is not a change.
        lock.write_text(json.dumps(json.loads(original)) + "\n")
        edited = json.loads(original)
        edited["nodes"]["nixpkgs"]["locked"]["narHash"] = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        lock.write_text(json.dumps(edited, indent=2))
        output, code = self.run_in("goblins devshell refresh")
        self.assertEqual(code, 1, output)
        self.assertIn("flake.lock differs from the trusted lock", output)
        self.assertIn("nodes.nixpkgs.locked.narHash", output)
        self.assertIn("goblins devshell restore-lock", output)
        self.assertEqual([r["state"] for r in self.d.permissions() if r["kind"] == "devshell"], ["failed"])
        output, code = self.run_in("goblins devshell restore-lock")
        self.assertEqual(code, 0, output)
        self.assertEqual(json.loads(lock.read_text()), json.loads(original))
        # The restored lock is the current generation, so no approval is needed.
        output, code = self.run_in("goblins devshell refresh")
        self.assertEqual(code, 0, output)
        self.assertEqual([r["state"] for r in self.d.permissions() if r["kind"] == "devshell"],
                         ["failed", "ready"])

    def test_lock_adds_new_inputs_and_writes_the_approved_lock(self):
        flake = (self.project / "flake.nix").read_text().replace(
            "inputs.nixpkgs.url",
            f'inputs.extra = {{ url = "path:{self.nixpkgs}"; flake = false; }};\n  inputs.nixpkgs.url')
        (self.project / "flake.nix").write_text(flake)
        # Without --lock, an incomplete lock is an error, not an update.
        output, code = self.run_in("goblins devshell refresh")
        self.assertEqual(code, 1, output)
        self.assertIn("does not lock everything flake.nix uses", output)
        self.assertIn("goblins devshell refresh --lock", output)
        n = self.send(self.terminal, "goblins devshell refresh --lock")
        record = self.pending_refresh()
        self.assertEqual(record["package"], "refresh --lock")
        description = record["preview"]["description"]
        self.assertIn(f"! new input 'extra' from path:{self.nixpkgs}", description)
        self.assertIn(f"+ extra  path:{self.nixpkgs}", description)
        self.assertNotIn('"nodes"', description)
        self.assertNotIn("extra", json.loads((self.project / "flake.lock").read_text())["nodes"])
        self.d.decide(record, True)
        output, code = self.end(self.terminal, n)
        self.assertEqual(code, 0, output)
        self.assertIn("flake.lock updated to the approved lock", output)
        self.assertIn("extra", json.loads((self.project / "flake.lock").read_text())["nodes"])

    def test_denied_refresh_changes_nothing(self):
        self.write_flake("edited")
        n = self.send(self.terminal, "goblins devshell refresh")
        self.d.decide(self.pending_refresh(), False)
        self.assertEqual(self.end(self.terminal, n)[1], 1)
        output, _ = self.run_in("bash -c '. /run/goblins/devshell/env.sh >/dev/null; printf \"N=%s\\n\" $DEV_MARKER'")
        self.assertIn("N=original", output)


if __name__ == "__main__":
    unittest.main()
