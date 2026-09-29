"""Host-launched flake dev shells (ADR 0007) through the real CLI and sandbox."""
import json
import os
import re
from pathlib import Path
import tempfile
import time
import unittest
import uuid

from daemon_support import Daemon, ROOT
from support import command
from terminal_support import Terminal
# A module import, so its DockerTests are not collected here as well.
import test_docker

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
            "dev_shell": str(self.project), "cwd": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn("flake.lock", record["detail"])

    def test_launch_requires_a_working_directory(self):
        # Without one the sandbox sees no host files, so evaluation may see none.
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn("needs a host working directory", record["detail"])

    def test_launch_rejects_inputs_missing_from_the_lock(self):
        # Nix would silently fetch and use an input the lock does not mention.
        flake = (self.project / "flake.nix").read_text().replace(
            "inputs.nixpkgs.url",
            f'inputs.extra = {{ url = "path:{self.nixpkgs}"; flake = false; }};\n  inputs.nixpkgs.url')
        (self.project / "flake.nix").write_text(flake)
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "cwd": str(self.project), "rows": 24, "cols": 80})
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
            "dev_shell": str(self.project), "cwd": str(self.project), "rows": 24, "cols": 80})
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
    test_launch_requires_a_working_directory = None
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
        # A tracked non-Nix file is part of the source diff too.
        (self.project / "notes.txt").write_text("new notes\n")
        command(["git", "-C", str(self.project), "add", "notes.txt"])
        n = self.send(self.terminal, "goblins devshell refresh --reason 'pick up flake.nix'")
        record = self.pending_refresh()
        # The daemon sends facts; frontends assemble sections and colors.
        preview = record["preview"]
        self.assertEqual((preview["kind"], preview["from"], preview["to"]), ("devshell", 1, 2))
        self.assertNotIn("description", preview)
        self.assertEqual(preview["files"], [{"path": "flake.nix", "added": 1, "removed": 1},
                                            {"path": "notes.txt", "added": 1, "removed": 0}])
        self.assertEqual(preview["attention"], [])
        self.assertEqual(preview["inputs"], [])
        self.assertIn({"name": "DEV_MARKER", "change": "changed", "old": "original", "new": "edited",
                       "long": False}, preview["env"])
        self.assertIn('-        DEV_MARKER = "original";', preview["diff"])
        self.assertIn("diff --git a/notes.txt b/notes.txt", preview["diff"])
        self.assertNotIn("flake.lock", preview["diff"])
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
        preview = record["preview"]
        self.assertEqual(preview["attention"],
                         [{"kind": "new-input", "input": "extra", "source": f"path:{self.nixpkgs}"}])
        self.assertEqual([(c["name"], c["change"], c["new"]["source"]) for c in preview["inputs"]],
                         [("extra", "added", f"path:{self.nixpkgs}")])
        self.assertNotIn('"nodes"', preview["diff"])
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

    def write_endless_flake(self):
        # 10^10 additions: evaluation effectively never ends.
        flake = (self.project / "flake.nix").read_text().replace(
            'DEV_MARKER = "original";',
            "DEV_MARKER = toString (builtins.foldl' (a: _: builtins.foldl' (b: _: b + 1) a "
            "(builtins.genList (x: x) 100000)) 0 (builtins.genList (x: x) 100000));")
        (self.project / "flake.nix").write_text(flake)

    def test_refresh_cannot_be_approved_before_its_preview(self):
        self.write_endless_flake()
        n = self.send(self.terminal, "goblins devshell refresh")
        record = self.d.wait(lambda: next((r for r in self.d.permissions(session=self.session)
                                           if r["kind"] == "devshell" and r["state"] == "pending"), None),
                             timeout=60)
        self.assertIsNone(record.get("preview"))
        # The preview is the approver's whole basis; nothing to approve yet.
        with self.assertRaises(ValueError) as caught:
            self.d.decide(record, True)
        self.assertIn("still being evaluated; decide once its preview is shown", str(caught.exception))
        self.assertEqual([r["state"] for r in self.d.permissions() if r["kind"] == "devshell"], ["pending"])
        # Denial still cancels the evaluation.
        self.d.decide(record, False)
        output, code = self.end(self.terminal, n, timeout=60)
        self.assertEqual(code, 1, output)
        self.assertEqual([r["state"] for r in self.d.permissions() if r["kind"] == "devshell"], ["denied"])

    def test_refresh_evaluation_times_out(self):
        d = Daemon(env={**os.environ, "GOBLINS_EVALUATION_TIMEOUT": "5"})
        self.addCleanup(d.close)
        terminal = Terminal([str(d.app), "--state-dir", str(d.state), "run", "shell",
                             "--dev-shell", ".", "--name", "slow"], cwd=self.project)
        self.addCleanup(terminal.close)
        terminal.expect(r"(?:^|\n)hook-ran in ")
        self.write_endless_flake()
        start = time.monotonic()
        output, code = self.run_in("goblins devshell refresh", terminal=terminal)
        self.assertLess(time.monotonic() - start, 60)
        self.assertEqual(code, 1, output)
        self.assertIn("evaluation timed out after 5 s", output)
        self.assertEqual([r["state"] for r in d.permissions() if r["kind"] == "devshell"], ["failed"])

    def roots(self):
        return sorted(p.name for p in (self.d.state / self.session / "resources/roots").iterdir()
                      if p.name.endswith("-source"))

    def test_pending_candidate_source_is_a_gc_root(self):
        before = self.roots()
        self.write_flake("edited")
        n = self.send(self.terminal, "goblins devshell refresh")
        record = self.pending_refresh()
        # The candidate's new source snapshot is rooted while approval waits.
        self.assertEqual(len(self.roots()), len(before) + 1, self.roots())
        self.d.decide(record, True)
        self.assertEqual(self.end(self.terminal, n)[1], 0)

    def test_refresh_never_builds_import_from_derivation_before_approval(self):
        name = f"goblins-ifd-{uuid.uuid4().hex[:12]}"
        # Its output path, computed without building it.
        out = command(["nix", "eval", "--impure", "--raw", "--expr",
                       f'let pkgs = import {self.nixpkgs} {{ system = "x86_64-linux"; }}; in '
                       f'(pkgs.runCommand "{name}" {{}} "echo built > $out").outPath'])
        flake = (self.project / "flake.nix").read_text().replace(
            'DEV_MARKER = "original";',
            f'DEV_MARKER = builtins.readFile (pkgs.runCommand "{name}" {{}} "echo built > $out");')
        (self.project / "flake.nix").write_text(flake)
        output, code = self.run_in("goblins devshell refresh")
        self.assertEqual(code, 1, output)
        self.assertIn("allow-import-from-derivation", output)
        self.assertFalse(Path(out).exists(), out)


