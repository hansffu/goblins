"""Raid owners running members, through real controller sockets, CLIs and namespaces."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC, frame, receive
from support import FLAKE, command
from terminal_support import Terminal


class RaidMemberTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def refused(self, session, method, params):
        with self.assertRaises(ValueError) as error:
            self.sandbox(session, method, params)
        return error.exception.args[0]

    def running(self, launch):
        record = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None), timeout=60)
        self.assertEqual(record["state"], "running", record)
        return launch

    def terminal(self, launch):
        # Sandbox launch replies deliberately omit the host-only terminal path.
        launch = dict(launch, terminal=self.d.get(launch["session"])["terminal"])
        peer = self.d.terminal(launch)
        self.addCleanup(peer.close)
        return peer

    def cli(self, *args, **kwargs):
        result = subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), *args],
                                env=self.d.env, timeout=60, text=True, capture_output=True, **kwargs)
        self.assertEqual(result.returncode, 0,
                         f"goblins {args}: {result.stdout}\n{result.stderr}")
        return result

    def wait_json(self, path):
        def read():
            try:
                return json.loads(path.read_text())
            except (OSError, ValueError):
                return None
        return self.d.wait(read)

    def test_owners_run_list_and_stop_only_the_members_they_created(self):
        owner = self.d.start(agent_name="owner")
        other = self.d.start(agent_name="other")
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", "owner"])
        self.addCleanup(terminal.close)
        terminal.send("goblin run shell --name early; printf 'EARLY=%s\\n' $status\n")
        terminal.expect(r"only a raid owner can run goblins[^\n]*\n(?:.*\n)*?EARLY=2\n")
        terminal.send("goblin raid create crew; goblin run shell --name member > /workspace/member.json; "
                      "goblin status > /workspace/status; goblin __complete-names -- goblin run > /workspace/targets; "
                      "goblin __complete-names -- goblin stop > /workspace/members; touch /workspace/done\n")
        workspace = self.d.state / owner["session"] / "resources/workspace"
        self.d.wait(lambda: (workspace / "done").exists())
        member = self.running(json.loads((workspace / "member.json").read_text()))
        self.assertEqual(set(member), {"session", "agent_name", "state"})
        self.assertIn("Raid: crew (role: none, owner: owner)\n", (workspace / "status").read_text())
        self.assertEqual((workspace / "targets").read_text().split(), ["shell"])
        self.assertEqual((workspace / "members").read_text().split(), ["member"])
        record = self.d.get(member["session"])
        self.assertEqual(record["created_by"], {"id": owner["session"], "agent_name": "owner"})
        self.assertNotIn("parent", record)
        self.assertNotIn("path", record)
        status = self.sandbox(member["session"], "sessions.status", {})
        self.assertEqual(set(status), {"id", "agent_name", "name", "description", "state", "agent_state", "scope",
                                       "docker_enabled", "dev_shell", "attached", "attached_since", "nix", "host_commands",
                                       "permission_bypass", "allowed_run_targets", "raid", "created_by"})
        self.assertIsNone(status["permission_bypass"])
        self.assertEqual(status["allowed_run_targets"], ["shell"])
        self.assertEqual(status["raid"]["owner_name"], "owner")
        # The owner sees only the members created in its raid.
        invited = self.cli("raid", "invite", "crew", "other")
        self.assertEqual(invited.returncode, 0, invited.stderr)
        listing = self.sandbox(owner["session"], "sessions.list", {})
        self.assertEqual([r["id"] for r in listing], [member["session"]])
        for record in listing:
            self.assertTrue({"configuration", "terminal", "identity", "detail", "path", "parent"}.isdisjoint(record))
        for target in (owner["session"], other["session"], "other", "absent"):
            for method in ("sessions.get", "sessions.stop"):
                self.assertEqual(self.refused(owner["session"], method, {"session": target})["code"], -32004)
        # No goblin attaches to, or resizes, another goblin's terminal.
        for method, extra in (("sessions.attach", {}), ("sessions.resize", {"rows": 40, "cols": 90})):
            self.assertEqual(self.refused(owner["session"], method, {"session": member["session"], **extra})["code"],
                             -32601)
        pipeline = RPC(self.d.state / owner["session"] / "resources/request.sock")
        try:
            pipeline.peer.sendall(frame({"jsonrpc": "2.0", "id": 2, "method": "sessions.attach",
                                         "params": {"session": member["session"]}}) + b"injected keys")
            self.assertIn("error", receive(pipeline.peer))
            self.assertFalse(self.d.get(member["session"])["terminal_attached"])
        finally:
            pipeline.close()
        for params in ({"name": "codex"}, {"configuration": self.d.manifest}, {"cwd": "/tmp"}, {"parent": "."},
                       {"inherit_raid": True}):
            with self.assertRaises(ValueError):
                self.sandbox(owner["session"], "sessions.start", {"key": uuid.uuid4().hex, "name": "shell", **params})
        for method in ("permissions.list", "permissions.decide", "server.stop", "state.subscribe", "raids.invite"):
            with self.assertRaises(ValueError):
                self.sandbox(owner["session"], method, {})
        # A member that is not the owner runs nobody and cannot leave.
        self.assertEqual(self.refused(member["session"], "sessions.start",
                                      {"key": "k", "name": "shell"})["code"], -32009)
        self.assertIn("stay until stopped", self.refused(member["session"], "raids.leave", {})["message"])
        # Equal idempotency keys of different owners never meet.
        params = {"key": "same-key", "name": "shell", "agent_name": "repeat"}
        first = self.running(self.sandbox(owner["session"], "sessions.start", params))
        self.assertEqual(self.sandbox(owner["session"], "sessions.start", params), first)
        third = self.d.start(agent_name="third")
        self.sandbox(third["session"], "raids.create", {"raid": "elsewhere"})
        with self.assertRaises(ValueError) as error:
            self.sandbox(third["session"], "sessions.start", params)
        self.assertIn("already in use", error.exception.args[0]["message"])
        # The owner stops its member; the CLI says who did.
        terminal.send("goblin stop member; printf 'STOP=%s\\n' $status\n")
        terminal.expect(r"STOP=0\n")
        self.d.wait(lambda: self.d.get(member["session"])["state"] == "stopped")
        self.assertEqual(self.d.get(member["session"])["stop_reason"],
                         f"stopped by raid owner owner ({owner['session']})")
        terminal.send("exit\n")
        self.assertEqual(terminal.wait(), 0)
        # Members outlive the owner that ran them.
        self.assertEqual(self.d.get(first["session"])["state"], "running")

    def test_members_outlive_their_owner_and_a_new_owner_takes_them_over(self):
        owner = self.d.start(agent_name="owner")
        member = self.d.member(owner["session"], agent_name="member")
        raid = self.d.get(member["session"])["raid"]["name"]
        self.terminal(owner).sendall(b"exit\n")
        self.d.wait(lambda: self.d.get(owner["session"])["state"] == "stopped")
        self.assertIsNone(self.d.get(owner["session"])["stop_reason"])
        record = self.d.get(member["session"])
        self.assertEqual(record["state"], "running")
        self.assertEqual((record["raid"]["paused"], record["raid"]["owner"]), (True, None))
        # Nobody owns a paused raid, so nobody runs or stops its members.
        self.assertEqual(self.refused(member["session"], "sessions.start", {"key": "k", "name": "shell"})["code"],
                         -32009)
        resumed = self.cli("raid", "set-owner", raid, "member")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        late = self.d.member(member["session"], agent_name="late")
        self.assertEqual(self.d.get(late["session"])["created_by"]["agent_name"], "member")
        self.assertEqual([r["id"] for r in self.sandbox(member["session"], "sessions.list", {})], [late["session"]])
        # Members of the raid message each other by name.
        sent = self.sandbox(late["session"], "messages.send", {"key": "k", "to": "member", "body": "hi"})
        self.assertEqual((sent["recipient"], sent["sender_name"]), (member["session"], "late"))

    def test_members_share_the_owners_workspace_and_pinned_working_directory(self):
        with tempfile.TemporaryDirectory(prefix="goblin-raid-work-") as folder:
            original = Path(folder) / "project"
            original.mkdir()
            (original / "input").write_text("owner\n")
            result = self.cli("run", "shell", "--name", "owner", "--detatched", cwd=original)
            self.assertEqual(result.returncode, 0, result.stderr)
            owner = self.running(json.loads(result.stdout))
            moved = Path(folder) / "moved"
            original.rename(moved)
            original.mkdir()
            (original / "input").write_text("replacement-host-path\n")
            shell = self.terminal(owner)
            shell.sendall(b"goblin raid create crew; goblin run shell --name member > member.json\n")
            member = self.running(self.wait_json(moved / "member.json"))
            self.terminal(member).sendall(b"cat input > seen; echo member-write > shared\n")
            self.d.wait(lambda: (moved / "shared").exists())
            self.assertEqual((moved / "seen").read_text(), "owner\n")
            self.assertFalse((original / "shared").exists())
            self.assertEqual(Path(self.d.get(member["session"])["cwd"]), original)

    def test_snapshot_is_shared_with_members(self):
        with tempfile.TemporaryDirectory(prefix="goblin-raid-snapshot-") as folder:
            source = Path(folder)
            (source / "input").write_text("host\n")
            d = Daemon(workspace=source)
            self.addCleanup(d.close)
            self.d = d
            owner = d.start()
            member = d.member(owner["session"])
            owner_workspace = d.state / owner["session"] / "resources/workspace"
            self.terminal(member).sendall(b"echo member > input; echo done > marker\n")
            d.wait(lambda: (owner_workspace / "marker").exists())
            self.assertEqual((owner_workspace / "input").read_text(), "member\n")
            self.assertEqual((source / "input").read_text(), "host\n")

    def test_members_launch_from_the_last_validated_host_manifest(self):
        mutable = Path(self.d.temp.name) / "mutable.json"
        mutable.write_text(Path(self.d.manifest).read_text())
        owner = self.d.start(manifest=mutable, agent_name="owner")
        member = self.d.member(owner["session"])
        self.assertEqual(self.d.get(member["session"])["configuration"], str(mutable))
        # The current manifest is a file the host chose; a member reads it
        # when it starts.
        mutable.write_text("invalid replacement manifest")
        broken = self.d.member(owner["session"], wait=False)
        failed = self.d.wait(lambda: (r if (r := self.d.get(broken["session"]))["state"] == "failed" else None),
                             timeout=60)
        self.assertIn("invalid configuration", failed["detail"])
        # A host launch that fails validation leaves the current manifest.
        rejected = self.d.start(manifest=mutable, wait=False)
        self.d.wait(lambda: self.d.get(rejected["session"])["state"] == "failed")
        self.assertIn(str(mutable), self.cli("server", "status").stdout)
        # A valid host launch makes its manifest current.
        self.d.start(agent_name="later")
        self.assertIn(self.d.manifest, self.cli("server", "status").stdout)
        member = self.d.member(owner["session"])
        self.assertEqual(self.d.get(member["session"])["configuration"], self.d.manifest)
        # There is no tree to launch into.
        with self.assertRaisesRegex(ValueError, "unknown field `parent`"):
            self.d.rpc.call("sessions.start", {"key": "host-child", "configuration": self.d.manifest,
                                               "name": "shell", "parent": owner["session"], "rows": 24, "cols": 80})
        with self.assertRaisesRegex(ValueError, "'codex' is not an allowed run target of 'shell'; allowed: shell"):
            self.sandbox(owner["session"], "sessions.start", {"key": "codex", "name": "codex"})

    def test_approvals_are_copied_to_members_run_later(self):
        root = self.d.start(agent_name="root")
        before = self.d.member(root["session"], agent_name="before")
        request, permission = self.d.pending(root["session"])
        self.addCleanup(request.close)
        self.d.decide(permission, True)
        request.peer.settimeout(120)
        self.assertEqual(receive(request.peer)["result"]["status"], "ready")
        # Members run afterwards get the approval as auto-approve, and pass it
        # on once they own the raid; nothing is mounted until they ask.
        after = self.d.member(root["session"], agent_name="after")
        raid = self.d.get(after["session"])["raid"]["name"]
        self.assertEqual(self.cli("raid", "set-owner", raid, "after").returncode, 0)
        deeper = self.d.member(after["session"], agent_name="deeper")
        for member in (deeper, after):
            record = self.d.get(member["session"])
            self.assertEqual((record["packages"], record["auto_approve"]), ([], ["hello"]))
            peer = RPC(self.d.state / member["session"] / "resources/request.sock")
            self.addCleanup(peer.close)
            peer.peer.settimeout(120)
            result = peer.call("permissions.request", {"kind": "package", "package": "hello", "reason": "copied"})
            self.assertEqual(result["status"], "ready")
            self.assertTrue(self.d.rpc.call("permissions.get", {"request": result["request"]})["approved"])
        # A repeated request is the goblin's own grant.
        peer = RPC(self.d.state / after["session"] / "resources/request.sock")
        self.addCleanup(peer.close)
        peer.peer.settimeout(120)
        self.assertEqual(peer.call("permissions.request", {"kind": "package", "package": "hello",
                                                           "reason": "again"})["status"], "ready")
        # A member run before the grant still asks.
        peer, record = self.d.pending(before["session"])
        self.addCleanup(peer.close)
        self.assertIsNone(record["approved"])
        self.d.decide(record, False)
        self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        # Copied once: a later grant to the owner does not reach members.
        owner_request, owner_permission = self.d.pending(after["session"], "cowsay")
        self.addCleanup(owner_request.close)
        self.d.decide(owner_permission, True)
        owner_request.peer.settimeout(120)
        self.assertEqual(receive(owner_request.peer)["result"]["status"], "ready")
        pending, waiting = self.d.pending(deeper["session"], "cowsay")
        self.addCleanup(pending.close)
        self.assertIsNone(waiting["approved"])
        self.d.decide(waiting, False)
        self.assertEqual(receive(pending.peer)["result"]["status"], "denied")
        self.assertEqual(self.d.get(after["session"])["packages"], ["hello", "cowsay"])


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

    sandbox = RaidMemberTests.sandbox
    running = RaidMemberTests.running

    def terminal(self, launch):
        # One attachment per sandbox, reused by later commands.
        if launch["session"] not in self.terminals:
            self.terminals[launch["session"]] = RaidMemberTests.terminal(self, launch)
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


class RunTargetTests(RunTargetFixture):
    def test_member_runs_its_own_configuration_in_the_owners_workspace(self):
        root = self.start_root()
        root_status = self.sandbox(root["session"], "sessions.status", {})
        self.assertEqual(root_status["allowed_run_targets"], ["coordinator", "reviewer", "shell", "broken", "unscoped"])
        member = self.launch(root, "reviewer")
        record = self.d.get(member["session"])
        self.assertEqual((record["name"], record["created_by"]["id"]), ("reviewer", root["session"]))
        self.assertEqual(record["description"], "reviewer run target")
        # The current manifest: the one the host last used.
        self.assertEqual(record["configuration"], self.d.manifest)
        status = self.sandbox(member["session"], "sessions.status", {})
        self.assertEqual((status["name"], status["allowed_run_targets"]), ("reviewer", []))
        writable = self.root / "reviewer"
        self.run_in(member, f"printf '%s' \"$TARGET_MARKER\" > /workspace/marker; cat /etc/run-target-test.conf > /workspace/etc; "
                            f"echo member > {writable}/from-member; echo $? > /workspace/member-rw", "member-done")
        self.assertEqual((self.workspace / "marker").read_text(), "reviewer")
        self.assertEqual((self.workspace / "etc").read_text(), "reviewer\n")
        self.assertEqual((self.workspace / "member-rw").read_text(), "0\n")
        self.assertEqual((writable / "from-member").read_text(), "member\n")
        # The owner gains none of the member's declared access.
        self.run_in(root, f"printf '%s' \"$TARGET_MARKER\" > /workspace/root-marker; cat /etc/run-target-test.conf > /workspace/root-etc; "
                          f"test -e {writable}; echo $? > /workspace/root-rw", "root-done")
        self.assertEqual((self.workspace / "root-marker").read_text(), "coordinator")
        self.assertEqual((self.workspace / "root-etc").read_text(), "coordinator\n")
        self.assertEqual((self.workspace / "root-rw").read_text(), "1\n")
        self.run_in(root, "goblin status > /workspace/status; goblin __complete-names -- goblin run > /workspace/complete",
                    "status-done")
        self.assertIn("Configuration: coordinator\nDescription: coordinator run target\n"
                      "Run targets: coordinator, reviewer, shell, broken, unscoped\n",
                      (self.workspace / "status").read_text())
        self.assertEqual((self.workspace / "complete").read_text().split(),
                         ["coordinator", "reviewer", "shell", "broken", "unscoped"])
        # A missing bind source of a permitted configuration fails only that member.
        broken = self.failed(self.launch(root, "broken", wait=False))
        self.assertIn("host bind", broken["detail"])
        self.assertEqual(self.d.get(root["session"])["state"], "running")
        # The owner exiting stops nobody else.
        self.terminal(root).sendall(b"exit\n")
        self.d.wait(lambda: self.d.get(root["session"])["state"] == "stopped")
        self.assertEqual(self.d.get(member["session"])["state"], "running")

    def test_each_owner_uses_its_own_list(self):
        root = self.start_root()
        self.refused(root, "unlisted", "configuration 'unlisted' is not an allowed run target of 'coordinator'; "
                                       "allowed: coordinator, reviewer, shell, broken, unscoped")
        reviewer = self.launch(root, "reviewer")
        # Only the owner runs members; as owner, an empty list permits nothing.
        self.refused(reviewer, "reviewer",
                     "only a raid owner can run goblins; create one with goblin raid create NAME", -32009)
        self.owner(reviewer)
        self.refused(reviewer, "reviewer", "configuration 'reviewer' permits no run targets")
        # Ownership moves on; a shell owner may run only what its list names.
        self.owner(root)
        shell = self.launch(root, "shell")
        self.owner(shell)
        leaf = self.launch(shell, "reviewer", "leaf")
        self.assertEqual(self.d.get(leaf["session"])["created_by"]["id"], shell["session"])
        self.refused(shell, "coordinator",
                     "configuration 'coordinator' is not an allowed run target of 'shell'; allowed: reviewer")

    def test_member_shares_the_owners_scope(self):
        launch = self.host_start("coordinator", scope="work", agent_name="root")
        root = self.running(launch)
        self.workspace = self.d.state / root["session"] / "resources/workspace"
        member = self.launch(root, "reviewer")
        self.assertEqual(self.d.get(member["session"])["scope"], "work")
        # The current manifest is read when the member starts.
        unscoped = self.failed(self.launch(root, "unscoped", wait=False))
        self.assertIn("configuration 'unscoped' cannot be launched here: 'unscoped' does not allow scope 'work'",
                      unscoped["detail"])
        # Members share the scope's namespaces: the member sees the owner's processes.
        self.run_in(root, "sleep 300 & echo $! > /workspace/root-sleep", "root-done")
        pid = (self.workspace / "root-sleep").read_text().strip()
        self.run_in(member, f"test -e /proc/{pid}; echo $? > /workspace/seen", "member-done")
        self.assertEqual((self.workspace / "seen").read_text(), "0\n")

    def test_member_shares_the_owners_dev_shell_generation(self):
        from test_devshell import TEMPLATE
        nixpkgs = command(["nix", "eval", "--impure", "--raw", "--expr",
                           f'(builtins.getFlake "{FLAKE}").inputs.nixpkgs.outPath'])
        project = self.root / "project"
        project.mkdir()
        flake = lambda marker: (project / "flake.nix").write_text(
            TEMPLATE.replace("NIXPKGS", nixpkgs).replace("@MARKER@", marker))
        flake("original")
        command(["nix", "flake", "lock", f"path:{project}"])
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

    def test_owner_approvals_reach_a_different_configuration_on_request(self):
        root = self.start_root()
        request, permission = self.d.pending(root["session"])
        self.addCleanup(request.close)
        self.d.decide(permission, True)
        request.peer.settimeout(120)
        self.assertEqual(receive(request.peer)["result"]["status"], "ready")
        member = self.launch(root, "reviewer")
        self.assertEqual(self.d.get(member["session"])["packages"], [])
        peer = RPC(self.d.state / member["session"] / "resources/request.sock")
        self.addCleanup(peer.close)
        peer.peer.settimeout(120)
        result = peer.call("permissions.request", {"kind": "package", "package": "hello", "reason": "copied"})
        self.assertEqual(result["status"], "ready")
        self.assertTrue(self.d.rpc.call("permissions.get", {"request": result["request"]})["approved"])
        self.assertEqual(self.d.get(member["session"])["packages"], ["hello"])

    def test_members_follow_the_build_the_host_last_used(self):
        root = self.start_root()
        updated = fixture_app("run-target-goblins-updated")
        cli = lambda *args: subprocess.run([str(updated), "--state-dir", str(self.d.state), *args],
                                           text=True, capture_output=True)
        # A launch from the new build makes its manifest current, which no
        # longer declares the reviewer; the owner's own list still names it.
        result = cli("run", "coordinator", "--name", "newer", "--detached")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.running(json.loads(result.stdout))
        late = self.failed(self.launch(root, "reviewer", "late", wait=False))
        self.assertIn("unknown goblin 'reviewer'", late["detail"])
        # Never permitted: refused before anything is launched.
        self.refused(root, "unlisted", "configuration 'unlisted' is not an allowed run target of 'coordinator'; "
                                       "allowed: coordinator, reviewer, shell, broken, unscoped")

    def test_daemon_rejects_an_allowed_run_targets_list_over_the_bound(self):
        manifest = self.root / "oversized.json"
        configuration = json.loads(Path(self.d.manifest).read_text())
        configuration["goblins"]["coordinator"]["allowed_run_targets"] = [f"g{i}" for i in range(65)]
        manifest.write_text(json.dumps(configuration))
        record = self.failed(self.d.start("coordinator", manifest=manifest, wait=False))
        self.assertIn("invalid allowed_run_targets for 'coordinator'", record["detail"])


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
