"""CLI scope clearing and GC: retention, active leases and confined deletion."""
import fcntl
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from build_support import fixture_app
from daemon_support import Daemon


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

    def lock(self, path, shared=False):
        path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        file = path.open("a+")
        fcntl.flock(file, fcntl.LOCK_SH if shared else fcntl.LOCK_EX)
        self.addCleanup(file.close)
        return file

    def test_scope_clear_requires_selection_and_validates_names(self):
        for args in [("scope", "clear"), ("scope", "clear", "work", "--all")]:
            self.cli(*args, code=2)
        self.assertIn("invalid scope name", self.cli("scope", "clear", "../escape", code=1))

    def test_scope_clear_dry_run_then_clears_only_the_selected_scope(self):
        selected = [self.store(kind, "scope-retired") for kind in ("scopes", "docker")]
        keep = self.store("scopes", "scope-keep")
        outside = self.directory(self.root / "host-project")
        (selected[0] / "link").symlink_to(outside, target_is_directory=True)
        os.link(outside / "payload", selected[0] / "hardlink")
        outside_mode = (outside / "payload").stat().st_mode
        locked_down = selected[0] / "locked"
        locked_down.mkdir()
        (locked_down / "file").write_text("x")
        locked_down.chmod(0)
        text = self.cli("scope", "clear", "retired", "--dry-run")
        self.assertIn("Would remove", text)
        self.assertTrue(all(path.exists() for path in selected))
        self.cli("scope", "clear", "retired")
        self.assertTrue(all(not path.exists() for path in selected))
        self.assertEqual((outside / "payload").read_text(), "retained data")
        self.assertEqual((outside / "payload").stat().st_mode, outside_mode)
        self.assertTrue(keep.exists())
        self.cli("scope", "clear", "--all")
        self.assertFalse(keep.exists())
        self.assertIn("No stored scope data", self.cli("scope", "clear", "--all"))

    def test_scope_clear_preflights_all_locks_and_keeps_lock_inodes(self):
        first = self.store("scopes", "scope-a")
        second = self.store("docker", "scope-z")
        path = self.cache / "docker/locks/z"
        lease = self.lock(path)
        inode = path.stat().st_ino
        self.assertIn("in use", self.cli("scope", "clear", "--all", code=1))
        self.assertTrue(first.exists())
        self.assertTrue(second.exists())
        fcntl.flock(lease, fcntl.LOCK_UN)
        self.cli("scope", "clear", "--all")
        self.assertEqual(path.stat().st_ino, inode)

    def test_scope_clear_refuses_a_top_level_symlink(self):
        outside = self.directory(self.root / "outside")
        root = self.cache / "scopes"
        root.mkdir(parents=True, mode=0o700)
        (root / "scope-work").symlink_to(outside, target_is_directory=True)
        self.assertIn("unsafe scope", self.cli("scope", "clear", "work", code=1))
        self.assertTrue((outside / "payload").exists())

    def test_gc_collects_only_dangling_data_and_preserves_history_and_logs(self):
        temp = self.store("scopes", "temporary-Abc123")
        engine = self.store("docker", "session-Xyz987")
        persistent = self.store("docker", "scope-retained")
        unknown = self.store("scopes", "unrecognized")
        scratch = self.directory(self.cache / "locks/cleanup-Abc123")
        legacy = self.directory(self.cache / "scopes/temporary-Old123")
        session = self.directory(self.state / ("a" * 32 + "-s1"))
        self.state.chmod(0o700)
        manifests = self.directory(self.state / "manifests")
        outside = self.directory(self.root / "host-project")
        (session / "host-bind").symlink_to(outside, target_is_directory=True)
        (manifests / "gc-root").symlink_to(outside, target_is_directory=True)
        (self.state / "server.log").write_text("log")
        (self.state / ("communications-" + "a" * 32 + ".jsonl")).write_text("audit")
        history = self.directory(self.root / "history/goblins/sessions")
        self.assertIn("Would remove", self.cli("gc", "--dry-run"))
        self.assertTrue(all(path.exists() for path in (temp, engine, session, manifests)))
        self.cli("gc")
        self.assertTrue(all(not path.exists() for path in (temp, engine, session, manifests, scratch)))
        self.assertTrue(all(path.exists() for path in (persistent, unknown, history, outside, legacy)))
        self.assertEqual((self.state / "server.log").read_text(), "log")
        self.assertEqual(len(list(self.state.glob("communications-*.jsonl"))), 1)
        self.cli("gc")

    def test_gc_keeps_live_stores_and_live_or_guardian_owned_resources(self):
        scope = self.store("scopes", "temporary-Abc123")
        engine = self.store("docker", "session-Xyz987")
        stale = self.store("scopes", "temporary-Qrs456")
        leases = [self.lock(self.cache / "locks/scopes-temporary-Abc123"),
                  self.lock(self.cache / "locks/docker-session-Xyz987")]
        resource = self.directory(self.state / ("a" * 32 + "-s1") / "resources")
        self.state.chmod(0o700)
        daemon_lock = self.lock(self.state / "daemon.lock")
        resource_lock = self.lock(resource / ".gc-lease", shared=True)
        self.cli("gc")
        self.assertTrue(scope.exists())
        self.assertTrue(engine.exists())
        self.assertFalse(stale.exists())
        self.assertTrue(resource.exists())
        fcntl.flock(daemon_lock, fcntl.LOCK_UN)
        self.assertIn("active session", self.cli("gc"))
        self.assertTrue(resource.exists())
        for lease in [*leases, resource_lock]:
            fcntl.flock(lease, fcntl.LOCK_UN)
        self.cli("gc")
        self.assertFalse(scope.exists())
        self.assertFalse(engine.exists())
        self.assertFalse(resource.parent.exists())

    def test_gc_refuses_symlinked_roots_and_session_directories(self):
        outside = self.directory(self.root / "outside")
        self.cache.mkdir(parents=True, mode=0o700)
        (self.cache / "scopes").symlink_to(outside, target_is_directory=True)
        self.cli("gc", code=1)
        (self.cache / "scopes").unlink()
        self.state.mkdir(mode=0o700)
        (self.state / ("a" * 32 + "-s1")).symlink_to(outside, target_is_directory=True)
        self.cli("gc", code=1)
        self.assertEqual((outside / "payload").read_text(), "retained data")
        (self.state / ("a" * 32 + "-s1")).unlink()
        session = self.directory(self.state / ("a" * 32 + "-s1"))
        (session / "resources").symlink_to(outside, target_is_directory=True)
        self.cli("gc", code=1)
        self.assertFalse((outside / ".gc-lease").exists())

    def test_gc_refuses_a_mounted_subtree_without_deleting_its_contents(self):
        outside = self.directory(self.root / "outside")
        session = self.directory(self.state / ("a" * 32 + "-s1"))
        self.state.chmod(0o700)
        mounted = session / "mounted"
        mounted.mkdir()
        # A private mount namespace exposes a same-filesystem bind mount to
        # the worker. Device-number checks alone cannot enforce this boundary.
        result = subprocess.run([
            "bwrap", "--die-with-parent", "--unshare-user", "--unshare-pid",
            "--bind", "/", "/", "--proc", "/proc",
            "--bind", str(outside), str(mounted),
            str(self.app), "--state-dir", str(self.state), "gc",
        ], env=self.env, text=True, capture_output=True, timeout=60)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("Invalid cross-device link", result.stderr)
        self.assertEqual((outside / "payload").read_text(), "retained data")
        self.assertTrue(session.exists())

    def test_running_scope_is_refused_then_can_be_cleared_after_stop(self):
        daemon = Daemon(self.app, env=self.env)
        self.addCleanup(daemon.close)
        session = daemon.start("member")["session"]
        result = daemon.cli("scope", "clear", "work", check=False)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("in use", result.stderr)
        daemon.rpc.call("sessions.stop", {"session": session})
        # Stopped records retain terminal.sock until the daemon shuts down;
        # resource teardown is the point where the scope lock is released.
        daemon.wait(lambda: not (daemon.state / session / "resources").exists())
        daemon.cli("scope", "clear", "work")
        self.assertFalse((self.cache / "scopes/scope-work").exists())
        daemon.process.terminate()
        daemon.process.wait(timeout=10)
        self.assertFalse((daemon.state / session).exists())