class DevShellSubdirectoryTests(DevShellRefreshTests):
    """A flake in a subdirectory of its Git repository keeps the repository as source."""

    test_approved_refresh_enters_the_edited_flake_and_keeps_running_environment = None
    test_child_refresh_to_an_inherited_generation_needs_no_approval = None
    test_hand_edited_lock_is_rejected_until_restored = None
    test_lock_adds_new_inputs_and_writes_the_approved_lock = None
    test_denied_refresh_changes_nothing = None
    test_pending_candidate_source_is_a_gc_root = None
    test_refresh_never_builds_import_from_derivation_before_approval = None
    test_refresh_evaluation_times_out = None

    def setUp(self):
        root = tempfile.TemporaryDirectory(prefix="goblin-devshell-repo-")
        self.addCleanup(root.cleanup)
        self.repo = Path(root.name)
        self.project = self.repo / "sub"
        self.project.mkdir()
        (self.repo / "marker").write_text("from-parent")
        self.write_flake("unused")
        command(["nix", "flake", "lock", f"path:{self.project}"])
        command(["git", "-C", str(self.repo), "init", "-q"])
        command(["git", "-C", str(self.repo), "add", "marker", "sub/flake.nix", "sub/flake.lock"])
        self.d = Daemon()
        self.addCleanup(self.d.close)
        self.counter = 0
        # Run from the repository root, so its whole tree is visible.
        self.terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "run", "shell",
                                  "--dev-shell", "sub", "--name", "dev"], cwd=self.repo)
        self.addCleanup(self.terminal.close)
        self.terminal.expect(r"(?:^|\n)hook-ran in ")
        self.session = next(r for r in self.d.rpc.call("sessions.list", {}) if r["agent_name"] == "dev")["id"]

    def write_flake(self, marker):
        (self.project / "flake.nix").write_text(
            FLAKE.replace("NIXPKGS", self.nixpkgs)
            .replace('DEV_MARKER = "@MARKER@";', "DEV_MARKER = builtins.readFile ../marker;"))

    def test_subdirectory_flake_uses_files_outside_its_directory(self):
        output, _ = self.run_in("printf 'M=%s\\n' $DEV_MARKER")
        self.assertIn("M=from-parent", output)
        (self.repo / "marker").write_text("changed")
        flake = (self.project / "flake.nix").read_text().replace(
            "inputs.nixpkgs.url",
            f'inputs.extra = {{ url = "path:{self.nixpkgs}"; flake = false; }};\n  inputs.nixpkgs.url')
        (self.project / "flake.nix").write_text(flake)
        n = self.send(self.terminal, "goblins devshell refresh --lock")
        record = self.pending_refresh()
        preview = record["preview"]
        self.assertEqual([f["path"] for f in preview["files"]], ["marker", "sub/flake.nix"])
        self.assertIn({"name": "DEV_MARKER", "change": "changed", "old": "from-parent", "new": "changed",
                       "long": False}, preview["env"])
        self.assertNotIn("flake.lock", preview["diff"])
        self.d.decide(record, True)
        output, code = self.end(self.terminal, n)
        self.assertEqual(code, 0, output)
        self.assertIn("extra", json.loads((self.project / "flake.lock").read_text())["nodes"])
        output, _ = self.run_in("bash -c '. /run/goblins/devshell/env.sh >/dev/null; printf \"N=%s\\n\" $DEV_MARKER'")
        self.assertIn("N=changed", output)

    def test_launch_from_the_flake_directory_cannot_widen_access_to_the_repository(self):
        # The sandbox would see only sub/, but evaluation would see all of repo/.
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": self.d.manifest,
            "dev_shell": str(self.project), "cwd": str(self.project), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"]
                                      not in ("starting", "running", "stopping") else None), timeout=60)
        self.assertEqual(record["state"], "failed", record)
        self.assertIn(f"source tree {self.repo.resolve()} is outside the sandbox's working directory "
                      f"{self.project.resolve()}", record["detail"])


