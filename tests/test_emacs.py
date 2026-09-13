"""Emacs approvals against the real daemon; never loads the user's init file."""
import os
import shutil
import subprocess
import tempfile
import unittest

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

    def test_start_server_from_status(self):
        application = app()
        with tempfile.TemporaryDirectory(prefix="ge-start-") as state:
            try:
                result = subprocess.run(
                    ["emacs", "--batch", "-Q", "-L", str(ROOT / "emacs"),
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
            ["emacs", "--batch", "-Q", "-L", str(ROOT / "emacs"),
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
