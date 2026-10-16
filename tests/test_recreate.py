"""Recreating goblins from their records through a real daemon.

Records are written into a private XDG_STATE_HOME before the daemon starts,
so each test chooses exactly what was recorded. No coding agents are used:
the demo shell configuration stands in, with probe arguments.
"""
import json
from pathlib import Path
import re
import socket
import tempfile
import time
import unittest
import uuid

from daemon_support import Daemon, RPC, receive
from support import command
from daemon_support import ROOT

FRESH = ["--no-config", "-c", "printf 'ARGS=fresh\\n'; exec sleep 600"]
RESUME = ["--no-config", "-c",
          "printf 'RESUMED=%s\\n' {session_id}; printf 'FIRST=%s\\n' (command -v hello); "
          "printf 'COWSAY=%s\\n' (readlink /run/goblins/packages/current/bin/cowsay); "
          "printf 'COWTHINK=%s\\n' (readlink /run/goblins/packages/current/bin/cowthink); exec sleep 600"]


def record(id, configuration="shell", agent_name="morb", **extra):
    value = {"version": 1, "id": id, "configuration": configuration, "agent_name": agent_name, "cwd": None,
             "scope": None, "dev_shell": None, "docker_enabled": False, "packages": {"approved": [], "denied": []},
             "variables": {}, "recreated_from": None, "skipped": [], "created": 1, "updated": 1, "stopped": 2}
    value.update(extra)
    return value


def checks_manifest(name):
    app = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                        f"path:{ROOT}#checks.x86_64-linux.{name}"])) / "bin/goblins"
    return re.search(r'exec \S+ --runtime (\S+)', app.read_text()).group(1)


