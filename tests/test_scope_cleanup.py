"""Scope runtime collection refuses same-device bind mounts for every caller."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from build_support import fixture_app
from support import rust_driver


class ScopeCleanupTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("scope-goblins")
        cls.driver = rust_driver()

    def probe(self, caller, mounted_root):
        with tempfile.TemporaryDirectory(prefix="goblins-scope-cleanup-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir(mode=0o700)
            (state / "scopes").mkdir(mode=0o700)
            selected = state / "scopes/work"
            selected.mkdir(mode=0o700)
            (selected / "lock").touch(mode=0o600)
            outside = root / "outside"
            outside.mkdir(mode=0o700)
            payload = outside / "payload"
            payload.write_text("outside scope runtime\n")
            mounted = selected if mounted_root else selected / "mounted"
            if not mounted_root:
                mounted.mkdir()
            # Both paths are on the same filesystem: device-number checks
            # alone cannot detect the bind mount or enforce this boundary.
            self.assertEqual(outside.stat().st_dev, mounted.stat().st_dev)
            env = {**os.environ, "XDG_CACHE_HOME": str(root / "cache"),
                   "XDG_STATE_HOME": str(root / "history")}
            prefix = ["bwrap", "--die-with-parent", "--unshare-user", "--unshare-pid",
                      "--bind", "/", "/", "--proc", "/proc",
                      "--bind", str(outside), str(mounted)]
            if caller == "gc":
                command = [str(self.app), "--state-dir", str(state), "gc"]
                request = None
            else:
                # Invoke exactly the runtime function used by Process::start,
                # through the existing ignored Rust host test adapter. No
                # production RPC, debug method or namespace fault switch.
                command = [self.driver, "--ignored", "--exact", "session::tests::host_driver", "--nocapture"]
                request = json.dumps({"op": "remove-stale", "path": str(selected)}) + "\n"
            result = subprocess.run([*prefix, *command], input=request, env=env,
                                    text=True, capture_output=True, timeout=60)
            self.assertTrue(payload.exists(), "cleanup deleted an outside bind mount's contents")
            self.assertEqual(payload.read_text(), "outside scope runtime\n")
            self.assertTrue(selected.exists())
            if mounted_root:
                self.assertFalse((outside / "lock").exists(), "opened a lock inside a mounted outside root")
            if caller == "gc":
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("Invalid cross-device link", result.stderr)
            else:
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                reply, = [json.loads(line[7:]) for line in result.stdout.splitlines() if line.startswith("DRIVER ")]
                self.assertIn("Invalid cross-device link", reply.get("error", ""), result.stdout + result.stderr)

    def test_gc_refuses_a_same_device_mounted_subtree(self):
        self.probe("gc", False)

    def test_relaunch_removal_refuses_a_same_device_mounted_subtree(self):
        self.probe("relaunch", False)

    def test_gc_refuses_a_mounted_root_before_opening_its_lock(self):
        self.probe("gc", True)

    def test_relaunch_removal_refuses_a_mounted_root_before_opening_its_lock(self):
        self.probe("relaunch", True)
