"""Recreating goblins from their records through a real daemon.

Records are written into a private XDG_STATE_HOME before the daemon starts,
so each test chooses exactly what was recorded. No coding agents are used:
the demo shell configuration stands in, with probe arguments.
"""
import json
import os
from pathlib import Path
import re
import shutil
import sys
import socket
import tempfile
import time
import unittest
import uuid

from daemon_support import Daemon, RPC, receive
from support import FLAKE, command
from daemon_support import ROOT

FRESH = ["--no-config", "-c", "printf 'ARGS=fresh\\n'; exec sleep 600"]
RESUME = ["--no-config", "-c",
          "printf 'RESUMED=%s\\n' {session_id}; printf 'FIRST=%s\\n' (command -v hello); "
          "printf 'COWSAY=%s\\n' (readlink /run/goblins/packages/current/bin/cowsay); "
          "printf 'COWTHINK=%s\\n' (readlink /run/goblins/packages/current/bin/cowthink); exec sleep 600"]


def record(id, configuration="shell", agent_name="morb", **extra):
    value = {"version": 1, "id": id, "configuration": configuration, "agent_name": agent_name, "cwd": None,
             "scope": None, "dev_shell": None, "docker_enabled": False, "packages": {"approved": [], "denied": []},
             "variables": {}, "sessions": [id], "skipped": [], "created": 1, "updated": 1, "stopped": 2}
    value.update(extra)
    return value