class RecreateTests(unittest.TestCase):
    def daemon(self, *records, **options):
        home = tempfile.TemporaryDirectory(prefix="goblins-recreate-")
        self.addCleanup(home.cleanup)
        store = Path(home.name) / "goblins/sessions"
        store.mkdir(parents=True, mode=0o700)
        for r in records:
            (store / f"{r['id']}.json").write_text(json.dumps(r))
        d = Daemon(state_home=home.name, **options)
        self.addCleanup(d.close)
        return d

    def probe_manifest(self, d):
        """The demo manifest with a shell that reports its first view."""
        config = json.loads(Path(d.manifest).read_text())
        config["goblins"]["shell"]["args"] = FRESH
        config["goblins"]["shell"]["resume_command"] = RESUME
        path = Path(d.temp.name) / "probe.json"
        path.write_text(json.dumps(config))
        return str(path)

    def recreate(self, d, target, manifest=None, wait="running", **extra):
        launch = d.rpc.call("sessions.start", {"key": uuid.uuid4().hex, "configuration": manifest or d.manifest,
                                               "recreate": target, "rows": 24, "cols": 100, "detached": True,
                                               **extra})
        if wait:
            r = d.wait(lambda: (r if (r := d.get(launch["session"]))["state"] != "starting" else None), timeout=180)
            self.assertEqual(r["state"], wait, r)
        return launch

    def refused(self, d, target, **extra):
        with self.assertRaises(ValueError) as error:
            self.recreate(d, target, wait=None, **extra)
        return error.exception.args[0]["message"]

    def output(self, d, launch, pattern, timeout=60):
        with d.terminal(launch) as peer:
            data = b""
            end = time.monotonic() + timeout
            while not re.search(pattern, data.replace(b"\r", b"")):
                if time.monotonic() > end:
                    self.fail(repr(data))
                try:
                    data += peer.recv(8192)
                except (TimeoutError, socket.timeout):
                    pass
            return data.replace(b"\r", b"").decode()

    def exec(self, d, session, script):
        return d.cli("exec", session, "--", "sh", "-c", script, check=False)

    def test_first_command_sees_recreated_grants_in_order_and_resumes(self):
        source = record("rec-a", variables={"session_id": "c8193c93", "session_name": "Plan it"},
                        packages={"approved": ["cowsay", "neo-cowsay", "hello", "no-such-goblins-package"],
                                  "denied": ["nmap"]})
        d = self.daemon(source)
        manifest = self.probe_manifest(d)
        launch = self.recreate(d, "rec-a", manifest)
        self.assertEqual(launch["agent_name"], "morb")
        text = self.output(d, launch, rb"COWTHINK=\S+\n")
        self.assertIn("RESUMED=c8193c93\n", text)
        self.assertIn("FIRST=/run/goblins/packages/current/bin/hello\n", text)
        self.assertNotIn("ARGS=fresh", text)
        # The earlier grant wins a collision, as live grants do.
        self.assertRegex(text, r"COWSAY=/nix/store/[a-z0-9]{32}-cowsay-[^/]*/bin/cowsay\n")
        self.assertRegex(text, r"COWTHINK=/nix/store/[a-z0-9]{32}-cowsay-[^/]*/bin/cowthink\n")
        session = launch["session"]
        r = d.get(session)
        self.assertEqual(r["packages"], ["cowsay", "neo-cowsay", "hello"])
        self.assertEqual((r["skipped"], r["recreated_from"], r["detail"]), (["no-such-goblins-package"], "rec-a", None))
        # Its own record: the same lists and variables, the skipped name kept.
        own = d.wait(lambda: d.record(session))
        self.assertEqual(own["packages"], source["packages"])
        self.assertEqual(own["variables"], source["variables"])
        self.assertEqual((own["skipped"], own["recreated_from"]), (["no-such-goblins-package"], "rec-a"))
        self.assertEqual(d.record("rec-a"), source)
        listing = d.cli("history").stdout
        line = next(l for l in listing.splitlines() if l.startswith(session))
        self.assertIn("Plan it", line)
        self.assertIn("no-such-goblins-package", line)
        self.assertRegex(line, r"\byes\b")
        records = json.loads(d.cli("history", "--json").stdout)
        self.assertEqual([r["id"] for r in records], [session, "rec-a"])
        self.assertEqual([r["running"] for r in records], [True, False])
        # Without a daemon the CLI reads the store under its lock.
        d.rpc.call("sessions.stop", {"session": session})
        d.wait(lambda: d.record(session)["stopped"])
        d.process.terminate(); d.process.wait(timeout=10)
        offline = json.loads(d.cli("history", "--json").stdout)
        self.assertEqual([(r["id"], r["running"]) for r in offline], [(session, False), ("rec-a", False)])

    def test_a_missing_variable_or_an_option_like_value_falls_back_to_args(self):
        d = self.daemon(record("rec-missing", agent_name="snaga"),
                        record("rec-option", agent_name="grub", variables={"session_id": "--dangerous"}))
        manifest = self.probe_manifest(d)
        for id, reason in [("rec-missing", "variable 'session_id' is not recorded"),
                           ("rec-option", "variable 'session_id' starts with '-'")]:
            launch = self.recreate(d, id, manifest)
            self.output(d, launch, rb"ARGS=fresh\n")
            self.assertIn(reason, d.get(launch["session"])["detail"])

    def test_changed_configuration_failed_start_and_lineage_reservations(self):
        cwd = tempfile.TemporaryDirectory(prefix="goblins-recreate-cwd-"); self.addCleanup(cwd.cleanup)
        d = self.daemon(record("rec-f", configuration="fishy", cwd=cwd.name),
                        record("rec-gone", cwd="/nonexistent-goblins-recreate"))
        # The demo manifest has no 'fishy': the start fails and releases it.
        failed = self.recreate(d, "rec-f", wait="failed")
        self.assertIn("unknown goblin", d.get(failed["session"])["detail"])
        updated = checks_manifest("updated-goblins")
        first = self.recreate(d, "rec-f", updated)["session"]
        self.assertEqual(self.exec(d, first, 'printf "%s %s" "$GOBLIN_MARKER" "$(pwd -P)"').stdout,
                         f"updated {Path(cwd.name).resolve()}")
        # Concurrent recreates of the record, or of the live goblin's own.
        self.assertIn("recreated from rec-f is live", self.refused(d, "rec-f"))
        own = d.wait(lambda: d.record(first))
        self.assertIn("is live", self.refused(d, first))
        d.rpc.call("sessions.stop", {"session": first})
        d.wait(lambda: d.get(first)["state"] == "stopped")
        # Transitive: the source is refused while a later generation lives.
        second = self.recreate(d, own["id"], updated)["session"]
        self.assertEqual(d.wait(lambda: d.record(second))["recreated_from"], first)
        self.assertIn(f"{first} was recreated from it", self.refused(d, "rec-f"))
        self.assertIn("no longer exists", self.refused(d, "rec-gone"))
        self.assertIn("--parent", self.refused(d, "rec-gone", parent=second))
        self.assertIn("no recorded goblin", self.refused(d, "nobody"))

    def test_children_neither_inherit_nor_mount_recreated_grants(self):
        d = self.daemon(record("rec-c", packages={"approved": ["hello"], "denied": []}))
        manifest = self.probe_manifest(d)
        root = self.recreate(d, "rec-c", manifest)["session"]
        hello = self.exec(d, root, "readlink /run/goblins/packages/current/bin/hello").stdout.strip()
        self.assertRegex(hello, r"^/nix/store/.*-hello-.*/bin/hello$")
        peer = RPC(d.state / root / "resources/request.sock")
        child = peer.call("sessions.start", {"key": "child", "name": "shell", "agent_name": "kid", "detached": True})
        peer.close()
        d.wait(lambda: d.get(child["session"])["state"] == "running", timeout=60)
        # Same configuration: its normal arguments, no recorded grant mounted.
        self.output(d, d.get(child["session"]), rb"ARGS=fresh\n")
        self.assertEqual(self.exec(d, child["session"],
                                   f"test -e /run/goblins/packages/current/bin/hello || test -e {hello}; echo $?").stdout,
                         "1\n")
        # Its request reuses the parent's exact output without asking the host.
        peer = RPC(d.state / child["session"] / "resources/request.sock")
        peer.peer.settimeout(60)
        peer.send("permissions.request", {"kind": "package", "package": "hello", "reason": "test"})
        self.assertEqual(receive(peer.peer)["result"]["status"], "ready")
        peer.close()
        self.assertEqual(self.exec(d, child["session"], "readlink /run/goblins/packages/current/bin/hello").stdout.strip(),
                         hello)
        self.assertIsNone(d.record(child["session"]))

    def test_cli_recreate_and_a_workspace_server_refuses(self):
        d = self.daemon(record("rec-cli", agent_name="cli-goblin"))
        result = d.cli("recreate", "--detached", "cli-goblin")
        launch = json.loads(result.stdout)
        self.assertEqual(launch["agent_name"], "cli-goblin")
        d.wait(lambda: d.get(launch["session"])["state"] == "running", timeout=60)
        self.assertEqual(d.get(launch["session"])["recreated_from"], "rec-cli")
        refused = d.cli("recreate", "--detached", "rec-cli", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("is live", refused.stderr)
        # Two records carry the name now; the attaching form takes an id.
        d.rpc.call("sessions.stop", {"session": launch["session"]})
        d.wait(lambda: (d.record(launch["session"]) or {}).get("stopped"))
        self.assertIn("2 records are named", d.cli("recreate", "--detached", "cli-goblin", check=False).stderr)
        from terminal_support import Terminal
        attached = Terminal([d.binary, "--runtime", d.manifest, "--state-dir", str(d.state), "recreate",
                             launch["session"]], env=d.env)
        self.addCleanup(attached.close)
        attached.expect(f"Recreated cli-goblin from {launch['session']} as ", timeout=60)
        attached.send("exit\r")
        self.assertEqual(attached.wait(), 0)
        workspace = tempfile.TemporaryDirectory(prefix="goblins-recreate-ws-"); self.addCleanup(workspace.cleanup)
        snapshot = self.daemon(record("rec-ws"), workspace=workspace.name)
        self.assertIn("--workspace", self.refused(snapshot, "rec-ws"))
        # Its own goblins are not recorded either.
        session = snapshot.start()["session"]
        time.sleep(1)
        self.assertIsNone(snapshot.record(session))

if __name__ == "__main__": unittest.main(verbosity=2)
