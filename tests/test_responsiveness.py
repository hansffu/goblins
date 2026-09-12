"""Regressions against the real Rust CLI/event loop and per-session socket."""
import contextlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import sys
import tempfile
import time
import unittest
from support import command, request
from test_connected import Terminal


class ResponsiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory(prefix="goblins-responsive-build-")
        cls.app = Path(command(["nix", "build", "--print-out-paths", "--out-link", Path(cls.build.name) / "app",
                                "path:" + str(Path(__file__).resolve().parents[1]) + "#goblins"])) / "bin/goblins"
        wrapper = cls.app.read_text()
        cls.binary, cls.config = re.search(r'exec (\S+) --runtime (\S+)', wrapper).groups()

    @classmethod
    def tearDownClass(cls):
        cls.build.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-responsive-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / "control"
        self.home = self.root / "home"
        (self.home / ".config/fish").mkdir(parents=True)

    def start(self, env=None):
        env = {**(env or os.environ), "HOME": str(self.home)}
        self.server = Terminal([self.binary, "--runtime", self.config, "--state-dir", str(self.state), "serve"], env=env)
        self.addCleanup(self.server.close)
        self.server.expect("Goblins serving")
        self.attach()

    def attach(self):
        self.client = Terminal([str(self.app), "--state-dir", str(self.state), "shell"])
        self.addCleanup(self.client.close)
        self.server.expect("shell connected")
        self.client.expect("workspace[>#]")
        self.server.send("status\n")
        status = json.loads(self.server.expect(r'(\{"packages":.*\})\n').group(1))
        self.payload = status["session"]["pid"]
        # Find the matching host-only event record; no debug endpoint is added.
        for path in Path("/tmp").glob("goblins-*/events.jsonl"):
            with contextlib.suppress(OSError, ValueError):
                if any(event.get("event") == "started" and event.get("fields", {}).get("pid") == self.payload
                       for event in map(json.loads, path.read_text().splitlines())):
                    self.directory = path.parent
                    break
        else:
            self.fail("no session event record")

    def peer(self):
        peer = socket.socket(socket.AF_UNIX)
        self.addCleanup(peer.close)
        peer.settimeout(5)
        peer.connect(str(self.directory / "request.sock"))
        return peer

    def pending(self):
        peer = self.peer()
        peer.sendall((json.dumps(request(package="hello")) + "\n").encode())
        self.server.expect("Approve package\\? Type approve or deny:")
        return peer

    def assert_status_responsive(self):
        start = time.monotonic()
        self.server.send("status\n")
        self.server.expect(r'\{"packages":.*\}\n', timeout=1.5)
        self.assertLess(time.monotonic() - start, 1.5)

    def assert_stopped(self):
        self.server.expect("Goblin stopped", timeout=2)
        self.assertFalse(Path(f"/proc/{self.payload}").exists())
        self.assertFalse(self.directory.exists())

    def test_whole_frame_timeout_and_other_requests_progress(self):
        self.start()
        slow = self.peer()
        start = time.monotonic()
        slow.sendall(b'{"v":')
        # Another client reaches a decision while the first frame is incomplete.
        peer = self.pending()
        self.assert_status_responsive()
        self.server.send("deny\n")
        self.assertEqual(json.loads(peer.recv(4096))["status"], "denied")
        # Drip bytes less than three seconds apart. The accept deadline must not
        # slide with them, unlike the PoC's per-recv timeout.
        for target in (.85, 1.7, 2.55):
            time.sleep(max(0, start + target - time.monotonic()))
            slow.sendall(b' ')
        reply = json.loads(slow.recv(4096))
        elapsed = time.monotonic() - start
        self.assertEqual(reply["status"], "error")
        self.assertIn("deadline", reply["message"])
        self.assertLess(elapsed, 3.5)
        # Both excess frames and disconnected partial frames stay non-authorizing.
        for data in (b'{}\n{}\n', b'x' * 4097):
            peer = self.peer(); peer.sendall(data)
            self.assertEqual(json.loads(peer.recv(4096))["status"], "error")
        peer = self.peer(); peer.sendall(b'{"v":'); peer.close()
        self.assert_status_responsive()
        self.server.send("quit\n")
        self.assertEqual(self.server.wait(), 0)
        self.assertEqual(self.client.wait(), 0)
        print(f"EVIDENCE whole-frame rejection in {elapsed:.3f}s, other request and status progressed", flush=True)

    def test_disconnect_and_shutdown_during_framing_and_approval(self):
        self.start()
        slow = self.peer(); slow.sendall(b'{')
        self.client.close()
        self.assert_stopped()
        self.attach()
        self.pending()
        self.assert_status_responsive()
        self.client.close()
        self.assert_stopped()
        self.attach()
        self.pending()
        self.server.send("quit\n")
        self.assertEqual(self.server.wait(), 0)
        self.assertEqual(self.client.wait(), 0)
        self.assertFalse(self.directory.exists())

    def test_build_cancellation_reaps_nix_and_handles_disconnect(self):
        fakebin = self.root / "bin"; fakebin.mkdir()
        marker = self.root / "building"
        # Trusted test substitute for a slow host Nix client. It is never put in
        # the sandbox, catalog, or production configuration. Startup nix-store
        # and eval still use real host Nix; only build pauses after approval.
        real_nix = shutil.which("nix")
        script = fakebin / "nix"
        script.write_text(f'''#!{sys.executable}
import json,os,subprocess,sys,time
if "build" in sys.argv:
 child=subprocess.Popen([{shutil.which("sleep")!r},"60"])
 open({str(marker)!r},"w").write(json.dumps([os.getpid(),child.pid]))
 time.sleep(60)
else:
 os.execv({real_nix!r},[{real_nix!r},*sys.argv[1:]])
''')
        script.chmod(0o700)
        env = {**os.environ, "PATH": str(fakebin) + ":" + os.environ["PATH"]}
        self.start(env=env)
        for action in ("disconnect", "quit"):
            if action == "quit":
                marker.unlink(); self.attach()
            self.pending()
            self.server.send("approve\n")
            deadline = time.monotonic() + 15
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue(marker.exists(), "host build did not start")
            processes = json.loads(marker.read_text())
            self.assert_status_responsive()
            start = time.monotonic()
            if action == "disconnect":
                self.client.close()
                self.assert_stopped()
            else:
                self.server.send("quit\n")
                self.assertEqual(self.server.wait(), 0)
                self.assertEqual(self.client.wait(), 0)
            self.assertLess(time.monotonic() - start, 2)
            self.assertFalse(Path(f"/proc/{processes[0]}").exists(), "owned Nix child was not reaped")
            for pid in processes:
                stat = Path(f"/proc/{pid}/stat")
                self.assertTrue(not stat.exists() or stat.read_text().split()[2] == 'Z', "build descendant still running")
            self.assertFalse(Path(f"/proc/{self.payload}").exists())
            self.assertFalse(self.directory.exists())
        print("EVIDENCE slow build: status, attachment disconnect and quit responsive; Nix client reaped and sandbox stopped", flush=True)

    def test_production_controller_death_kills_sandbox(self):
        self.start()
        self.client.send("while true; sleep 1; end &\n")
        os.kill(self.server.pid, signal.SIGKILL)
        self.server.wait()
        self.client.wait()
        deadline = time.monotonic() + 3
        while Path(f"/proc/{self.payload}").exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertFalse(Path(f"/proc/{self.payload}").exists())
        self.assertTrue(self.directory.exists())
        shutil.rmtree(self.directory)
