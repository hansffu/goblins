"""Ownership boundaries through real controller sockets, CLIs and namespaces."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from daemon_support import Daemon, RPC, ROOT, frame, receive
from support import command
from terminal_support import Terminal


class OwnershipTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def running(self, launch):
        record = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None), timeout=60)
        self.assertEqual(record["state"], "running", record)
        return launch

    def child(self, owner, name="child", **extra):
        return self.running(self.sandbox(owner["session"], "sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "agent_name": name, **extra,
        }))

    def terminal(self, launch):
        # Inner launch replies deliberately omit the host-only terminal path.
        launch = dict(launch, terminal=self.d.get(launch["session"])["terminal"])
        peer = self.d.terminal(launch)
        self.addCleanup(peer.close)
        return peer

    def cli(self, *args, **kwargs):
        return subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), *args],
                              text=True, capture_output=True, **kwargs)

    def wait_json(self, path):
        def read():
            try:
                return json.loads(path.read_text())
            except (OSError, ValueError):
                return None
        return self.d.wait(read)

    def test_nested_run_attach_status_detach_resize_and_exit(self):
        import fcntl
        import struct
        import termios
        parent = self.d.start(agent_name="parent")
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", "parent"])
        self.addCleanup(terminal.close)
        terminal.send("goblins status\n")
        terminal.expect(
            r"Sandbox: parent\nConfiguration: shell\n"
            r"Description: fish running in a Goblins sandbox\n"
        )
        terminal.send("goblins run shell --name child; printf 'CHILD_EXIT=%s\\n' $status\n")
        child = self.d.wait(lambda: next((r for r in self.d.rpc.call("sessions.list", {})
                                         if r["agent_name"] == "child" and r["state"] == "running"), None))
        self.d.wait(lambda: self.d.get(child["id"])["terminal_attached"])
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: child\nConfiguration: shell\n")
        child_status = self.sandbox(child["id"], "sessions.status", {})
        self.assertEqual(child_status["agent_name"], "child")
        self.assertEqual(
            child_status["description"], "fish running in a Goblins sandbox"
        )
        terminal.send("goblins run shell --name leaf; echo LEAF_RETURN\n")
        leaf = self.d.wait(lambda: next((r for r in self.d.rpc.call("sessions.list", {})
                                        if r["agent_name"] == "leaf" and r["state"] == "running"), None))
        self.d.wait(lambda: self.d.get(leaf["id"])["terminal_attached"])
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: leaf\nConfiguration: shell\n")
        fcntl.ioctl(terminal.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 111, 0, 0))
        # Both relays must propagate SIGWINCH down to the leaf PTY.
        terminal.send("sleep 1; stty size\n")
        terminal.expect(r"(?:^|\n)31 111\n")
        terminal.send("goblins detach\n")
        self.d.wait(lambda: self.d.get(leaf["id"])["terminal_detached"])
        terminal.expect(r"(?:^|\n)LEAF_RETURN\n")
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: child\nConfiguration: shell\n")
        terminal.send("goblins detach\n")
        terminal.expect(r"(?:^|\n)CHILD_EXIT=0\n")
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: parent\nConfiguration: shell\n")
        # Attach a grandchild by relative path, without attaching its parent.
        terminal.send("goblins attach child/leaf; printf 'LEAF_EXIT=%s\\n' $status\n")
        self.d.wait(lambda: self.d.get(leaf["id"])["terminal_attached"])
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: leaf\nConfiguration: shell\n")
        terminal.send("exit 7\n")
        terminal.expect(r"(?:^|\n)LEAF_EXIT=7\n")
        terminal.send("goblins attach child; printf 'REATTACH_EXIT=%s\\n' $status\n")
        self.d.wait(lambda: self.d.get(child["id"])["terminal_attached"])
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: child\nConfiguration: shell\n")
        terminal.send("exit 9\n")
        terminal.expect(r"(?:^|\n)REATTACH_EXIT=9\n")
        terminal.send("goblins run shell --name background --detatched > /workspace/background.json\n")
        workspace = self.d.state / parent["session"] / "resources/workspace"
        background = self.wait_json(workspace / "background.json")
        self.running(background)
        self.assertTrue(self.d.get(background["session"])["terminal_detached"])
        terminal.send("goblins run shell --name nonterminal </dev/null; echo NO_TTY_DONE\n")
        terminal.expect(r"(?:^|\n)NO_TTY_DONE\n")
        self.assertFalse(any(r["agent_name"] == "nonterminal" for r in self.d.rpc.call("sessions.list", {})))
        terminal.send("exit\n")
        self.assertEqual(terminal.wait(), 0)
        self.d.wait(lambda: self.d.get(background["session"])["state"] == "stopped")

    def test_terminal_and_status_authorization(self):
        parent = self.d.start(agent_name="parent")
        unrelated = self.d.start(agent_name="unrelated")
        child = self.child(parent)
        for target in (parent["session"], unrelated["session"], "unrelated", "unknown"):
            for method, extra in (("sessions.attach", {}), ("sessions.resize", {"rows":40,"cols":90})):
                with self.assertRaises(ValueError) as error:
                    self.sandbox(parent["session"], method, {"session":target, **extra})
                self.assertEqual(error.exception.args[0]["code"], -32004)
        with self.assertRaises(ValueError):
            self.sandbox(parent["session"], "sessions.status", {"session":unrelated["session"]})
        status = self.sandbox(parent["session"], "sessions.status", {})
        self.assertEqual(set(status), {"id", "agent_name", "name", "description", "state", "scope", "docker_enabled",
                                       "dev_shell", "allowed_children", "raid"})
        self.assertEqual(status["allowed_children"], ["shell"])
        self.assertEqual(status["agent_name"], "parent")
        pipeline = RPC(self.d.state / parent["session"] / "resources/request.sock")
        try:
            pipeline.peer.sendall(frame({"jsonrpc":"2.0", "id":2, "method":"sessions.attach",
                                         "params":{"session":child["session"]}}) + b"injected keys")
            self.assertEqual(receive(pipeline.peer)["error"]["code"], -32600)
            self.assertFalse(self.d.get(child["session"])["terminal_attached"])
        finally:
            pipeline.close()
        peer = RPC(self.d.state / parent["session"] / "resources/request.sock")
        self.addCleanup(peer.close)
        self.assertEqual(peer.call("sessions.attach", {"session":child["session"]}), {"attached":True})
        with self.assertRaisesRegex(ValueError, "already attached"):
            self.sandbox(parent["session"], "sessions.attach", {"session":child["session"]})
        import socket
        with socket.socket(socket.AF_UNIX) as host_terminal:
            host_terminal.settimeout(5)
            host_terminal.connect(self.d.get(child["session"])["terminal"])
            self.assertIn("already attached", receive(host_terminal)["error"]["message"])
        self.assertTrue(self.sandbox(parent["session"], "sessions.resize", {"session":child["session"],"rows":40,"cols":90})["accepted"])

    def test_subtree_scope_names_configurations_and_completions(self):
        a = self.d.start(agent_name="alpha")
        b = self.d.start(agent_name="beta")
        child = self.child(a, "kid")
        other = self.child(b, "kid")
        grandchild = self.child(a, "leaf", parent="kid")
        own = self.child(a, "own", parent=".")
        listing = self.sandbox(a["session"], "sessions.list", {})
        self.assertEqual({r["id"] for r in listing}, {child["session"], grandchild["session"], own["session"]})
        self.assertEqual({r["path"] for r in listing}, {"kid", "kid/leaf", "own"})
        for record in listing:
            self.assertTrue({"configuration", "terminal", "identity", "detail"}.isdisjoint(record))
        self.assertEqual(self.d.get("alpha/kid")["id"], child["session"])
        self.assertEqual(self.d.get("beta/kid")["id"], other["session"])
        for target in (a["session"], b["session"], other["session"], "beta", "beta/kid", "absent"):
            for method in ("sessions.get", "sessions.stop"):
                with self.assertRaises(ValueError) as error:
                    self.sandbox(a["session"], method, {"session": target})
                self.assertEqual(error.exception.args[0]["code"], -32004)
        for parent in (b["session"], other["session"], "absent"):
            with self.assertRaises(ValueError) as error:
                self.child(a, parent=parent)
            self.assertEqual(error.exception.args[0]["code"], -32004)
        for params in ({"name": "codex"}, {"configuration": self.d.manifest}, {"cwd": "/tmp"}):
            with self.assertRaises(ValueError):
                self.sandbox(a["session"], "sessions.start", {"key": uuid.uuid4().hex, "name": "shell", **params})
        for method in ("permissions.list", "permissions.decide", "server.stop", "state.subscribe", "sessions.resize"):
            with self.assertRaises(ValueError):
                self.sandbox(a["session"], method, {})
        with self.assertRaises(ValueError):
            self.sandbox(a["session"], "sessions.detach", {"session": child["session"]})
        complete = self.cli("__complete-names", "--", "goblins", "run", "shell", "--parent")
        self.assertEqual(complete.returncode, 0, complete.stderr)
        self.assertIn("alpha/kid", complete.stdout.splitlines())
        shell = self.terminal(a)
        workspace = self.d.state / a["session"] / "resources/workspace"
        shell.sendall(b"goblins __complete-names -- goblins run shell --parent > /workspace/completion; goblins __complete-names -- goblins run > /workspace/configuration; complete -C 'goblins run shell --parent ' > /workspace/fish-parent; touch /workspace/done\n")
        self.d.wait(lambda: (workspace / "done").exists())
        self.assertEqual(set((workspace / "completion").read_text().splitlines()), {".", "kid", "kid/leaf", "own"})
        self.assertEqual((workspace / "configuration").read_text().strip(), "shell")
        self.assertEqual({line.split("\t")[0] for line in (workspace / "fish-parent").read_text().splitlines()}, {".", "kid", "kid/leaf", "own"})
        # Equal idempotency keys in different branches cannot retrieve a launch
        # or collide with another branch's request history.
        params = {"key": "same-key", "name": "shell", "agent_name": "repeat"}
        first = self.running(self.sandbox(a["session"], "sessions.start", params))
        self.assertEqual(self.sandbox(a["session"], "sessions.start", params), first)
        second = self.running(self.sandbox(b["session"], "sessions.start", params))
        self.assertNotEqual(first["session"], second["session"])

    def test_attached_child_reports_parent_stop(self):
        parent = self.d.start(agent_name="parent")
        child = self.child(parent)
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", child["session"]])
        self.addCleanup(terminal.close)
        self.d.wait(lambda: self.d.get(child["session"])["terminal_attached"])
        self.sandbox(parent["session"], "sessions.stop", {"session": child["session"]})
        terminal.expect(r"goblins: stopped by goblin parent")
        self.assertNotEqual(terminal.wait(), 0)
        reason = f"stopped by goblin parent ({parent['session']})"
        self.assertEqual(self.d.get(child["session"])["stop_reason"], reason)
        self.assertEqual(self.sandbox(parent["session"], "sessions.get", {"session": child["session"]})["stop_reason"], reason)

    def test_kill_guard_and_normal_exit_cascade(self):
        root = self.d.start(agent_name="root")
        unrelated = self.d.start(agent_name="unrelated")
        child = self.child(root)
        leaf = self.child(child, "leaf")
        for verb in ("kill", "stop"):
            failed = self.cli(verb, "root")
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("--kill-children", failed.stderr)
            self.assertEqual(self.d.get(child["session"])["state"], "running")
        with self.assertRaisesRegex(ValueError, "kill-children"):
            self.sandbox(root["session"], "sessions.stop", {"session": child["session"]})
        stopped = self.sandbox(root["session"], "sessions.stop", {"session": child["session"], "kill_children": True})
        self.assertTrue(stopped["accepted"])
        for entry in (child, leaf):
            self.d.wait(lambda: self.d.get(entry["session"])["state"] == "stopped")
            self.assertEqual(self.d.get(entry["session"])["stop_reason"],
                             f"stopped by goblin root ({root['session']})")
        child = self.child(root, "second")
        leaf = self.child(child, "leaf")
        pids = [self.d.get(x["session"])["identity"]["pid"] for x in (root, child, leaf)]
        self.terminal(root).sendall(b"exit\n")
        for entry in (root, child, leaf):
            self.d.wait(lambda: self.d.get(entry["session"])["state"] == "stopped")
        self.assertIsNone(self.d.get(root["session"])["stop_reason"])
        self.assertEqual(self.d.get(root["session"])["exit_code"], 0)
        for entry in (child, leaf):
            self.assertIn("stopped because owner goblin", self.d.get(entry["session"])["stop_reason"])
        self.d.wait(lambda: all(not Path(f"/proc/{pid}").exists() for pid in pids))
        self.assertEqual(self.d.get(unrelated["session"])["state"], "running")
        child = self.child(unrelated)
        success = self.cli("kill", "unrelated", "--kill-children")
        self.assertEqual(success.returncode, 0, success.stderr)
        self.d.wait(lambda: self.d.get(child["session"])["state"] == "stopped")
        for entry in (unrelated, child):
            self.assertEqual(self.d.get(entry["session"])["stop_reason"], "stopped by host")

    def test_cli_shared_workspace_and_pinned_parent_paths(self):
        with tempfile.TemporaryDirectory(prefix="goblin-tree-work-") as folder:
            original = Path(folder) / "project"
            original.mkdir()
            (original / "input").write_text("parent\n")
            result = self.cli("run", "shell", "--name", "parent", "--detatched", cwd=original)
            self.assertEqual(result.returncode, 0, result.stderr)
            parent = self.running(json.loads(result.stdout))
            moved = Path(folder) / "moved"
            original.rename(moved)
            original.mkdir()
            (original / "input").write_text("replacement-host-path\n")
            shell = self.terminal(parent)
            shell.sendall(b"goblins run shell --name child --detatched > child.json\n")
            child = self.running(self.wait_json(moved / "child.json"))
            child_shell = self.terminal(child)
            child_shell.sendall(b"cat input > seen; echo child-write > shared; goblins list > children.json\n")
            self.d.wait(lambda: (moved / "shared").exists())
            self.assertEqual((moved / "seen").read_text(), "parent\n")
            self.assertFalse((original / "shared").exists())
            host_child = self.cli("run", "shell", "--parent", "parent", "--name", "host-child", "--detatched", cwd=original)
            self.assertEqual(host_child.returncode, 0, host_child.stderr)
            launched = self.running(json.loads(host_child.stdout))
            self.terminal(launched).sendall(b"cat shared > host-child-seen; touch host-child-done\n")
            self.d.wait(lambda: (moved / "host-child-done").exists())
            self.assertEqual((moved / "host-child-seen").read_text(), "child-write\n")

    def test_snapshot_is_shared_with_children(self):
        with tempfile.TemporaryDirectory(prefix="goblin-tree-snapshot-") as folder:
            source = Path(folder)
            (source / "input").write_text("host\n")
            d = Daemon(workspace=source)
            self.addCleanup(d.close)
            self.d = d
            parent = d.start()
            child = self.child(parent)
            root_workspace = d.state / parent["session"] / "resources/workspace"
            self.terminal(child).sendall(b"echo child > input; echo done > marker\n")
            d.wait(lambda: (root_workspace / "marker").exists())
            self.assertEqual((root_workspace / "input").read_text(), "child\n")
            self.assertEqual((source / "input").read_text(), "host\n")

    def test_children_reuse_loaded_configuration_after_manifest_changes(self):
        manifest = Path(self.d.temp.name) / "mutable.json"
        configuration = json.loads(Path(self.d.manifest).read_text())
        manifest.write_text(json.dumps(configuration))
        parent = self.d.start(manifest=manifest)
        # If a child reopened this manifest, it would fail to launch. Both host
        # and sandbox child launches must instead use the parent's capability.
        manifest.write_text("invalid replacement manifest")
        child = self.child(parent)
        self.assertEqual(self.d.get(child["session"])["configuration"], str(manifest))
        sibling = self.d.rpc.call("sessions.start", {
            "key": "host-child", "configuration": self.d.manifest, "name": "shell",
            "parent": parent["session"], "rows": 24, "cols": 80,
        })
        self.running(sibling)
        with self.assertRaisesRegex(ValueError, "'codex' is not an allowed child of 'shell'; allowed: shell"):
            self.d.rpc.call("sessions.start", {
                "key": "different", "configuration": self.d.manifest, "name": "codex",
                "parent": parent["session"], "rows": 24, "cols": 80,
            })

    def test_package_approvals_flow_downward_only_on_request(self):
        root = self.d.start()
        child = self.child(root)
        sibling = self.child(root, "sibling")
        leaf = self.child(child, "leaf")
        request, permission = self.d.pending(child["session"])
        self.addCleanup(request.close)
        self.d.decide(permission, True)
        request.peer.settimeout(120)
        self.assertEqual(receive(request.peer)["result"]["status"], "ready")
        self.assertEqual(self.d.get(root["session"])["packages"], [])
        self.assertEqual(self.d.get(sibling["session"])["packages"], [])
        self.assertEqual(self.d.get(leaf["session"])["packages"], [])
        # A deeper descendant can use the approval even before its immediate
        # parent has requested (and mounted) that package.
        deeper = self.child(leaf, "deeper")
        for descendant in (deeper, leaf):
            peer = RPC(self.d.state / descendant["session"] / "resources/request.sock")
            self.addCleanup(peer.close)
            peer.peer.settimeout(120)
            result = peer.call("permissions.request", {"kind": "package", "package": "hello", "reason": "inherit"})
            self.assertEqual(result["status"], "ready")
            record = self.d.rpc.call("permissions.get", {"request": result["request"]})
            self.assertTrue(record["approved"])
        for unapproved in (root, sibling):
            peer, record = self.d.pending(unapproved["session"])
            self.addCleanup(peer.close)
            self.assertIsNone(record["approved"])
            self.d.decide(record, False)
            self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        # Existing pending requests become eligible when an ancestor gains it.
        pending, waiting = self.d.pending(leaf["session"], "cowsay")
        self.addCleanup(pending.close)
        parent_request, parent_permission = self.d.pending(root["session"], "cowsay")
        self.addCleanup(parent_request.close)
        self.d.decide(parent_permission, True)
        parent_request.peer.settimeout(120)
        pending.peer.settimeout(120)
        self.assertEqual(receive(parent_request.peer)["result"]["status"], "ready")
        self.assertEqual(receive(pending.peer)["result"]["status"], "ready")
        self.assertEqual(self.d.get(child["session"])["packages"], ["hello"])


def build(root, check):
    return Path(command(["nix", "build", "--print-out-paths", "--out-link", root / check,
                         f"path:{ROOT}#checks.x86_64-linux.{check}"])) / "bin/goblins"


class ChildrenFixture(unittest.TestCase):
    """Children using other configurations (checks.nix children-goblins)."""

    @classmethod
    def setUpClass(cls):
        cls.builds = tempfile.TemporaryDirectory(prefix="goblins-children-build-")
        cls.app = build(Path(cls.builds.name), "children-goblins")

    @classmethod
    def tearDownClass(cls):
        cls.builds.cleanup()

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblins-children-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        (self.root / "reviewer").mkdir()
        self.d = Daemon(self.app, env={**os.environ, "GOBLINS_TEST_ROOT": str(self.root),
                                       "XDG_CACHE_HOME": str(self.root / "cache")})
        self.addCleanup(self.d.close)
        self.terminals = {}

    sandbox = OwnershipTests.sandbox
    running = OwnershipTests.running

    def terminal(self, launch):
        # One attachment per sandbox, reused by later commands.
        if launch["session"] not in self.terminals:
            self.terminals[launch["session"]] = OwnershipTests.terminal(self, launch)
        return self.terminals[launch["session"]]

    def launch(self, owner, name, agent_name=None, wait=True, **extra):
        launch = self.sandbox(owner["session"], "sessions.start", {
            "key": uuid.uuid4().hex, "name": name, "agent_name": agent_name or name, **extra})
        return self.running(launch) if wait else launch

    def refused(self, owner, name, message, **extra):
        with self.assertRaises(ValueError) as error:
            self.launch(owner, name, **extra)
        self.assertEqual(error.exception.args[0]["code"], -32602)
        self.assertEqual(error.exception.args[0]["message"], message)

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


class AllowedChildrenTests(ChildrenFixture):
    def test_child_runs_its_own_configuration_in_the_parents_workspace(self):
        root = self.start_root()
        root_status = self.sandbox(root["session"], "sessions.status", {})
        self.assertEqual(root_status["allowed_children"], ["coordinator", "reviewer", "shell", "broken", "unscoped"])
        child = self.launch(root, "reviewer")
        record = self.d.get(child["session"])
        self.assertEqual((record["name"], record["parent"]), ("reviewer", root["session"]))
        self.assertEqual(record["description"], "reviewer child configuration")
        # The record keeps the parent's manifest: the snapshot's origin.
        self.assertEqual(record["configuration"], self.d.get(root["session"])["configuration"])
        status = self.sandbox(child["session"], "sessions.status", {})
        self.assertEqual((status["name"], status["allowed_children"]), ("reviewer", []))
        # The root launch retained the permitted configuration's build spec.
        spec = json.loads(Path(self.d.manifest).read_text())["goblins"]["reviewer"]["build_spec"]
        self.assertTrue((self.d.state / root["session"] / "resources/roots" / Path(spec).name).is_symlink())
        writable = self.root / "reviewer"
        self.run_in(child, f"printf '%s' \"$CHILD_MARKER\" > /workspace/marker; cat /etc/children-test.conf > /workspace/etc; "
                           f"echo child > {writable}/from-child; echo $? > /workspace/child-rw", "child-done")
        self.assertEqual((self.workspace / "marker").read_text(), "reviewer")
        self.assertEqual((self.workspace / "etc").read_text(), "reviewer\n")
        self.assertEqual((self.workspace / "child-rw").read_text(), "0\n")
        self.assertEqual((writable / "from-child").read_text(), "child\n")
        # The parent gains none of the child's declared access.
        self.run_in(root, f"printf '%s' \"$CHILD_MARKER\" > /workspace/root-marker; cat /etc/children-test.conf > /workspace/root-etc; "
                          f"test -e {writable}; echo $? > /workspace/root-rw", "root-done")
        self.assertEqual((self.workspace / "root-marker").read_text(), "coordinator")
        self.assertEqual((self.workspace / "root-etc").read_text(), "coordinator\n")
        self.assertEqual((self.workspace / "root-rw").read_text(), "1\n")
        self.run_in(root, "goblins status > /workspace/status; goblins __complete-names -- goblins run > /workspace/complete",
                    "status-done")
        self.assertIn("Configuration: coordinator\nDescription: coordinator child configuration\n"
                      "Allowed children: coordinator, reviewer, shell, broken, unscoped\n",
                      (self.workspace / "status").read_text())
        self.assertEqual((self.workspace / "complete").read_text().split(),
                         ["coordinator", "reviewer", "shell", "broken", "unscoped"])
        # A missing bind source of a permitted configuration fails only that child.
        broken = self.failed(self.launch(root, "broken", wait=False))
        self.assertIn("host bind", broken["detail"])
        self.assertEqual(self.d.get(root["session"])["state"], "running")
        # Parent exit stops a different-configuration child.
        self.terminal(root).sendall(b"exit\n")
        self.d.wait(lambda: self.d.get(child["session"])["state"] == "stopped")
        self.assertIn("stopped because owner goblin", self.d.get(child["session"])["stop_reason"])

    def test_each_parent_uses_its_own_list(self):
        root = self.start_root()
        self.refused(root, "unlisted", "configuration 'unlisted' is not an allowed child of 'coordinator'; "
                                       "allowed: coordinator, reviewer, shell, broken, unscoped")
        reviewer = self.launch(root, "reviewer")
        # An empty list rejects even the parent's own configuration.
        self.refused(reviewer, "reviewer", "configuration 'reviewer' permits no children")
        shell = self.launch(root, "shell")
        leaf = self.launch(shell, "reviewer", "leaf")
        self.assertEqual(self.d.get(leaf["session"])["parent"], shell["session"])
        refused = "configuration 'coordinator' is not an allowed child of 'shell'; allowed: reviewer"
        self.refused(shell, "coordinator", refused)
        # Selecting a descendant uses its list, from the root and from the host.
        self.refused(root, "coordinator", refused, parent="shell")
        with self.assertRaises(ValueError) as error:
            self.host_start("coordinator", parent=shell["session"])
        self.assertEqual(error.exception.args[0]["message"], refused)
        self.running(self.host_start("reviewer", parent=shell["session"], agent_name="host-leaf"))
        self.assertEqual(self.launch(root, "coordinator", "same")["path"], "same")

    def test_child_shares_the_parents_scope(self):
        launch = self.host_start("coordinator", scope="work", agent_name="root")
        root = self.running(launch)
        self.workspace = self.d.state / root["session"] / "resources/workspace"
        child = self.launch(root, "reviewer")
        self.assertEqual(self.d.get(child["session"])["scope"], "work")
        self.refused(root, "unscoped", "configuration 'unscoped' cannot be launched here: "
                                       "'unscoped' does not allow scope 'work'")
        # Members share the scope's namespaces: the child sees the root's processes.
        self.run_in(root, "sleep 300 & echo $! > /workspace/root-sleep", "root-done")
        pid = (self.workspace / "root-sleep").read_text().strip()
        self.run_in(child, f"test -e /proc/{pid}; echo $? > /workspace/seen", "child-done")
        self.assertEqual((self.workspace / "seen").read_text(), "0\n")

    def test_child_shares_the_parents_dev_shell_generation(self):
        from test_devshell import FLAKE
        nixpkgs = command(["nix", "eval", "--impure", "--raw", "--expr",
                           f'(builtins.getFlake "path:{ROOT}").inputs.nixpkgs.outPath'])
        project = self.root / "project"
        project.mkdir()
        flake = lambda marker: (project / "flake.nix").write_text(
            FLAKE.replace("NIXPKGS", nixpkgs).replace("@MARKER@", marker))
        flake("original")
        command(["nix", "flake", "lock", f"path:{project}"])
        command(["git", "-C", str(project), "init", "-q"])
        command(["git", "-C", str(project), "add", "flake.nix", "flake.lock"])
        cli = lambda *args: subprocess.run([str(self.app), "--state-dir", str(self.d.state), "run", *args,
                                            "--detached"], text=True, capture_output=True, cwd=project)
        result = cli("coordinator", "--dev-shell", ".", "--name", "root")
        self.assertEqual(result.returncode, 0, result.stderr)
        root = json.loads(result.stdout)
        # Evaluating the dev shell can take a while on a cold store.
        record = self.d.wait(lambda: (r if (r := self.d.get(root["session"]))["state"] != "starting" else None),
                             timeout=600)
        self.assertEqual(record["state"], "running", record)
        self.workspace = self.d.state / root["session"] / "resources/workspace"
        # The child gets the parent's current generation, not the edited flake.
        flake("edited")
        result = cli("reviewer", "--parent", "root", "--name", "child")
        self.assertEqual(result.returncode, 0, result.stderr)
        child = self.running(json.loads(result.stdout))
        self.assertEqual(self.d.get(child["session"])["dev_shell"], str(project))
        self.run_in(child, "printf '%s %s' \"$DEV_MARKER\" \"$CHILD_MARKER\" > /workspace/marker; pwd > /workspace/cwd",
                    "child-done")
        self.assertEqual((self.workspace / "marker").read_text(), "original reviewer")
        # It also shares the parent's pinned working directory.
        self.assertEqual((self.workspace / "cwd").read_text(), f"{project}\n")

    def test_parent_approvals_reach_a_different_configuration_on_request(self):
        root = self.start_root()
        child = self.launch(root, "reviewer")
        request, permission = self.d.pending(root["session"])
        self.addCleanup(request.close)
        self.d.decide(permission, True)
        request.peer.settimeout(120)
        self.assertEqual(receive(request.peer)["result"]["status"], "ready")
        self.assertEqual(self.d.get(child["session"])["packages"], [])
        peer = RPC(self.d.state / child["session"] / "resources/request.sock")
        self.addCleanup(peer.close)
        peer.peer.settimeout(120)
        result = peer.call("permissions.request", {"kind": "package", "package": "hello", "reason": "inherit"})
        self.assertEqual(result["status"], "ready")
        self.assertTrue(self.d.rpc.call("permissions.get", {"request": result["request"]})["approved"])
        self.assertEqual(self.d.get(child["session"])["packages"], ["hello"])

    def test_branch_keeps_its_snapshot_after_the_manifest_changes(self):
        manifest = self.root / "mutable.json"
        manifest.write_text(Path(self.d.manifest).read_text())
        root = self.start_root(manifest=manifest)
        manifest.write_text("invalid replacement manifest")
        child = self.launch(root, "reviewer")
        self.assertEqual(self.d.get(child["session"])["configuration"], str(manifest))
        self.running(self.host_start("shell", parent=root["session"], agent_name="host-child"))

    def test_host_cli_parent_launch_uses_the_snapshot_after_a_rebuild(self):
        updated = build(Path(self.builds.name), "children-goblins-updated")
        root = self.start_root()
        cli = lambda *args: subprocess.run([str(updated), "--state-dir", str(self.d.state), *args],
                                           text=True, capture_output=True)
        # The new build no longer declares reviewer; the root's snapshot does.
        result = cli("run", "reviewer", "--parent", "root", "--name", "late", "--detached")
        self.assertEqual(result.returncode, 0, result.stderr)
        child = self.running(json.loads(result.stdout))
        self.assertEqual(self.d.get(child["session"])["name"], "reviewer")
        # Never permitted: rejected by the daemon, not the newer manifest.
        result = cli("run", "unlisted", "--parent", "root", "--detached")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("'unlisted' is not an allowed child of 'coordinator'", result.stderr)
        # Without --parent the installed build still decides.
        result = cli("run", "reviewer", "--detached")
        self.assertIn("unknown configuration 'reviewer'", result.stderr)

    def test_retained_roots_cover_a_permitted_configuration_after_a_rebuild(self):
        manifest = json.loads(Path(self.d.manifest).read_text())
        root = self.start_root()
        # A rebuild no longer declares reviewer, so only the branch's roots
        # keep its store paths from garbage collection.
        updated = build(Path(self.builds.name), "children-goblins-updated")
        runtime = next(word for word in updated.read_text().split() if word.endswith("-goblins-config.json"))
        self.assertNotIn("reviewer", json.loads(Path(runtime).read_text())["goblins"])
        top = lambda path: str(Path(*Path(path).parts[:4]))

        def needed(name):
            config = manifest["goblins"][name]
            spec = json.loads(Path(config["build_spec"]).read_text())
            return {
                "executable": {top(spec["sandboxed_binary"])},
                "closure": set(Path(spec["closure_paths_file"]).read_text().split()),
                "helper": {top(config["helper"])},
                "client": {top(config["client_package"])},
                "injected files": {top(p) for p in [*config["sandbox_etc"].values(),
                                                    *config["dev_shell_sandbox_etc"].values()]},
            }
        reviewer = needed("reviewer")
        self.assertTrue(reviewer["closure"] and reviewer["injected files"])
        # The root's own roots cover what the two configurations share, so
        # paths only the reviewer needs are what show the branch retained it.
        own = set().union(*needed("coordinator").values())
        self.assertTrue(reviewer["closure"] - own and reviewer["injected files"] - own)
        roots = self.d.state / root["session"] / "resources/roots"
        retained = set(command(["nix-store", "--query", "--requisites",
                                *(os.readlink(link) for link in roots.iterdir())]).split())
        for kind, paths in reviewer.items():
            self.assertLessEqual(paths, retained, f"{kind} not retained by the branch's roots")
        # The retained configuration still launches from the parent's snapshot.
        self.assertEqual(self.d.get(self.launch(root, "reviewer")["session"])["name"], "reviewer")

    def test_daemon_rejects_an_allowed_children_list_over_the_bound(self):
        manifest = self.root / "oversized.json"
        configuration = json.loads(Path(self.d.manifest).read_text())
        configuration["goblins"]["coordinator"]["allowed_children"] = [f"g{i}" for i in range(65)]
        manifest.write_text(json.dumps(configuration))
        record = self.failed(self.d.start("coordinator", manifest=manifest, wait=False))
        self.assertIn("invalid allowed_children for 'coordinator'", record["detail"])


class AllowedChildrenDockerTests(ChildrenFixture):
    """Docker attachment of different-configuration children (rootless Docker)."""

    def test_child_attachment_follows_parent_or_own_definition(self):
        attached = self.start_root("attached")
        self.assertTrue(self.d.get(attached["session"])["docker_enabled"])
        plain = self.launch(attached, "plain")
        self.assertTrue(self.d.get(plain["session"])["docker_enabled"])
        # Both reach the scope's one engine.
        self.run_in(attached, "docker info --format '{{.ID}}' > /workspace/parent-engine", "parent-done")
        self.run_in(plain, "docker info --format '{{.ID}}' > /workspace/child-engine", "child-done")
        engine = (self.workspace / "parent-engine").read_text()
        self.assertTrue(engine.strip())
        self.assertEqual((self.workspace / "child-engine").read_text(), engine)
        # One engine cannot mount two branches' different /workspace sources.
        self.d.rpc.call("sessions.stop", {"session": attached["session"], "kill_children": True})
        for launch in (attached, plain):
            self.d.wait(lambda: self.d.get(launch["session"])["state"] == "stopped")
        detached = self.d.start("detached", agent_name="other")
        self.assertFalse(self.d.get(detached["session"])["docker_enabled"])
        eager = self.launch(detached, "eager")
        self.assertTrue(self.d.get(eager["session"])["docker_enabled"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
