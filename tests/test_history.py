"""Session history as a person uses it: resume yesterday's goblin, and keep
the record safe through a second server and a crash.

Each daemon gets a private XDG_STATE_HOME (daemon_support), never the user's.
No coding agents are used: the demo shell configuration stands in, with
probe arguments.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import time
import unittest

from build_support import fixture_path, nix_bin
from daemon_support import Daemon, receive
from support import FLAKE, command, step
from terminal_support import Terminal

FRESH = ["--no-config", "-c", "printf 'ARGS=fresh\\n'; exec sleep 600"]
RESUME = ["--no-config", "-c",
          "printf 'RESUMED=%s\\n' {session_id}; printf 'FIRST=%s\\n' (command -v hello); "
          "printf 'COWSAY=%s\\n' (readlink /run/goblins/packages/current/bin/cowsay); "
          "printf 'COWTHINK=%s\\n' (readlink /run/goblins/packages/current/bin/cowthink); exec sleep 600"]


def record(id, configuration="shell", agent_name="morb", **extra):
    value = {"version": 3, "id": id, "configuration": configuration, "agent_name": agent_name, "cwd": None,
             "scope": None, "dev_shell": None, "docker_enabled": False,
             "packages": {"approved": [], "denied": [], "auto_approve": []}, "variables": {},
             "sessions": [id], "skipped": [], "worktree": None, "created": 1, "updated": 1, "stopped": 2}
    value.update(extra)
    return value


CUSTOM_CATALOGUE = """{
  inputs.tools = { url = "path:@TOOLS@"; flake = false; };
  outputs = { tools, ... }: {
    legacyPackages.x86_64-linux.probe = derivation {
      name = "goblins-custom-catalogue-probe";
      system = "x86_64-linux";
      builder = "${tools}/bin/bash";
      args = [ "-c" "echo probe > $out" ];
    };
  };
}
"""


class ResumeWorkflow(unittest.TestCase):
    """A goblin from yesterday is resumed today, offline and then online."""

    def setUp(self):
        # Yesterday's history holds two older records next to today's goblin.
        home = tempfile.TemporaryDirectory(prefix="goblins-resume-")
        self.addCleanup(home.cleanup)
        store = Path(home.name) / "goblins/sessions"
        store.mkdir(parents=True, mode=0o700)
        now = int(time.time() * 1000)
        self.day = 86_400_000
        self.now = now
        for r in (record("old", updated=now - 40 * self.day),
                  record("mid", agent_name="grub", updated=now - 10 * self.day),
                  record("unfree", agent_name="snaga", updated=now - 2 * self.day,
                         packages={"approved": ["unrar"], "denied": [], "auto_approve": []})):
            (store / f"{r['id']}.json").write_text(json.dumps(r))
        # A Nix that cannot download 'tree' while 'offline' exists.
        bindir = Path(tempfile.mkdtemp(prefix="goblins-fake-nix-"))
        self.addCleanup(shutil.rmtree, bindir)
        self.offline = bindir / "offline"
        real = str(nix_bin() / "nix")
        (bindir / "nix").write_text(f"""#!{sys.executable}
import os, sys
if os.path.exists({str(self.offline)!r}) and "eval" in sys.argv and any(a.endswith(".tree") for a in sys.argv):
    sys.stderr.write("error: unable to download 'https://cache.example/x.narinfo': Couldn't resolve host name (6)\\n")
    sys.exit(1)
