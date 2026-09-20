"""Emacs approvals against the real daemon; never loads the user's init file."""
import os
import json
import shlex
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

from daemon_support import Daemon, ROOT, RPC, app, receive


@unittest.skipUnless(shutil.which("emacs"), "Emacs is not installed")
class EmacsTests(unittest.TestCase):
    def setUp(self):
        available = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (locate-library "magit-section") 0 1))'],
            capture_output=True, timeout=10,
        )
        if available.returncode:
            self.skipTest("magit-section is not on Emacs load-path")

    def test_live_ownership_tree_and_stopped_branches(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        parent = daemon.start(agent_name="parent")
        other = daemon.start(agent_name="other")
        def child(owner, name):
            launch = daemon.rpc.call("sessions.start", {
                "key":uuid.uuid4().hex,"name":"shell","agent_name":name,
                "configuration":daemon.manifest,"parent":owner["session"],"rows":24,"cols":100,
            })
            daemon.wait(lambda: daemon.get(launch["session"])["state"] == "running")
            return launch
        kid = child(parent, "kid")
        leaf = child(kid, "leaf")
        child(other, "kid")
        result = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
             "-l", "goblins.el", "-l", "goblins-tests.el", "-f", "goblins-test-tree-live"],
            env={**os.environ,"GOBLINS_TEST_STATE":str(daemon.state),
                 "GOBLINS_TEST_PARENT":parent["session"],"GOBLINS_TEST_CHILD":kid["session"],
                 "GOBLINS_TEST_LEAF":leaf["session"]},
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(daemon.get(other["session"])["state"], "running")

    def test_start_server_from_status(self):
        application = app()
        with tempfile.TemporaryDirectory(prefix="ge-start-") as state:
            try:
                result = subprocess.run(
                    ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
                     "-l", "goblins-tests", "-f", "goblins-test-start-live"],
                    env={**os.environ, "GOBLINS_APP": str(application),
                         "GOBLINS_TEST_STATE": state},
                    capture_output=True, text=True, timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                peer = RPC(os.path.join(state, "host.sock"))
                try:
                    self.assertEqual(peer.call("server.status", {})["state"], "running")
                finally:
                    peer.close()
            finally:
                subprocess.run([str(application), "--state-dir", state, "server", "stop"],
                               capture_output=True, check=True, timeout=20)

    def test_live_status_accept_deny_reconnect_and_close(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        agents = [daemon.start(agent_name=name) for name in ("snikk", "zoggit")]
        peers, requests = [], []
        for agent in agents:
            peer = RPC(daemon.state / agent["session"] / "resources/request.sock")
            self.addCleanup(peer.close)
            peer.send("permissions.request", {
                "kind": "package", "package": "hello", "reason": "Try a live grant 🐟",
            })
            requests.append(daemon.wait(lambda: next(iter(daemon.permissions(
                session=agent["session"], state="pending")), None)))
            peers.append(peer)
        result = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
             "-l", "goblins-tests", "-f", "goblins-test-live"],
            env={**os.environ, "GOBLINS_TEST_STATE": str(daemon.state),
                 "GOBLINS_TEST_ACCEPT": requests[0]["id"],
                 "GOBLINS_TEST_DENY": requests[1]["id"]},
            capture_output=True, text=True, timeout=90,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(receive(peers[0].peer)["result"]["status"], "ready")
        self.assertEqual(receive(peers[1].peer)["result"]["status"], "denied")
        for agent in agents:
            self.assertEqual(daemon.get(agent["session"])["state"], "running")

    def test_run_in_ghostel_and_visit_from_status(self):
        available = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (and (locate-library "ghostel") (locate-library "ghostel-module")) 0 1))'],
            capture_output=True, timeout=10,
        )
        if available.returncode:
            self.skipTest("Ghostel and its native module are not installed")
        daemon = Daemon()
        self.addCleanup(daemon.close)
        external = daemon.start()
        # Blank selection exercises the configured default. Keep this fixture
        # shell-only: adding Codex must never turn a default test into model work.
        manifest = json.loads(Path(daemon.manifest).read_text())
        manifest["goblins"] = {"shell": manifest["goblins"]["shell"]}
        manifest_path = Path(daemon.temp.name) / "shell-only.json"
        manifest_path.write_text(json.dumps(manifest))
        launcher = Path(daemon.temp.name) / "goblins-shell-only"
        launcher.write_text(f'#!/bin/sh\nexec {shlex.quote(daemon.binary)} --runtime {shlex.quote(str(manifest_path))} "$@"\n')
        launcher.chmod(0o700)
        with tempfile.TemporaryDirectory(prefix="ge-workspace-") as workspace:
            result = subprocess.run(
                ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
                 "-l", "goblins-tests", "-f", "goblins-test-run-live"],
                env={**os.environ, "GOBLINS_APP": str(launcher),
                     "GOBLINS_TEST_STATE": str(daemon.state),
                     "GOBLINS_TEST_WORKSPACE": workspace,
                     "GOBLINS_TEST_EXTERNAL": external["session"]},
                capture_output=True, text=True, timeout=90,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((Path(workspace) / "emacs-launch.txt").read_text(), "ghostel-run-ok")


if __name__ == "__main__":
    unittest.main(verbosity=2)