APPS = """      apps.x86_64-linux = {
        default = { type = "app"; program = "${greet}/bin/greet"; };
        greet = { type = "app"; program = "${greet}/bin/greet"; };
        outside = { type = "app"; program = "/bin/sh"; };
        # A store path the app does not build: no string context.
        literal = { type = "app"; program = builtins.unsafeDiscardStringContext "${pkgs.hello}/bin/hello"; };
        leak = { type = "app"; program = "${pkgs.writeShellScriptBin "leak" ''
          echo ${builtins.readFile "${builtins.fetchGit { url = "file://@REPO@"; rev = "@REV@"; }}/token"}
        ''}/bin/leak"; };
      };
      packages.x86_64-linux.tool = pkgs.writeShellScriptBin "tool" "echo tool-ran";
      devShells.x86_64-linux.default"""


class DevShellFlakeRunTests(DevShellRefreshTests):
    """goblins flake run: apps from the trusted generation, with no nix in the sandbox."""

    def setUp(self):
        # A Git repository outside the flake that evaluation must not reach.
        outside = tempfile.TemporaryDirectory(prefix="goblin-devshell-outside-")
        self.addCleanup(outside.cleanup)
        self.outside = Path(outside.name)
        (self.outside / "token").write_text("host-secret")
        for args in (["init", "-q"], ["add", "token"],
                     ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "secret"]):
            command(["git", "-C", str(self.outside), *args])
        self.rev = command(["git", "-C", str(self.outside), "rev-parse", "HEAD"])
        super().setUp()

    def write_flake(self, marker):
        flake = FLAKE.replace("let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {",
                              "let pkgs = nixpkgs.legacyPackages.x86_64-linux;\n"
                              "        greet = pkgs.writeShellScriptBin \"greet\" ''echo \"greet:@MARKER@:$*\"'';\n"
                              "    in {")
        flake = flake.replace("      devShells.x86_64-linux.default", APPS, 1)
        (self.project / "flake.nix").write_text(
            flake.replace("NIXPKGS", self.nixpkgs).replace("@MARKER@", marker)
            .replace("@REPO@", str(self.outside)).replace("@REV@", self.rev))

    def test_apps_and_packages_of_the_trusted_generation_run_with_arguments(self):
        output, code = self.run_in("goblins flake run .#greet -- a 'b c'", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("greet:original:a b c", output)
        output, code = self.run_in("goblins flake run")
        self.assertEqual(code, 0, output)
        self.assertIn("greet:original:", output)
        # A package runs its main program, as with nix run.
        output, code = self.run_in("goblins flake run '#tool'", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("tool-ran", output)
        # Running a trusted app asks nothing of the host.
        self.assertEqual(self.d.permissions(), [])

    def test_programs_must_be_built_store_outputs(self):
        output, code = self.run_in("goblins flake run '#outside'")
        self.assertEqual(code, 1, output)
        self.assertIn("app program /bin/sh is not a path in the store", output)
        output, code = self.run_in("goblins flake run '#literal'")
        self.assertEqual(code, 1, output)
        self.assertIn("does not come from a derivation or store path", output)
        output, code = self.run_in("goblins flake run '#missing'")
        self.assertEqual(code, 1, output)
        self.assertIn("has no apps.x86_64-linux.missing or packages.x86_64-linux.missing", output)

    def test_only_the_dev_shell_flake_is_served(self):
        output, code = self.run_in("goblins flake run /tmp#greet")
        self.assertEqual(code, 2, output)
        self.assertIn("serves only this sandbox's dev shell flake", output)
        output, code = self.run_in("goblins flake run github:NixOS/nixpkgs#hello")
        self.assertEqual(code, 2, output)
        self.assertIn("serves only this sandbox's dev shell flake", output)
        output, code = self.run_in(f"goblins flake run {self.project}#greet")
        self.assertEqual(code, 0, output)

    def test_changed_workspace_flake_needs_a_refresh_first(self):
        self.write_flake("edited")
        output, code = self.run_in("goblins flake run '#greet'")
        self.assertEqual(code, 1, output)
        self.assertIn("the workspace flake differs from dev shell generation 1", output)
        self.assertIn("goblins devshell refresh", output)
        n = self.send(self.terminal, "goblins devshell refresh")
        self.d.decide(self.pending_refresh(), True)
        self.assertEqual(self.end(self.terminal, n)[1], 0)
        output, code = self.run_in("goblins flake run '#greet'", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("greet:edited:", output)

    def test_app_evaluation_cannot_reach_host_files_outside_the_flake(self):
        # The dev shell launched; only the app reads the outside repository.
        output, code = self.run_in("goblins flake run '#leak'")
        self.assertEqual(code, 1, output)
        self.assertIn("could not evaluate app leak", output)
        self.assertNotIn("host-secret", output)


for _name in dir(DevShellRefreshTests):
    if _name.startswith("test_") and _name not in DevShellFlakeRunTests.__dict__:
        setattr(DevShellFlakeRunTests, _name, None)

class DevShellDiffTests(DevShellRefreshTests):
    """goblins devshell diff: a refresh preview with no request and no change."""

    def env_marker(self):
        output, _ = self.run_in("bash -c '. /run/goblins/devshell/env.sh >/dev/null; printf \"N=%s\\n\" $DEV_MARKER'")
        return output

    def test_diff_shows_the_edited_flake_and_changes_nothing(self):
        output, code = self.run_in("goblins devshell diff", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("No changes: the workspace flake matches dev shell generation 1.", output)
        roots = self.roots()
        self.write_flake("edited")
        output, code = self.run_in("goblins devshell diff", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("generation 1 → 2 (preview only; nothing was requested or changed)", output)
        self.assertIn("ATTENTION nothing unusual", output)
        self.assertIn("SOURCE    flake.nix +1 -1", output)
        self.assertIn("ENV       [C] DEV_MARKER original → edited", output)
        self.assertIn("COST      ", output)
        self.assertIn("modified   flake.nix", output)
        self.assertIn('-        DEV_MARKER = "original";', output)
        self.assertIn('+        DEV_MARKER = "edited";', output)
        # No request, no rooted candidate, no new generation.
        self.assertEqual(self.d.permissions(), [])
        self.assertEqual(self.roots(), roots)
        self.assertIn("N=original", self.env_marker())

    def test_diff_rejects_a_hand_edited_lock(self):
        lock = self.project / "flake.lock"
        edited = json.loads(lock.read_text())
        edited["nodes"]["nixpkgs"]["locked"]["narHash"] = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        lock.write_text(json.dumps(edited, indent=2))
        output, code = self.run_in("goblins devshell diff")
        self.assertEqual(code, 1, output)
        self.assertIn("flake.lock differs from the trusted lock", output)
        self.assertIn("goblins devshell restore-lock", output)
        self.assertEqual(self.d.permissions(), [])
        self.assertEqual(json.loads(lock.read_text()), edited)

    def test_diff_lock_shows_new_inputs_without_writing_the_lock(self):
        lock = (self.project / "flake.lock").read_text()
        flake = (self.project / "flake.nix").read_text().replace(
            "inputs.nixpkgs.url",
            f'inputs.extra = {{ url = "path:{self.nixpkgs}"; flake = false; }};\n  inputs.nixpkgs.url')
        (self.project / "flake.nix").write_text(flake)
        output, code = self.run_in("goblins devshell diff")
        self.assertEqual(code, 1, output)
        self.assertIn("does not lock everything flake.nix uses", output)
        output, code = self.run_in("goblins devshell diff --lock", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn(f"ATTENTION ! new input 'extra' from path:{self.nixpkgs}", output)
        self.assertIn(f"INPUTS    [A] extra path:{self.nixpkgs}", output)
        self.assertEqual((self.project / "flake.lock").read_text(), lock)
        self.assertEqual(self.d.permissions(), [])

    def test_diff_while_a_refresh_is_pending_leaves_it_alone(self):
        self.write_flake("edited")
        output, code = self.run_in("goblins devshell refresh >/tmp/refresh.log 2>&1 &")
        self.assertEqual(code, 0, output)
        record = self.pending_refresh()
        roots = self.roots()
        # The workspace moves on; the pending candidate must not.
        self.write_flake("later")
        output, code = self.run_in("goblins devshell diff", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("ENV       [C] DEV_MARKER original → later", output)
        self.assertEqual(self.roots(), roots)
        self.assertEqual([r["state"] for r in self.d.permissions()], ["pending"])
        self.d.decide(record, True)
        output, code = self.run_in("wait; cat /tmp/refresh.log")
        self.assertIn("Dev shell generation 2 is active", output)
        # The approved candidate is the one the host saw, not the later edit.
        self.assertIn("N=edited", self.env_marker())


for _name in dir(DevShellRefreshTests):
    if _name.startswith("test_") and _name not in DevShellDiffTests.__dict__:
        setattr(DevShellDiffTests, _name, None)


DOCKER_FLAKE = """{
  inputs.nixpkgs.url = "path:NIXPKGS";
  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      # Programs report their own path, so containers can run the same file.
      added = pkgs.writeShellScriptBin "added" ''echo "added:@MARKER@:$0"'';
      greet = pkgs.writeShellScriptBin "greet" ''echo "greet:@MARKER@:$0"'';
    in {
      apps.x86_64-linux.greet = { type = "app"; program = "${greet}/bin/greet"; };
      devShells.x86_64-linux.default = pkgs.mkShellNoCC {
        packages = [ pkgs.hello ] ++ pkgs.lib.optional @ADDED@ added;
        # The dev shell environment replaces the goblin's PS1.
        shellHook = ''echo "hook-ran in $PWD"; export PS1="docker-test> "'';
      };
    };
}
"""


class DevShellDockerTests(test_docker.DockerTests):
    """Store paths a dev shell mounts after Docker is attached reach the engine."""

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        DevShellTests.setUpClass.__func__(cls)

    def setUp(self):
        super().setUp()
        self.marker = uuid.uuid4().hex[:12]
        self.project = self.root / "project"
        self.project.mkdir()
        self.write_flake(added=False)
        command(["nix", "flake", "lock", f"path:{self.project}"])
        command(["git", "-C", str(self.project), "init", "-q"])
        command(["git", "-C", str(self.project), "add", "flake.nix", "flake.lock"])
        params = {"name": "plain", "configuration": self.d.manifest, "key": uuid.uuid4().hex,
                  "rows": 24, "cols": 100, "cwd": str(self.project), "dev_shell": str(self.project)}
        self.session = self.d.rpc.call("sessions.start", params)["session"]
        self.terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", self.session])
        self.addCleanup(self.terminal.close)
        try:
            self.terminal.expect("docker-test>", timeout=120)
        except AssertionError as error:
            raise AssertionError(self.d.get(self.session).get("detail") or str(error)) from error
        self.enable(self.session, self.terminal)
        self.load(self.terminal)

    def write_flake(self, added):
        (self.project / "flake.nix").write_text(
            DOCKER_FLAKE.replace("NIXPKGS", self.nixpkgs).replace("@MARKER@", self.marker)
            .replace("@ADDED@", "true" if added else "false"))

    def output(self, text, timeout=300):
        self.terminal.send(text + "; printf '\\nDOCKER_TEST_STATUS=%s\\n' \"$?\"\n")
        result = self.terminal.expect(r"([\s\S]*?)\nDOCKER_TEST_STATUS=(\d+)\n", timeout=timeout)
        output = result[1].decode(errors="replace")
        self.assertEqual(result[2], b"0", output)
        self.terminal.expect("docker-test>")
        return output

    def program(self, output, name):
        match = re.search(rf"{name}:{self.marker}:(/nix/store/\S+)", output)
        self.assertIsNotNone(match, output)
        return match[1]

    def test_engine_sees_a_refreshed_dev_shell(self):
        self.write_flake(added=True)
        self.terminal.send("goblins devshell refresh --reason 'add a tool'\n")
        record = self.d.wait(lambda: next((r for r in self.d.permissions(session=self.session, state="pending")
                                           if r["kind"] == "devshell" and r.get("preview")), None), timeout=180)
        self.d.decide(record, True)
        self.terminal.expect("Dev shell generation 2 is active", timeout=300)
        self.terminal.expect("docker-test>")
        program = self.program(self.output("bash -c '. /run/goblins/devshell/env.sh >/dev/null; added'"), "added")
        output = self.output(f"docker run --rm -v /nix/store:/nix/store:ro goblins-test {program}")
        self.assertIn(f"added:{self.marker}:{program}", output)

    def test_engine_sees_a_flake_app(self):
        program = self.program(self.output("goblins flake run .#greet"), "greet")
        output = self.output(f"docker run --rm -v /nix/store:/nix/store:ro goblins-test {program}")
        self.assertIn(f"greet:{self.marker}:{program}", output)


for _name in dir(test_docker.DockerTests):
    if _name.startswith("test_") and _name not in DevShellDockerTests.__dict__:
        setattr(DevShellDockerTests, _name, None)

if __name__ == "__main__":
    unittest.main()