os.execv({real!r}, [{real!r}, *sys.argv[1:]])
""")
        (bindir / "nix").chmod(0o700)
        # The pinned catalogue ignores the host's nixpkgs settings.
        self.d = Daemon(state_home=home.name, env={**os.environ, "NIXPKGS_ALLOW_UNFREE": "1"}, tool_path=bindir)
        self.addCleanup(self.d.close)

    def probe_manifest(self):
        """The demo manifest with a shell that reports what it starts with."""
        config = json.loads(Path(self.d.manifest).read_text())
        config["goblins"]["shell"]["args"] = FRESH
        config["goblins"]["shell"]["resume_command"] = RESUME
        path = Path(self.d.temp.name) / "probe.json"
        path.write_text(json.dumps(config))
        return str(path)

    def decide(self, session, package, approved):
        peer, request = self.d.pending(session, package)
        self.addCleanup(peer.close)
        self.d.decide(request, approved)
        peer.peer.settimeout(120)
        return receive(peer.peer)["result"], request

    def stored(self):
        return sorted(p.stem for p in self.d.history.glob("*.json") if not p.name.startswith("."))

    def test_resume_yesterdays_goblin(self):
        d = self.d
        manifest = self.probe_manifest()

        with step("the host approves and denies a goblin's packages, and its record follows"):
            goblin = d.start(manifest=manifest, agent_name="morb")["session"]
            first = d.wait(lambda: d.record(goblin))
            line = first["id"]
            # A short random id of its own, unlike the goblin's.
            self.assertRegex(line, r"^[0-9a-hjkmnp-tv-z]{8}$")
            self.assertEqual((first["version"], first["sessions"], first["configuration"]), (3, [goblin], "shell"))
            for package in ("cowsay", "neo-cowsay", "hello", "tree"):
                self.assertEqual(self.decide(goblin, package, True)[0]["status"], "ready", package)
            self.assertEqual(self.decide(goblin, "nmap", False)[0]["status"], "denied")
            approved = ["cowsay", "neo-cowsay", "hello", "tree"]
            d.wait(lambda: d.record(line)["packages"] == {"approved": approved, "denied": ["nmap"],
                                                          "auto_approve": []})

        with step("the goblin records how to resume its conversation"):
            result = d.cli("exec", goblin, "--", "goblin", "variable", "set", "session_id=c8193c93",
                           "session_name=Plan it")
            self.assertEqual(json.loads(result.stdout), {"accepted": True, "recorded": True})
            d.wait(lambda: d.record(line)["variables"] == {"session_id": "c8193c93", "session_name": "Plan it"})

        with step("it stops, and goblins history lists it"):
            d.rpc.call("sessions.stop", {"session": goblin})
            d.wait(lambda: d.record(line)["stopped"])
            d.wait_record_released(line)
            listing = d.cli("history", "--no-pager").stdout
            self.assertIn("Plan it", next(l for l in listing.splitlines() if l.startswith(line)))

        with step("offline, the host resumes it by its old goblin id"):
            self.offline.touch()
            attached = Terminal([d.binary, "--runtime", manifest, "--state-dir", str(d.state), "resume", goblin],
                                env=d.env)
            self.addCleanup(attached.close)
            text = attached.expect(rf"Resumed morb \({line}\) as [\s\S]*?COWTHINK=\S+\n", timeout=120)[0].decode()
            session = next(r["id"] for r in d.rpc.call("sessions.list", {}) if r.get("recreated_from") == line)

        with step("the resume command gets its variable, and the packages are back in order"):
            self.assertIn("RESUMED=c8193c93\n", text)
            self.assertIn("FIRST=/run/goblins/packages/current/bin/hello\n", text)
            self.assertNotIn("ARGS=fresh", text)
            # The earlier grant wins a collision, as live grants do.
            self.assertRegex(text, r"COWSAY=/nix/store/[a-z0-9]{32}-cowsay-[^/]*/bin/cowsay\n")
            self.assertRegex(text, r"COWTHINK=/nix/store/[a-z0-9]{32}-cowsay-[^/]*/bin/cowthink\n")
            r = d.get(session)
            self.assertEqual(r["packages"], ["cowsay", "neo-cowsay", "hello"])

        with step("what could not be restored is listed with why, and stays approved"):
            self.assertEqual([s["name"] for s in r["skipped"]], ["tree"])
            self.assertIn("unable to download", r["skipped"][0]["reason"])
            own = d.wait(lambda: (r if (r := d.record(line))["sessions"] == [goblin, session] else None))
            self.assertEqual((own["packages"]["approved"], own["skipped"], own["stopped"]),
                             (approved, r["skipped"], None))
            self.assertFalse((d.history / f"{session}.json").exists())
            listing = next(l for l in d.cli("history", "--no-pager").stdout.splitlines() if l.startswith(line))
            self.assertIn("tree", listing)
            self.assertRegex(listing, r"\byes\b")

        with step("while it runs, resuming it again is refused"):
            refused = d.cli("resume", "--detached", line, check=False)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn(f"goblin morb ({session}) is live and runs it", refused.stderr)

        with step("back online, the next resume restores what was skipped"):
            attached.close()
            d.rpc.call("sessions.stop", {"session": session})
            d.wait(lambda: d.record(line)["stopped"])
            d.wait_record_released(line)
            self.offline.unlink()
            launch = json.loads(d.cli("resume", "--detached", session).stdout)
            again = d.wait(lambda: (r if (r := d.get(launch["session"]))["state"] != "starting" else None),
                           timeout=180)
            self.assertEqual(again["state"], "running", again)
            self.assertEqual((again["packages"], again["skipped"]), (["cowsay", "neo-cowsay", "hello", "tree"], []))
            d.wait(lambda: d.record(line)["sessions"][-1] == launch["session"])

        with step("a recorded unfree package stays refused although the server's environment allows it"):
            other = json.loads(d.cli("resume", "--detached", "unfree").stdout)["session"]
            unfree = d.wait(lambda: (r if (r := d.get(other))["state"] != "starting" else None),
                            timeout=180)
            self.assertEqual((unfree["state"], unfree["packages"]), ("running", []))
            self.assertEqual([s["name"] for s in unfree["skipped"]], ["unrar"])
            self.assertIn("unfree", unfree["skipped"][0]["reason"])
            d.rpc.call("sessions.stop", {"session": other})
            d.wait_record_released("unfree")

        with step("history prune removes records by age, date and count, never a running one"):
            self.assertEqual(d.cli("history", "prune", "--older-than", "30d", "--dry-run").stdout,
                             "Would remove 1 record\nold\n")
            self.assertEqual(self.stored(), sorted(["old", "mid", "unfree", line]))
            self.assertEqual(d.cli("history", "prune", "--older-than", "30d").stdout, "Removed 1 record\nold\n")
            before = time.strftime("%Y-%m-%d", time.localtime((self.now - 5 * self.day) / 1000))
            self.assertEqual(d.cli("history", "prune", "--before", before).stdout, "Removed 1 record\nmid\n")
            self.assertEqual(d.cli("history", "prune", "--keep", "0").stdout, "Removed 1 record\nunfree\n")
            d.wait(lambda: self.stored() == [line])

        with step("without a server, history reads the store and cannot tell what runs"):
            d.rpc.call("sessions.stop", {"session": launch["session"]})
            d.wait(lambda: d.record(line)["stopped"])
            d.process.terminate()
            d.process.wait(timeout=10)
            offline = json.loads(d.cli("history", "--json").stdout)
            self.assertEqual([(r["id"], r["running"]) for r in offline], [(line, None)])
            self.assertRegex(d.cli("history").stdout, rf"(?m)^{line} .* \? ")
            # Pruning takes the store's own lock.
            self.assertEqual(d.cli("history", "prune", "--keep", "0").stdout, f"Removed 1 record\n{line}\n")
            self.assertEqual(self.stored(), [])


class TwoServersWorkflow(unittest.TestCase):
    """A second server on the same store, and a crash, never lose a record."""

    def test_two_servers_and_a_crash_keep_history_safe(self):
        d = Daemon()
        self.addCleanup(d.close)

        with step("the first server keeps the history"):
            session = d.start()["session"]
            line = d.wait(lambda: d.record(session))["id"]
            d.cli("exec", session, "--", "goblin", "variable", "set", "session_id=survivor")
            d.wait(lambda: d.record(session)["variables"].get("session_id") == "survivor")
            self.assertIn(f"Session history in {d.history}", d.cli("server", "status").stdout)

        with step("a second server on the same store runs without history, and says whose it is"):
            other = Daemon(state_home=d.state_home)
            self.addCleanup(other.close)
            status = other.rpc.call("server.status", {})["history"]
            self.assertFalse(status["owner"])
            owner = f"goblins server (pid {d.process.pid}, --state-dir {d.state}) owns {d.history}"
            self.assertIn(owner, status["reason"])
            self.assertIn(f"No session history: {owner}", other.cli("server", "status").stdout)
            foreign = other.start()["session"]
            peer, request = other.pending(foreign)
            self.addCleanup(peer.close)
            other.decide(request, True)
            peer.peer.settimeout(60)
            self.assertEqual(receive(peer.peer)["result"]["status"], "ready")
            result = other.cli("exec", foreign, "--", "goblin", "variable", "set", "session_id=kept")
            self.assertEqual(json.loads(result.stdout), {"accepted": True, "recorded": False})
            self.assertIsNone(d.record(foreign))
            # It prunes like the CLI alone, under the store's lock, which the first holds.
            refused = other.cli("history", "prune", "--keep", "0", check=False)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn(owner, refused.stderr)

        with step("the first server is killed, and its goblin's record is intact"):
            d.process.send_signal(signal.SIGKILL)
            d.process.wait(timeout=10)
            survivor = d.record(line)
            self.assertEqual((survivor["variables"], survivor["stopped"]), ({"session_id": "survivor"}, None))

        with step("the second server takes the store over and records its running goblin"):
            other.wait(lambda: other.rpc.call("server.status", {})["history"]["owner"], timeout=15)
            kept = other.wait(lambda: other.record(foreign))
            self.assertEqual((kept["packages"]["approved"], kept["variables"], kept["sessions"]),
                             (["hello"], {"session_id": "kept"}, [foreign]))
            refused = other.cli("resume", "--detached", foreign, check=False)
            self.assertIn("is live and runs it", refused.stderr)
            # The crashed server's goblin can be resumed.
            records = {r["id"]: r for r in json.loads(other.cli("history", "--json").stdout)}
            self.assertEqual((records[line]["running"], records[kept["id"]]["running"]), (False, True))


class CustomCatalogueWorkflow(unittest.TestCase):
    def test_a_catalogue_that_is_not_nixpkgs_serves_its_packages(self):
        # A flake in the store whose only package is 'probe', with no
        # default.nix and no nixpkgs: only the flake path can serve it.
        bash = fixture_path(FLAKE + "#checks.x86_64-linux.static-bash^out")
        source = Path(tempfile.mkdtemp(prefix="goblins-catalogue-"))
        self.addCleanup(shutil.rmtree, source)
        (source / "flake.nix").write_text(CUSTOM_CATALOGUE.replace("@TOOLS@", str(bash)))
        command(["nix", "flake", "lock", f"path:{source}"])
        store = command(["nix", "store", "add-path", str(source)]).strip()
        d = Daemon()
        self.addCleanup(d.close)
        config = json.loads(Path(d.manifest).read_text())
        goblin = config["goblins"]["shell"]
        goblin["flake"] = f"path:{store}"
        del goblin["nixpkgs"]
        manifest = Path(d.temp.name) / "custom-catalogue.json"
        manifest.write_text(json.dumps(config))
        session = d.start(manifest=manifest)["session"]
        for package, status in (("probe", "ready"), ("hello", "error")):
            with step(f"the host approves {package}"):
                request, permission = d.pending(session, package)
                self.addCleanup(request.close)
                d.decide(permission, True)
                request.peer.settimeout(120)
                self.assertEqual(receive(request.peer)["result"]["status"], status)
        self.assertEqual(d.get(session)["packages"], ["probe"])
        self.assertIn("does not provide attribute",
                      d.rpc.call("permissions.get", {"request": permission["id"]})["message"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
