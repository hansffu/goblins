"""Working in flake dev shells (ADR 0007), as a person and an agent do."""
import json
import os
import re
from pathlib import Path
import tempfile
import time
import unittest
import uuid

from build_support import fixture_path
from daemon_support import Daemon
from support import FLAKE, command, lock_flake, pinned_nixpkgs, step
from terminal_support import Terminal
# A module import, so its workflows are not collected here as well.
import test_docker

TEMPLATE = """{
  inputs.nixpkgs.url = "NIXPKGS";
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


# A flake in a subdirectory of its repository: its dev shell and its app read
# a file beside it, so the whole repository is the flake's source.
WORKFLOW_FLAKE = """{
  inputs.nixpkgs.url = "NIXPKGS";
  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      marker = builtins.readFile ../marker;
      greet = pkgs.writeShellScriptBin "greet" ''echo "greet:${marker}:$*"'';
    in {
      apps.x86_64-linux.default = { type = "app"; program = "${greet}/bin/greet"; };
      packages.x86_64-linux.tool = pkgs.writeShellScriptBin "tool" "echo tool-ran";
      devShells.x86_64-linux.default = pkgs.mkShellNoCC {
        packages = [ pkgs.hello ];
        DEV_MARKER = marker;
        # Runs inside the sandbox on entry; its exports reach the entry program.
        shellHook = ''
          echo "hook-ran in $PWD"
          export HOOK_VAR="from-hook-$DEV_MARKER"
        '';
      };
    };
}
"""


class DevShellWorkflow(unittest.TestCase):
    """Helpers for driving a dev shell goblin through its terminal."""

    @classmethod
    def setUpClass(cls):
        # Test flakes name this repository's own nixpkgs as a locked github:
        # input (see lock_flake); the store path serves direct imports.
        cls.nixpkgs_url = pinned_nixpkgs()[0]
        cls.nixpkgs = command(["nix", "eval", "--impure", "--raw", "--expr",
                               f'(builtins.getFlake "{FLAKE}").inputs.nixpkgs.outPath'])
        # A small input for steps that add one: a path: input is hashed whole.
        cls.tools = fixture_path(FLAKE + "#checks.x86_64-linux.static-bash^out")

    def setUp(self):
        self.counter = 0
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def git(self, repo, *args):
        return command(["git", "-C", str(repo), *args])

    def run_cli(self, cwd, *args):
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "run", "shell", *args], cwd=cwd)
        self.addCleanup(terminal.close)
        return terminal

    def session(self, agent_name):
        return next(r for r in self.d.rpc.call("sessions.list", {}) if r["agent_name"] == agent_name)

    def run_member(self, owner):
        """A goblin the raid owner `owner` (an agent name) runs, attached
        through the host CLI; it shares the owner's dev shell and directory."""
        launch = self.d.member(self.session(owner)["id"])
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", launch["session"]])
        self.addCleanup(terminal.close)
        terminal.expect(r"(?:^|\n)hook-ran in ")
        return launch["session"], terminal

    def send(self, terminal, script):
        """Start a command; return the markers that end() waits for."""
        self.counter += 1
        terminal.send(f"printf 'BEGIN%s\\n' {self.counter}; {script}; printf '\\nEND{self.counter}=%s\\n' $status\n")
        return self.counter

    def end(self, terminal, n, timeout=120):
        match = terminal.expect(rf"BEGIN{n}\n([\s\S]*?)\nEND{n}=(\d+)\n", timeout=timeout)
        return match.group(1).decode(), int(match.group(2))

    def run_in(self, terminal, script, timeout=120):
        return self.end(terminal, self.send(terminal, script), timeout)

    def entered_marker(self, terminal):
        """DEV_MARKER as a shell that enters the current generation sees it."""
        output, _ = self.run_in(terminal, "bash -c '. /run/goblins/devshell/env.sh >/dev/null; printf \"N=%s\\n\" $DEV_MARKER'")
        return re.search(r"N=(\S*)", output)[1]

    def pending_refresh(self, session):
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

    def refreshes(self):
        return [r for r in self.d.permissions() if r["kind"] == "devshell"]


