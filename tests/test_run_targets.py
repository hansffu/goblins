"""Raid owners running members of other configurations (checks.nix
run-target-goblins): a coordinator and its reviewer, through real controller
sockets, CLIs and namespaces."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC, receive
from support import command, lock_flake, pinned_nixpkgs, step


class RunTargetFixture(unittest.TestCase):
    """Members using other configurations (checks.nix run-target-goblins)."""

    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("run-target-goblins")

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblins-run-targets-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        (self.root / "reviewer").mkdir()
        self.d = Daemon(self.app, env={**os.environ, "GOBLINS_TEST_ROOT": str(self.root),
                                       "XDG_CACHE_HOME": str(self.root / "cache")})
        self.addCleanup(self.d.close)
        self.terminals = {}

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def running(self, launch):
        record = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                             timeout=60)
        self.assertEqual(record["state"], "running", record)
        return launch

    def terminal(self, launch):
        # One attachment per sandbox, reused by later commands. Sandbox launch
        # replies leave out the host-only terminal path.
        if launch["session"] not in self.terminals:
            peer = self.d.terminal(dict(launch, terminal=self.d.get(launch["session"])["terminal"]))
            self.addCleanup(peer.close)
            self.terminals[launch["session"]] = peer
        return self.terminals[launch["session"]]

    def launch(self, owner, name, agent_name=None, wait=True):
        """Run a member, creating the owner's raid when it has none."""
        if self.sandbox(owner["session"], "sessions.status", {})["raid"] is None:
            self.sandbox(owner["session"], "raids.create", {"raid": f"crew-{uuid.uuid4().hex[:8]}"})
        launch = self.sandbox(owner["session"], "sessions.start", {
            "key": uuid.uuid4().hex, "name": name, "agent_name": agent_name or name})
        return self.running(launch) if wait else launch

    def refused(self, owner, name, message, code=-32602):
        with self.assertRaises(ValueError) as error:
            self.launch(owner, name)
        self.assertEqual(error.exception.args[0]["code"], code)
        self.assertEqual(error.exception.args[0]["message"], message)

    def owner(self, launch):
        """Make a member the owner of its raid."""
        raid = self.d.get(launch["session"])["raid"]["name"]
        self.d.rpc.call("raids.set_owner", {"raid": raid, "session": launch["session"]})

    def host_start(self, name, **extra):
        return self.d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "name": name,
                                                  "configuration": self.d.manifest, "rows": 24, "cols": 80, **extra})

    def run_in(self, launch, script, done):
        """Run shell commands in a sandbox; they write into the shared workspace."""
        self.terminal(launch).sendall(script.encode() + f"; touch /workspace/{done}\n".encode())
        self.d.wait(lambda: (self.workspace / done).exists(), timeout=30)

    def start_root(self, name="coordinator", **extra):
        root = self.d.start(name, agent_name="root", **extra)
        self.workspace = self.d.state / root["session"] / "resources/workspace"
        return root

    def failed(self, launch):
        return self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] == "failed" else None), timeout=60)