def checks_manifest(name):
    app = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                        f"{FLAKE}#checks.x86_64-linux.{name}"])) / "bin/goblins"
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
        self.assertEqual(([s["name"] for s in r["skipped"]], r["recreated_from"], r["detail"]),
                         (["no-such-goblins-package"], "rec-a", None))
        self.assertIn("does not provide attribute", r["skipped"][0]["reason"])
        # The record continues: same lists and variables, the skipped name
        # kept, and this goblin added to the goblins it ran as.
        own = d.wait(lambda: (r if (r := d.record("rec-a"))["sessions"] == ["rec-a", session] else None))
        self.assertEqual(own["packages"], source["packages"])
        self.assertEqual(own["variables"], source["variables"])
        self.assertEqual((own["skipped"], own["stopped"]), (r["skipped"], None))
        self.assertFalse((d.history / f"{session}.json").exists())
        listing = d.cli("history", "--no-pager").stdout
        line = next(l for l in listing.splitlines() if l.startswith("rec-a"))
        self.assertIn("Plan it", line)
        self.assertIn("no-such-goblins-package", line)
        self.assertRegex(line, r"\byes\b")
        records = json.loads(d.cli("history", "--json").stdout)
        self.assertEqual([(r["id"], r["running"]) for r in records], [("rec-a", True)])
        d.rpc.call("sessions.stop", {"session": session})
        d.wait(lambda: d.record("rec-a")["stopped"])
        d.process.terminate(); d.process.wait(timeout=10)
        # Without a daemon the CLI only reads; whether they run is unknown.
        offline = json.loads(d.cli("history", "--json").stdout)
        self.assertEqual([(r["id"], r["running"]) for r in offline], [("rec-a", None)])
        self.assertRegex(d.cli("history").stdout, r"(?m)^rec-a .* \? ")

    def test_packages_that_fail_are_skipped_with_reasons_and_tried_again(self):
        bindir = Path(tempfile.mkdtemp(prefix="goblins-fake-nix-"))
        self.addCleanup(shutil.rmtree, bindir)
        offline = bindir / "offline"
        offline.touch()
        real = shutil.which("nix")
        # While 'offline' exists, evaluating 'cowsay' fails like a download.
        (bindir / "nix").write_text(f"""#!{sys.executable}
import os, sys
if os.path.exists({str(offline)!r}) and "eval" in sys.argv and any(a.endswith(".cowsay") for a in sys.argv):
    sys.stderr.write("error: unable to download 'https://cache.example/x.narinfo': Couldn't resolve host name (6)\\n")
    sys.exit(1)
os.execv({real!r}, [{real!r}, *sys.argv[1:]])
""")
        (bindir / "nix").chmod(0o700)
        # unrar is unfree, which the pinned catalog refuses to evaluate.
        approved = ["unrar", "hello", "cowsay"]
        d = self.daemon(record("rec-best", packages={"approved": approved, "denied": []}),
                        env={**os.environ, "PATH": f"{bindir}:{os.environ['PATH']}"})
        session = self.recreate(d, "rec-best")["session"]
        r = d.get(session)
        self.assertEqual(r["packages"], ["hello"])
        reasons = {s["name"]: s["reason"] for s in r["skipped"]}
        self.assertEqual(list(reasons), ["unrar", "cowsay"])
        self.assertIn("unfree", reasons["unrar"])
        self.assertIn("unable to download", reasons["cowsay"])
        self.assertEqual(self.exec(d, session, "test -x /run/goblins/packages/current/bin/hello; echo $?").stdout, "0\n")
        # Skipped packages stay approved, so the next recreate tries again.
        own = d.wait(lambda: (r if session in (r := d.record("rec-best"))["sessions"] else None))
        self.assertEqual(own["packages"]["approved"], approved)
        self.assertEqual(own["skipped"], r["skipped"])
        d.rpc.call("sessions.stop", {"session": session})
        d.wait(lambda: d.record("rec-best")["stopped"])
        offline.unlink()
        again = d.get(self.recreate(d, session)["session"])
        self.assertEqual(again["packages"], ["hello", "cowsay"])
        self.assertEqual([s["name"] for s in again["skipped"]], ["unrar"])

    def test_docker_the_configuration_no_longer_offers_is_skipped(self):
        d = self.daemon(record("rec-docker", docker_enabled=True))
        session = self.recreate(d, "rec-docker")["session"]
        r = d.get(session)
        self.assertFalse(r["docker_enabled"])
        self.assertEqual(r["skipped"], [{"name": "docker",
                                         "reason": "the configuration runs without a scope, so without Docker"}])
        # Still recorded, so a later recreate tries again.
        d.wait(lambda: session in d.record("rec-docker")["sessions"])
        self.assertTrue(d.record("rec-docker")["docker_enabled"])

    def test_a_missing_variable_or_an_option_like_value_falls_back_to_args(self):
        d = self.daemon(record("rec-missing", agent_name="snaga"),
                        record("rec-option", agent_name="grub", variables={"session_id": "--dangerous"}))
        manifest = self.probe_manifest(d)
        for id, reason in [("rec-missing", "variable 'session_id' is not recorded"),
                           ("rec-option", "variable 'session_id' starts with '-'")]:
            launch = self.recreate(d, id, manifest)
            self.output(d, launch, rb"ARGS=fresh\n")
            self.assertIn(reason, d.get(launch["session"])["detail"])

    def test_changed_configuration_failed_start_and_one_record_per_line(self):
        cwd = tempfile.TemporaryDirectory(prefix="goblins-recreate-cwd-"); self.addCleanup(cwd.cleanup)
        d = self.daemon(record("rec-f", configuration="fishy", cwd=cwd.name),
                        record("rec-gone", cwd="/nonexistent-goblins-recreate"))
        # The demo manifest has no 'fishy': the start fails and releases it.
        failed = self.recreate(d, "rec-f", wait="failed")
        self.assertIn("unknown goblin", d.get(failed["session"])["detail"])
        self.assertEqual(d.record("rec-f")["sessions"], ["rec-f"])
        updated = checks_manifest("updated-goblins")
        first = self.recreate(d, "rec-f", updated)["session"]
        self.assertEqual(self.exec(d, first, 'printf "%s %s" "$GOBLIN_MARKER" "$(pwd -P)"').stdout,
                         f"updated {Path(cwd.name).resolve()}")
        d.wait(lambda: d.record("rec-f")["sessions"] == ["rec-f", first])
        # While it runs, the record is refused by any of its ids.
        for target in ("rec-f", first):
            self.assertIn(f"goblin morb ({first}) is live and runs it", self.refused(d, target))
        d.rpc.call("sessions.stop", {"session": first})
        d.wait(lambda: d.get(first)["state"] == "stopped")
        # An older goblin id, as Emacs shows stopped goblins, finds the latest.
        second = self.recreate(d, first, updated)["session"]
        line = d.wait(lambda: (r if (r := d.record("rec-f"))["sessions"][-1] == second else None))
        self.assertEqual(line["sessions"], ["rec-f", first, second])
        self.assertEqual(sorted(p.stem for p in d.history.glob("*.json")), ["rec-f", "rec-gone"])
        self.assertIn("no longer exists", self.refused(d, "rec-gone"))
        self.assertIn("--parent", self.refused(d, "rec-gone", parent=second))
        self.assertIn("no recorded goblin", self.refused(d, "nobody"))
        # Names are labels: they select nothing.
        self.assertIn("no recorded goblin", self.refused(d, "morb"))

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
        self.assertIn("no recorded goblin", d.cli("recreate", "--detached", "cli-goblin", check=False).stderr)
        result = d.cli("recreate", "--detached", "rec-cli")
        launch = json.loads(result.stdout)
        self.assertEqual(launch["agent_name"], "cli-goblin")
        d.wait(lambda: d.get(launch["session"])["state"] == "running", timeout=60)
        self.assertEqual(d.get(launch["session"])["recreated_from"], "rec-cli")
        refused = d.cli("recreate", "--detached", "rec-cli", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("is live", refused.stderr)
        # Attached, by the goblin's own id.
        d.rpc.call("sessions.stop", {"session": launch["session"]})
        d.wait(lambda: d.record("rec-cli")["stopped"])
        from terminal_support import Terminal
        attached = Terminal([d.binary, "--runtime", d.manifest, "--state-dir", str(d.state), "recreate",
                             launch["session"]], env=d.env)
        self.addCleanup(attached.close)
        attached.expect("Recreated cli-goblin from rec-cli as ", timeout=60)
        attached.send("exit\r")
        self.assertEqual(attached.wait(), 0)
        workspace = tempfile.TemporaryDirectory(prefix="goblins-recreate-ws-"); self.addCleanup(workspace.cleanup)
        snapshot = self.daemon(record("rec-ws"), workspace=workspace.name)
        self.assertIn("--workspace", self.refused(snapshot, "rec-ws"))
        # Its own goblins are not recorded either.
        session = snapshot.start()["session"]
        time.sleep(1)
        self.assertIsNone(snapshot.record(session))

    def test_prune_by_age_date_and_count_never_running_records(self):
        now = int(time.time() * 1000)
        day = 86_400_000
        d = self.daemon(record("old", updated=now - 40 * day), record("mid", updated=now - 10 * day),
                        record("new", updated=now - day))
        goblin = d.start()["session"]
        running = d.wait(lambda: d.record(goblin))["id"]
        stored = lambda: sorted(p.stem for p in d.history.glob("*.json") if not p.name.startswith("."))
        usage = d.cli("history", "prune", check=False)
        self.assertEqual(usage.returncode, 2)
        self.assertIn("Usage", usage.stderr)
        self.assertEqual(d.cli("history", "prune", "--older-than", "30d", "--dry-run").stdout, "Would remove 1 record\nold\n")
        self.assertEqual(stored(), sorted(["old", "mid", "new", running]))
        # A deletion that fails is reported, and the record stays.
        d.history.chmod(0o500)
        try:
            failed = d.cli("history", "prune", "--older-than", "30d", check=False)
        finally:
            d.history.chmod(0o700)
        self.assertEqual(failed.returncode, 1)
        self.assertEqual(failed.stdout, "Removed 0 records\n")
        self.assertIn("Could not remove 1 record:\nold: ", failed.stderr)
        self.assertIn("old", [r["id"] for r in json.loads(d.cli("history", "--json").stdout)])
        self.assertEqual(d.cli("history", "prune", "--older-than", "30d").stdout, "Removed 1 record\nold\n")
        before = time.strftime("%Y-%m-%d", time.localtime((now - 5 * day) / 1000))
        self.assertEqual(d.cli("history", "prune", "--before", before).stdout, "Removed 1 record\nmid\n")
        self.assertEqual(d.cli("history", "prune", "--keep", "0").stdout, "Removed 1 record\nnew\n")
        d.wait(lambda: stored() == [running])
        # Offline, the CLI needs the store's lock.
        d.rpc.call("sessions.stop", {"session": goblin})
        d.wait(lambda: d.record(running)["stopped"])
        d.process.terminate(); d.process.wait(timeout=10)
        other = Daemon(state_home=d.state_home); self.addCleanup(other.close)
        owner = f"goblins server (pid {other.process.pid}, --state-dir {other.state}) owns {d.history}"
        refused = d.cli("history", "prune", "--keep", "0", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(owner, refused.stderr)
        # Listing reads without the lock and leaves the owner's files alone.
        (d.history / f".tmp-{running}.json").write_text("{")
        self.assertIn(running, d.cli("history").stdout)
        self.assertTrue((d.history / f".tmp-{running}.json").exists())
        # A server without history prunes like the CLI alone: it refuses too.
        third = Daemon(state_home=d.state_home); self.addCleanup(third.close)
        refused = third.cli("history", "prune", "--keep", "0", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(owner, refused.stderr)
        # Released, the waiting server takes the store over.
        other.close()
        third.wait(lambda: third.rpc.call("server.status", {})["history"]["owner"], timeout=15)
        self.assertFalse((d.history / f".tmp-{running}.json").exists())
        self.assertEqual(third.cli("history", "prune", "--keep", "0", "--dry-run").stdout,
                         f"Would remove 1 record\n{running}\n")
        third.close()
        self.assertEqual(d.cli("history", "prune", "--keep", "0", "--dry-run").stdout, f"Would remove 1 record\n{running}\n")
        self.assertEqual(d.cli("history", "prune", "--keep", "0").stdout, f"Removed 1 record\n{running}\n")
        self.assertEqual(stored(), [])

if __name__ == "__main__": unittest.main(verbosity=2)