class DevShellRefreshWorkflow(DevShellWorkflow):
    """A person works in a flake's dev shell, edits the flake and refreshes it."""

    def test_work_in_a_dev_shell_edit_the_flake_and_refresh(self):
        root = tempfile.TemporaryDirectory(prefix="goblin-devshell-repo-")
        self.addCleanup(root.cleanup)
        repo = Path(root.name)
        project = repo / "sub"
        project.mkdir()
        marker = repo / "marker"
        marker.write_text("original")
        flake = project / "flake.nix"
        flake.write_text(WORKFLOW_FLAKE.replace("NIXPKGS", self.nixpkgs_url))
        lock_flake(project)
        lock = project / "flake.lock"
        # A Git flake is the common case: only tracked files reach the store.
        self.git(repo, "init", "-q")
        self.git(repo, "add", "marker", "sub/flake.nix", "sub/flake.lock")

        with step("launch from the repository root with the dev shell of sub/"):
            shell = self.run_cli(repo, "--dev-shell", "sub", "--name", "dev")
            shell.expect(rf"(?:^|\n)hook-ran in {repo}\n")
            output, code = self.run_in(shell, "printf 'M=%s H=%s R=%s\\n' $DEV_MARKER $HOOK_VAR $GOBLINS_DEV_SHELL; hello")
            self.assertEqual(code, 0, output)
            self.assertIn(f"M=original H=from-hook-original R={project}", output)
            self.assertIn("Hello, world!", output)
            record = self.session("dev")
            self.assertEqual((record["dev_shell"], record["cwd"]), (str(project), str(repo)))
            # Another shell in the sandbox enters the same environment.
            self.assertEqual(self.entered_marker(shell), "original")

        with step("run the flake's app and package"):
            output, code = self.run_in(shell, "goblin flake run '#default' -- a 'b c'", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("greet:original:a b c", output)
            output, code = self.run_in(shell, "goblin flake run '#tool'", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("tool-ran", output)
            # Running what the host already trusts asks nothing of it.
            self.assertEqual(self.d.permissions(), [])

        with step("diff an unchanged flake"):
            output, code = self.run_in(shell, "goblin devshell diff", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("No changes: the workspace flake matches dev shell generation 1.", output)

        with step("edit the flake's source: running goblins and new members keep the trusted generation"):
            marker.write_text("edited")
            _, early_member = self.run_member("dev")
            output, _ = self.run_in(early_member, "printf 'M=%s\\n' $DEV_MARKER")
            self.assertIn("M=original", output)
            output, code = self.run_in(shell, "goblin flake run '#default'")
            self.assertEqual(code, 1, output)
            self.assertIn("the workspace flake differs from dev shell generation 1", output)
            self.assertIn("goblin devshell refresh", output)

        with step("diff the edit without requesting anything"):
            output, code = self.run_in(shell, "goblin devshell diff", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("generation 1 → 2 (preview only; nothing was requested or changed)", output)
            self.assertIn("SOURCE    marker +1 -1", output)
            self.assertIn("ENV       [C] DEV_MARKER original → edited", output)
            self.assertEqual(self.d.permissions(), [])

        with step("a denied refresh changes nothing"):
            roots = self.d.state / record["id"] / "resources/roots"
            sources = len(list(roots.glob("*-source")))
            n = self.send(shell, "goblin devshell refresh")
            pending = self.pending_refresh(record["id"])
            # A Nix garbage collection while the host decides cannot remove
            # what it is shown: the candidate's source is rooted. This needs
            # real Nix, so it is checked here rather than in a unit test.
            self.assertEqual(len(list(roots.glob("*-source"))), sources + 1)
            self.d.decide(pending, False)
            self.assertEqual(self.end(shell, n)[1], 1)
            self.assertEqual(self.entered_marker(shell), "original")

        with step("approve a refresh while the workspace keeps changing"):
            output, code = self.run_in(shell, "goblin devshell refresh --reason 'pick up marker' >/tmp/refresh.log 2>&1 &")
            self.assertEqual(code, 0, output)
            pending = self.pending_refresh(record["id"])
            self.assertEqual(pending["package"], "refresh")
            self.assertEqual([f["path"] for f in pending["preview"]["files"]], ["marker"])
            self.assertIn({"name": "DEV_MARKER", "change": "changed", "old": "original", "new": "edited",
                           "long": False}, pending["preview"]["env"])
            # The workspace moves on while the host decides. A diff shows the
            # later edit and leaves the pending request and its candidate alone.
            candidates = sorted(roots.glob("*-source"))
            marker.write_text("later")
            output, code = self.run_in(shell, "goblin devshell diff", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("ENV       [C] DEV_MARKER original → later", output)
            self.assertEqual(sorted(roots.glob("*-source")), candidates)
            self.assertEqual([r["state"] for r in self.refreshes()][-1], "pending")
            # The approver decides on the preview, not on the later edit.
            self.d.decide(pending, True)
            output, _ = self.run_in(shell, "wait; cat /tmp/refresh.log")
            self.assertIn("Dev shell generation 2 is active", output)
            # The running shell keeps its environment until it enters the new one.
            output, _ = self.run_in(shell, "printf 'M=%s\\n' $DEV_MARKER")
            self.assertIn("M=original", output)
            self.assertEqual(self.entered_marker(shell), "edited")

        with step("a member started now gets the new generation and refreshes to it without asking"):
            marker.write_text("edited")
            _, member = self.run_member("dev")
            output, _ = self.run_in(member, "printf 'C=%s\\n' $DEV_MARKER")
            self.assertIn("C=edited", output)
            output, code = self.run_in(member, "goblin devshell refresh")
            self.assertEqual(code, 0, output)
            self.assertIn("is active", output)
            self.assertEqual([(r["state"], r["approved"]) for r in self.refreshes()][-1], ("ready", True))
            output, code = self.run_in(shell, "goblin flake run '#default'")
            self.assertEqual(code, 0, output)
            self.assertIn("greet:edited:", output)

        with step("add an input: lock it with an approved refresh"):
            trusted_lock = lock.read_text()
            edited = flake.read_text().replace(
                "inputs.nixpkgs.url",
                f'inputs.extra = {{ url = "path:{self.tools}"; flake = false; }};\n  inputs.nixpkgs.url')
            self.assertNotEqual(edited, flake.read_text())
            flake.write_text(edited)
            output, code = self.run_in(shell, "goblin devshell refresh")
            self.assertEqual(code, 1, output)
            self.assertIn("does not lock everything flake.nix uses", output)
            self.assertIn("goblin devshell refresh --lock", output)
            output, code = self.run_in(shell, "goblin devshell diff --lock", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn(f"ATTENTION ! new input 'extra' from path:{self.tools}", output)
            self.assertIn(f"INPUTS    [A] extra path:{self.tools}", output)
            self.assertEqual(lock.read_text(), trusted_lock)
            n = self.send(shell, "goblin devshell refresh --lock")
            pending = self.pending_refresh(record["id"])
            self.assertEqual(pending["package"], "refresh --lock")
            self.assertEqual(pending["preview"]["attention"],
                             [{"kind": "new-input", "input": "extra", "source": f"path:{self.tools}"}])
            self.assertNotIn("extra", json.loads(lock.read_text())["nodes"])
            self.d.decide(pending, True)
            output, code = self.end(shell, n)
            self.assertEqual(code, 0, output)
            self.assertIn("flake.lock updated to the approved lock", output)
            self.assertIn("extra", json.loads(lock.read_text())["nodes"])

        with step("a hand-edited lock is refused until it is restored"):
            approved_lock = json.loads(lock.read_text())
            tampered = json.loads(lock.read_text())
            tampered["nodes"]["nixpkgs"]["locked"]["narHash"] = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            lock.write_text(json.dumps(tampered, indent=2))
            requests = len(self.refreshes())
            output, code = self.run_in(shell, "goblin devshell refresh")
            self.assertEqual(code, 1, output)
            self.assertIn("flake.lock differs from the trusted lock", output)
            self.assertIn("nodes.nixpkgs.locked.narHash", output)
            self.assertIn("goblin devshell restore-lock", output)
            output, code = self.run_in(shell, "goblin devshell restore-lock")
            self.assertEqual(code, 0, output)
            self.assertEqual(json.loads(lock.read_text()), approved_lock)
            # The restored lock is the current generation: no approval needed.
            output, code = self.run_in(shell, "goblin devshell refresh")
            self.assertEqual(code, 0, output)
            self.assertEqual([r["state"] for r in self.refreshes()[requests:]], ["failed", "ready"])


# 10^10 additions: evaluation effectively never ends.
ENDLESS = ("DEV_MARKER = toString (builtins.foldl' (a: _: builtins.foldl' (b: _: b + 1) a "
           "(builtins.genList (x: x) 100000)) 0 (builtins.genList (x: x) 100000));")


class DevShellConfinementWorkflow(DevShellWorkflow):
    """A flake cannot use evaluation to read the host, build before approval or hold the goblin."""

    def project(self):
        """A dev shell flake with an app that reads a Git repository outside
        it, which host Nix could read. Returns the flake file, its safe text,
        and the outside repository and the expression that fetches it."""
        root = tempfile.TemporaryDirectory(prefix="goblin-devshell-confined-")
        self.addCleanup(root.cleanup)
        project = Path(root.name) / "project"
        project.mkdir()
        outside = Path(root.name) / "outside"
        outside.mkdir()
        (outside / "token").write_text("host-secret")
        self.git(outside, "init", "-q")
        self.git(outside, "add", "token")
        self.git(outside, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "secret")
        fetch = f'builtins.fetchGit {{ url = "file://{outside}"; rev = "{self.git(outside, "rev-parse", "HEAD")}"; }}'
        leak = f"""      apps.x86_64-linux.leak = {{ type = "app"; program = "${{pkgs.writeShellScriptBin "leak" ''
          echo ${{builtins.readFile "${{{fetch}}}/token"}}
        ''}}/bin/leak"; }};
      devShells.x86_64-linux.default"""
        flake = project / "flake.nix"
        safe = TEMPLATE.replace("NIXPKGS", self.nixpkgs_url).replace("@MARKER@", "original") \
            .replace("      devShells.x86_64-linux.default", leak, 1)
        flake.write_text(safe)
        lock_flake(project)
        self.git(project, "init", "-q")
        self.git(project, "add", "flake.nix", "flake.lock")
        return flake, safe, outside, fetch

    def test_evaluation_cannot_reach_the_host_or_build_before_approval(self):
        flake, safe, outside, fetch = self.project()

        with step("a dev shell that reads the host does not launch"):
            flake.write_text(safe.replace('DEV_MARKER = "original";', f'DEV_MARKER = builtins.readFile "${{{fetch}}}/token";'))
            reader = self.run_cli(flake.parent, "--dev-shell", ".", "--name", "reader")
            self.assertNotEqual(reader.wait(), 0)
            record = self.session("reader")
            self.assertEqual(record["state"], "failed", record)
            self.assertIn(f'"{outside}" does not exist', record["detail"])
            self.assertNotIn("host-secret", record["detail"])

        flake.write_text(safe)
        shell = self.run_cli(flake.parent, "--dev-shell", ".", "--name", "dev")
        shell.expect(r"(?:^|\n)hook-ran in ")
        session = self.session("dev")["id"]

        with step("an app that reads the host does not evaluate"):
            output, code = self.run_in(shell, "goblin flake run '#leak'")
            self.assertEqual(code, 1, output)
            self.assertIn("could not evaluate app leak", output)
            self.assertNotIn("host-secret", output)

        with step("a refresh stuck in evaluation can be denied, and the goblin keeps working"):
            flake.write_text(safe.replace('DEV_MARKER = "original";', ENDLESS))
            n = self.send(shell, "goblin devshell refresh")
            record = self.d.wait(lambda: next((r for r in self.d.permissions(session=session)
                                               if r["kind"] == "devshell" and r["state"] == "pending"), None),
                                 timeout=60)
            # Nothing to approve before the preview, but the host can say no.
            self.assertIsNone(record.get("preview"))
            self.d.decide(record, False)
            output, code = self.end(shell, n, timeout=60)
            self.assertEqual(code, 1, output)
            self.assertEqual(self.refreshes()[-1]["state"], "denied")
            # The endless evaluation no longer holds the goblin's worker.
            flake.write_text(safe)
            output, code = self.run_in(shell, "goblin devshell diff", timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("No changes", output)

        with step("a refresh never builds an import from derivation"):
            name = f"goblins-ifd-{uuid.uuid4().hex[:12]}"
            # Its output path, computed without building it.
            out = command(["nix", "eval", "--impure", "--raw", "--expr",
                           f'let pkgs = import {self.nixpkgs} {{ system = "x86_64-linux"; }}; in '
                           f'(pkgs.runCommand "{name}" {{}} "echo built > $out").outPath'])
            flake.write_text(safe.replace(
                'DEV_MARKER = "original";',
                f'DEV_MARKER = builtins.readFile (pkgs.runCommand "{name}" {{}} "echo built > $out");'))
            output, code = self.run_in(shell, "goblin devshell refresh")
            self.assertEqual(code, 1, output)
            self.assertIn("allow-import-from-derivation", output)
            self.assertFalse(Path(out).exists(), out)

    def test_an_operator_limits_how_long_a_refresh_may_evaluate(self):
        flake, safe, _, _ = self.project()
        self.d = Daemon(env={**os.environ, "GOBLINS_EVALUATION_TIMEOUT": "5"})
        self.addCleanup(self.d.close)
        shell = self.run_cli(flake.parent, "--dev-shell", ".", "--name", "slow")
        shell.expect(r"(?:^|\n)hook-ran in ")
        flake.write_text(safe.replace('DEV_MARKER = "original";', ENDLESS))
        start = time.monotonic()
        output, code = self.run_in(shell, "goblin devshell refresh")
        self.assertLess(time.monotonic() - start, 60)
        self.assertEqual(code, 1, output)
        self.assertIn("evaluation timed out after 5 s", output)
        self.assertEqual([r["state"] for r in self.refreshes()], ["failed"])
        # The timed-out evaluation no longer holds the goblin's worker.
        flake.write_text(safe)
        output, code = self.run_in(shell, "goblin devshell diff", timeout=300)
        self.assertEqual(code, 0, output)
        self.assertIn("No changes", output)


DOCKER_FLAKE = """{
  inputs.nixpkgs.url = "NIXPKGS";
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


class DevShellDockerWorkflow(test_docker.DockerWorkflow):
    """Programs a dev shell gains after Docker is attached run in containers."""

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        DevShellWorkflow.setUpClass.__func__(cls)

    def write_flake(self, added):
        (self.project / "flake.nix").write_text(
            DOCKER_FLAKE.replace("NIXPKGS", self.nixpkgs_url).replace("@MARKER@", self.marker)
            .replace("@ADDED@", "true" if added else "false"))

    def program(self, output, name):
        match = re.search(rf"{name}:{self.marker}:(/nix/store/\S+)", output)
        self.assertIsNotNone(match, output)
        return match[1]

    def test_flake_apps_and_refreshed_tools_run_in_containers(self):
        self.marker = uuid.uuid4().hex[:12]
        self.project = self.root / "project"
        self.project.mkdir()
        self.write_flake(added=False)
        lock_flake(self.project)
        command(["git", "-C", str(self.project), "init", "-q"])
        command(["git", "-C", str(self.project), "add", "flake.nix", "flake.lock"])
        self.start_daemon()
        session, shell = self.shell("plain", cwd=self.project, dev_shell=str(self.project))
        self.enable(session, shell)
        self.load(shell)

        with step("a flake app runs in a container"):
            program = self.program(self.ok(shell, "goblin flake run .#greet", timeout=300), "greet")
            output = self.ok(shell, f"docker run --rm --network none -v /nix/store:/nix/store:ro goblins-test {program}")
            self.assertIn(f"greet:{self.marker}:{program}", output)

        with step("a tool a refresh adds runs in a container"):
            self.write_flake(added=True)
            n = self.send(shell, "goblin devshell refresh --reason 'add a tool'")
            record = self.d.wait(lambda: next((r for r in self.d.permissions(session=session, state="pending")
                                               if r["kind"] == "devshell" and r.get("preview")), None), timeout=180)
            self.d.decide(record, True)
            output, code = self.end(shell, n, timeout=300)
            self.assertEqual(code, 0, output)
            self.assertIn("Dev shell generation 2 is active", output)
            program = self.program(self.ok(shell, "bash -c '. /run/goblins/devshell/env.sh >/dev/null; added'"), "added")
            output = self.ok(shell, f"docker run --rm --network none -v /nix/store:/nix/store:ro goblins-test {program}")
            self.assertIn(f"added:{self.marker}:{program}", output)


if __name__ == "__main__":
    unittest.main()