class CoordinatorWorkflow(RunTargetFixture):
    def test_a_coordinator_runs_a_reviewer(self):
        targets = ["coordinator", "reviewer", "shell", "broken", "unscoped"]

        with step("a coordinator in a scope sees what it may run"):
            root = self.running(self.host_start("coordinator", scope="work", agent_name="root"))
            self.workspace = self.d.state / root["session"] / "resources/workspace"
            self.run_in(root, "goblin targets > /workspace/targets; goblin status > /workspace/status; "
                              "goblin __complete-names -- goblin run > /workspace/complete", "listed")
            self.assertIn("reviewer  reviewer run target\n  shopt  extglob  A bash option\n",
                          (self.workspace / "targets").read_text())
            self.assertIn("Configuration: coordinator\nDescription: coordinator run target\n"
                          "Run targets: coordinator, reviewer, shell, broken, unscoped\n",
                          (self.workspace / "status").read_text())
            self.assertEqual((self.workspace / "complete").read_text().split(), targets)
            self.assertIn("Host commands: none\n", (self.workspace / "status").read_text())
            # The host's launch made this manifest the server's current one.
            self.assertIn(self.d.manifest, self.d.cli("server", "status").stdout)

        with step("completion offers the reviewer's launch options, in bash too"):
            self.run_in(root, "goblin __complete-names -- goblin run reviewer -o > /workspace/choices; "
                              "goblin __complete-names -- goblin run reviewer -o shopt = > /workspace/bash-choices; "
                              "goblin completions bash > /workspace/completion.bash", "completed")
            self.assertEqual((self.workspace / "choices").read_text().split(), ["shopt=extglob"])
            # Bash splits `-o shopt=` at the equals sign: only the choices.
            self.assertEqual((self.workspace / "bash-choices").read_text().split(), ["extglob"])
            # Through the Bash hook, at `-o shopt=<TAB>`, `-o shopt=ext<TAB>` and
            # a second `-o <TAB>` after `-o shopt=extglob`, as Bash splits them.
            for words, expected in ((["goblin", "run", "reviewer", "-o", "shopt", "="], "extglob"),
                                    (["goblin", "run", "reviewer", "-o", "shopt", "=", "ext"], "extglob"),
                                    (["goblin", "run", "reviewer", "-o", "shopt", "=", "extglob", "-o", ""],
                                     "shopt=extglob")):
                code = ("source /workspace/completion.bash; COMP_WORDS=(" + shlex.join(words) + "); "
                        f"COMP_CWORD={len(words) - 1}; _goblin goblin {shlex.quote(words[-1])} "
                        f"{shlex.quote(words[-2])}; printf '%s\\n' \"${{COMPREPLY[@]}}\" > /workspace/hook")
                self.run_in(root, "bash --noprofile --norc -c " + shlex.quote(code), f"hook-{len(words)}")
                self.assertEqual((self.workspace / "hook").read_text().split(), [expected], words)

        with step("the host approves a package for the coordinator"):
            request, permission = self.d.pending(root["session"])
            self.addCleanup(request.close)
            self.d.decide(permission, True)
            request.peer.settimeout(120)
            self.assertEqual(receive(request.peer)["result"]["status"], "ready")

        with step("the coordinator runs a reviewer with a launch option"):
            self.run_in(root, "goblin raid create crew; "
                              "goblin run reviewer --name opted -o shopt=extglob > /workspace/opted.json", "ran")
            member = self.running(json.loads((self.workspace / "opted.json").read_text()))
            record = self.d.get(member["session"])
            self.assertEqual((record["name"], record["created_by"]["id"]), ("reviewer", root["session"]))
            self.assertEqual(record["description"], "reviewer run target")
            # The current manifest: the one the host last used.
            self.assertEqual(record["configuration"], self.d.manifest)
            self.assertEqual((record["scope"], record["options"]), ("work", {"shopt": "extglob"}))
            self.assertEqual(record["packages"], [])

        with step("the reviewer runs its own configuration, with its environment, files, binds and arguments"):
            writable = self.root / "reviewer"
            self.run_in(member, "printf '%s' \"$TARGET_MARKER\" > /workspace/marker; "
                                "cat /etc/run-target-test.conf > /workspace/etc; "
                                f"echo member > {writable}/from-member; echo $? > /workspace/member-rw; "
                                "tr '\\0' ' ' < /proc/$$/cmdline > /workspace/cmdline; shopt -q extglob; "
                                "echo $? > /workspace/extglob; goblin status > /workspace/member-status", "member")
            self.assertEqual((self.workspace / "marker").read_text(), "reviewer")
            self.assertEqual((self.workspace / "etc").read_text(), "reviewer\n")
            self.assertEqual((self.workspace / "member-rw").read_text(), "0\n")
            self.assertEqual((writable / "from-member").read_text(), "member\n")
            # Its configuration's arguments, then the choice's.
            self.assertTrue((self.workspace / "cmdline").read_text().endswith("--noprofile --norc -i -O extglob "))
            self.assertEqual((self.workspace / "extglob").read_text(), "0\n")
            self.assertIn("Launch options: shopt=extglob\n", (self.workspace / "member-status").read_text())
            status = self.sandbox(member["session"], "sessions.status", {})
            self.assertEqual((status["name"], status["allowed_run_targets"]), ("reviewer", []))

        with step("the coordinator gets none of the reviewer's access"):
            self.run_in(root, "printf '%s' \"$TARGET_MARKER\" > /workspace/root-marker; "
                              "cat /etc/run-target-test.conf > /workspace/root-etc; "
                              f"test -e {writable}; echo $? > /workspace/root-rw", "root")
            self.assertEqual((self.workspace / "root-marker").read_text(), "coordinator")
            self.assertEqual((self.workspace / "root-etc").read_text(), "coordinator\n")
            self.assertEqual((self.workspace / "root-rw").read_text(), "1\n")

        with step("the reviewer shares the coordinator's scope and sees its processes"):
            self.run_in(root, "sleep 300 & echo $! > /workspace/root-sleep", "slept")
            pid = (self.workspace / "root-sleep").read_text().strip()
            self.run_in(member, f"test -e /proc/{pid}; echo $? > /workspace/seen", "seen-done")
            self.assertEqual((self.workspace / "seen").read_text(), "0\n")

        with step("the coordinator's approval reaches the reviewer when it asks"):
            self.run_in(member, "goblin request-package hello --reason copied > /workspace/requested 2>&1; "
                                "command -v hello > /workspace/hello", "requested-done")
            self.assertEqual((self.workspace / "hello").read_text(), "/run/goblins/packages/current/bin/hello\n")
            self.assertEqual(self.d.get(member["session"])["packages"], ["hello"])
            self.assertEqual(self.d.permissions(session=member["session"], state="pending"), [])

        with step("raid members show the reviewer's options"):
            self.run_in(root, "goblin raid members > /workspace/members", "members-done")
            self.assertRegex((self.workspace / "members").read_text(),
                             r"\nopted +-  +reviewer .* shopt=extglob +\S+\n")

        with step("a configuration with a missing bind, or one that refuses the scope, fails alone"):
            broken = self.failed(self.launch(root, "broken", wait=False))
            self.assertIn("host bind", broken["detail"])
            unscoped = self.failed(self.launch(root, "unscoped", wait=False))
            self.assertIn("configuration 'unscoped' cannot be launched here: 'unscoped' does not allow scope 'work'",
                          unscoped["detail"])
            self.assertEqual(self.d.get(root["session"])["state"], "running")

        with step("the coordinator exits; the reviewer keeps running"):
            self.terminal(root).sendall(b"exit\n")
            self.d.wait(lambda: self.d.get(root["session"])["state"] == "stopped")
            self.assertEqual(self.d.get(member["session"])["state"], "running")


