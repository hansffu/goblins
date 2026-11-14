"""CLI scope clearing and GC: retention, active leases and confined deletion."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from build_support import fixture_app
from daemon_support import Daemon
from support import step


class CleanupTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("scope-goblins")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-cleanup-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / "state"
        self.cache = self.root / "cache/goblins"
        self.env = dict(os.environ, XDG_CACHE_HOME=str(self.root / "cache"),
                        XDG_STATE_HOME=str(self.root / "history"))

    def cli(self, *args, code=0):
        result = subprocess.run([str(self.app), "--state-dir", str(self.state), *args],
                                env=self.env, text=True, capture_output=True, timeout=60)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def directory(self, path):
        path.mkdir(parents=True, mode=0o700, exist_ok=True)
        path.chmod(0o700)
        (path / "payload").write_text("retained data")
        return path

    def store(self, kind, name):
        # Each host-private store root must itself have mode 0700.
        (self.cache / kind).mkdir(parents=True, mode=0o700, exist_ok=True)
        path = self.directory(self.cache / kind / name)
        if name.startswith(("temporary-", "session-")):
            locks = self.cache / "locks"
            locks.mkdir(mode=0o700, exist_ok=True)
            (locks / f"{kind}-{name}").touch(mode=0o600)
        return path

    def test_scope_data_maintenance(self):
        with step("a running scope refuses clearing and survives garbage collection"):
            daemon = Daemon(self.app, env=self.env)
            self.addCleanup(daemon.close)
            self.state = daemon.state
            session = daemon.start("member")["session"]
            live = self.cache / "scopes/scope-work"
            self.assertTrue(live.exists())
            self.assertIn("in use", self.cli("scope", "clear", "work", code=1))
            self.cli("gc")
            self.assertTrue(live.exists())
            daemon.cli("exec", session, "--", "sh", "-c", "test -d /tmp")
            lock = self.cache / "scopes/locks/work"
            inode = lock.stat().st_ino

        with step("preview and clear a retired scope without clearing the running scope"):
            retired = [self.store(kind, "scope-retired") for kind in ("scopes", "docker")]
            keep = self.store("scopes", "scope-keep")
            self.assertIn("Would remove", self.cli("scope", "clear", "retired", "--dry-run"))
            self.assertTrue(all(path.exists() for path in retired))
            self.cli("scope", "clear", "retired")
            self.assertTrue(all(not path.exists() for path in retired))
            self.assertTrue(live.exists() and keep.exists())

        with step("stop the goblin, clear its scope, then clear all retired scope data"):
            daemon.cli("stop", session)
            daemon.wait(lambda: not (daemon.state / session / "resources").exists())
            self.cli("scope", "clear", "work")
            self.assertFalse(live.exists())
            self.assertEqual(lock.stat().st_ino, inode)
            self.cli("scope", "clear", "--all")
            self.assertFalse(keep.exists())
            self.assertIn("No stored scope data", self.cli("scope", "clear", "--all"))
            daemon.process.terminate()
            daemon.process.wait(timeout=10)
            self.assertFalse((daemon.state / session).exists())

        with step("preview and collect abandoned data while keeping persistent stores, history and logs"):
            temporary = self.store("scopes", "temporary-Abc123")
            engine = self.store("docker", "session-Xyz987")
            persistent = self.store("docker", "scope-retained")
            unknown = self.store("scopes", "unrecognized")
            legacy = self.directory(self.cache / "scopes/temporary-Old123")
            scratch = self.directory(self.cache / "locks/cleanup-Abc123")
            abandoned = self.directory(self.state / ("a" * 32 + "-s1"))
            manifests = self.directory(self.state / "manifests")
            (self.state / "server.log").write_text("log")
            audit = next(self.state.glob("communications-*.jsonl"))
            audit_contents = audit.read_bytes()
            history = {path: path.read_bytes() for path in daemon.history.glob("*.json")}
            self.assertTrue(history)
            collected = [temporary, engine, scratch, abandoned, manifests]
            self.assertIn("Would remove", self.cli("gc", "--dry-run"))
            self.assertTrue(all(path.exists() for path in collected))
            self.cli("gc")
            self.assertFalse(any(path.exists() for path in collected))
            self.assertTrue(all(path.exists() for path in (persistent, unknown, legacy)))
            self.assertEqual((self.state / "server.log").read_text(), "log")
            self.assertEqual(audit.read_bytes(), audit_contents)
            for path, contents in history.items():
                self.assertEqual(path.read_bytes(), contents)
            self.cli("gc")


if __name__ == "__main__":
    unittest.main()
