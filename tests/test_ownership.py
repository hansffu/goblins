"""Ownership boundaries through real controller sockets, CLIs and namespaces."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from daemon_support import Daemon, RPC, frame, receive
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
        terminal.expect(r"Sandbox: parent\nConfiguration: shell\n")
        terminal.send("goblins run shell --name child; printf 'CHILD_EXIT=%s\\n' $status\n")
        child = self.d.wait(lambda: next((r for r in self.d.rpc.call("sessions.list", {})
                                         if r["agent_name"] == "child" and r["state"] == "running"), None))
        self.d.wait(lambda: self.d.get(child["id"])["terminal_attached"])
        terminal.send("goblins status\n")
        terminal.expect(r"Sandbox: child\nConfiguration: shell\n")
        self.assertEqual(self.sandbox(child["id"], "sessions.status", {})["agent_name"], "child")
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
        self.assertEqual(set(status), {"id", "agent_name", "name", "state"})
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
        child = self.child(root, "second")
        leaf = self.child(child, "leaf")
        pids = [self.d.get(x["session"])["identity"]["pid"] for x in (root, child, leaf)]
        self.terminal(root).sendall(b"exit\n")
        for entry in (root, child, leaf):
            self.d.wait(lambda: self.d.get(entry["session"])["state"] == "stopped")
        self.d.wait(lambda: all(not Path(f"/proc/{pid}").exists() for pid in pids))
        self.assertEqual(self.d.get(unrelated["session"])["state"], "running")
        child = self.child(unrelated)
        success = self.cli("kill", "unrelated", "--kill-children")
        self.assertEqual(success.returncode, 0, success.stderr)
        self.d.wait(lambda: self.d.get(child["session"])["state"] == "stopped")

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
        with self.assertRaisesRegex(ValueError, "parent's configuration"):
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