class DevShellMemberWorkflow(RunTargetFixture):
    def test_a_member_shares_the_owners_dev_shell_generation(self):
        from test_devshell import TEMPLATE
        project = self.root / "project"
        project.mkdir()
        flake = lambda marker: (project / "flake.nix").write_text(
            TEMPLATE.replace("NIXPKGS", pinned_nixpkgs()[0]).replace("@MARKER@", marker))
        flake("original")
        lock_flake(project)
        command(["git", "-C", str(project), "init", "-q"])
        command(["git", "-C", str(project), "add", "flake.nix", "flake.lock"])
        result = subprocess.run([str(self.app), "--state-dir", str(self.d.state), "run", "coordinator",
                                 "--dev-shell", ".", "--name", "root", "--detached"],
                                text=True, capture_output=True, cwd=project)
        self.assertEqual(result.returncode, 0, result.stderr)
        root = json.loads(result.stdout)
        # Evaluating the dev shell can take a while on a cold store.
        record = self.d.wait(lambda: (r if (r := self.d.get(root["session"]))["state"] != "starting" else None),
                             timeout=600)
        self.assertEqual(record["state"], "running", record)
        self.workspace = self.d.state / root["session"] / "resources/workspace"
        # The member gets the owner's current generation, not the edited flake.
        flake("edited")
        member = self.launch(root, "reviewer", "member")
        self.assertEqual(self.d.get(member["session"])["dev_shell"], str(project))
        self.run_in(member, "printf '%s %s' \"$DEV_MARKER\" \"$TARGET_MARKER\" > /workspace/marker; pwd > /workspace/cwd",
                    "member-done")
        self.assertEqual((self.workspace / "marker").read_text(), "original reviewer")
        # It also shares the owner's pinned working directory.
        self.assertEqual((self.workspace / "cwd").read_text(), f"{project}\n")


class RunTargetDockerTests(RunTargetFixture):
    """Docker attachment of members with other configurations (rootless Docker)."""

    def test_member_attachment_follows_owner_or_own_definition(self):
        attached = self.start_root("attached")
        self.assertTrue(self.d.get(attached["session"])["docker_enabled"])
        plain = self.launch(attached, "plain")
        self.assertTrue(self.d.get(plain["session"])["docker_enabled"])
        # Both reach the scope's one engine.
        self.run_in(attached, "docker info --format '{{.ID}}' > /workspace/owner-engine", "owner-done")
        self.run_in(plain, "docker info --format '{{.ID}}' > /workspace/member-engine", "member-done")
        engine = (self.workspace / "owner-engine").read_text()
        self.assertTrue(engine.strip())
        self.assertEqual((self.workspace / "member-engine").read_text(), engine)
        # One engine cannot mount two owners' different /workspace sources.
        for launch in (attached, plain):
            self.d.rpc.call("sessions.stop", {"session": launch["session"]})
            self.d.wait(lambda: self.d.get(launch["session"])["state"] == "stopped")
        detached = self.d.start("detached", agent_name="other")
        self.assertFalse(self.d.get(detached["session"])["docker_enabled"])
        eager = self.launch(detached, "eager")
        self.assertTrue(self.d.get(eager["session"])["docker_enabled"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
