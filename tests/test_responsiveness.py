"""Slow owned work, subscriber backpressure and daemon crash teardown."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sys
import tempfile
import time
import unittest
from daemon_support import Daemon, RPC, receive

class ResponsiveTests(unittest.TestCase):
    def slow_daemon(self, kind):
        root = tempfile.TemporaryDirectory(prefix="gs-"); self.addCleanup(root.cleanup)
        root = Path(root.name); bindir = root / "bin"; bindir.mkdir(); marker = root / "owned"
        tool = "nix-store" if kind == "startup" else "nix"
        real = shutil.which(tool)
        condition = '"eval" in sys.argv and any(".hello" in a for a in sys.argv)' if kind == "preview" else ('"build" in sys.argv and "--dry-run" not in sys.argv' if kind == "build" else '"--realise" in sys.argv')
        script = bindir / tool
        script.write_text(f'''#!{sys.executable}
import json,os,subprocess,sys,time
if {condition} and not os.path.exists({str(marker)!r}):
 child=subprocess.Popen([{shutil.which("sleep")!r},"60"])
 open({str(marker)!r},"w").write(json.dumps([os.getpid(),child.pid]))
 for _ in range(6000):
  if os.path.exists({str(marker.with_suffix('.release'))!r}): break
  time.sleep(.01)
os.execv({real!r},[{real!r},*sys.argv[1:]])
'''); script.chmod(0o700)
        d = Daemon(env={**os.environ, "PATH": str(bindir) + ":" + os.environ["PATH"]}); self.addCleanup(d.close)
        return d, marker

    def assert_reaped(self, d, marker):
        pids = json.loads(marker.read_text())
        d.wait(lambda: not Path(f"/proc/{pids[0]}").exists(), timeout=2)
        for pid in pids:
            stat = Path(f"/proc/{pid}/stat")
            self.assertTrue(not stat.exists() or stat.read_text().split()[2] == "Z")

    def test_slow_startup_does_not_block_other_session(self):
        d, marker = self.slow_daemon("startup")
        a = d.start(wait=False); d.wait(marker.exists)
        b = d.start()
        peer, record = d.pending(b["session"], "tree"); self.addCleanup(peer.close)
        start = time.monotonic(); d.decide(record, False)
        self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        d.rpc.call("sessions.stop", {"session": b["session"]})
        d.wait(lambda: d.get(b["session"])["state"] == "stopped", timeout=2)
        self.assertLess(time.monotonic() - start, 2)
        d.rpc.call("sessions.stop", {"session": a["session"]}); self.assert_reaped(d, marker)
        print("EVIDENCE slow startup A does not block launch, approval or stop B", flush=True)

    def test_resize_during_startup_is_applied_when_pty_arrives(self):
        d, marker = self.slow_daemon("startup")
        # A raw byte consumer is not a terminal emulator. Use a noninteractive
        # payload so Fish's capability negotiation cannot swallow test input.
        config = json.loads(Path(d.manifest).read_text())
        config["goblins"]["shell"]["args"] = ["--no-config", "-c", "exec /bin/sh -c 'read -r ready; stty size'"]
        manifest = marker.parent / "resize.json"
        manifest.write_text(json.dumps(config))
        a = d.start(manifest=manifest, wait=False); d.wait(marker.exists)
        self.assertTrue(d.rpc.call("sessions.resize", {"session":a["session"],"rows":40,"cols":110})["accepted"])
        marker.with_suffix(".release").touch()
        d.wait(lambda: d.get(a["session"])["state"] == "running")
        with d.terminal(a) as terminal:
            data = b""
            terminal.sendall(b"go\n")
            while b"40 110" not in data:
                try:
                    chunk = terminal.recv(8192)
                except TimeoutError:
                    self.fail(f"missing resized dimensions: output={data!r}; session={d.get(a['session'])}")
                self.assertTrue(chunk, data)
                data += chunk
        self.assert_reaped(d, marker)

    def test_trailing_byte_disconnect_cancels_and_reaps_preview(self):
        d, marker = self.slow_daemon("preview")
        a, b = d.start(), d.start()
        peer, record = d.pending(a["session"]); d.wait(marker.exists)
        peer.peer.sendall(b"x"); peer.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request": record["id"]})["state"] == "withdrawn", timeout=2)
        self.assert_reaped(d, marker)
        next_peer, record = d.pending(a["session"], "tree"); self.addCleanup(next_peer.close); d.decide(record, False)
        self.assertEqual(receive(next_peer.peer)["result"]["status"], "denied")
        self.assertEqual(d.get(b["session"])["state"], "running")
        print("EVIDENCE valid request + extra byte + close withdraws approval and reaps slow preview/descendant; next request proceeds", flush=True)

    def test_slow_build_and_stop_are_session_local(self):
        d, marker = self.slow_daemon("build")
        a, b = d.start(), d.start()
        peer, record = d.pending(a["session"]); self.addCleanup(peer.close); d.decide(record, True); d.wait(marker.exists)
        other, request = d.pending(b["session"], "tree"); self.addCleanup(other.close)
        start = time.monotonic(); d.decide(request, False)
        self.assertEqual(receive(other.peer)["result"]["status"], "denied")
        self.assertLess(time.monotonic() - start, 1)
        d.rpc.call("sessions.stop", {"session": a["session"]}); self.assert_reaped(d, marker)
        self.assertEqual(d.get(b["session"])["state"], "running")
        d.wait(lambda: not (d.state / a["session"] / "resources").exists(), timeout=2)
        print("EVIDENCE slow approved build A leaves B responsive; explicit stop reaps only A's owned work", flush=True)

    def test_normal_exit_kills_children_while_package_work_is_blocked(self):
        for kind in ("preview", "build"):
            with self.subTest(kind=kind):
                d, marker = self.slow_daemon(kind)
                # A raw socket is not a terminal emulator. Use a predictable
                # read/exit payload so Fish's terminal queries cannot consume
                # the test input before the shell is ready.
                config = json.loads(Path(d.manifest).read_text())
                config["goblins"]["shell"]["args"] = [
                    "--no-config", "-c", "exec /bin/sh -c 'read -r ready; exit 7'",
                ]
                manifest = marker.parent / "normal-exit.json"
                manifest.write_text(json.dumps(config))
                parent = d.start(manifest=manifest)
                child = d.rpc.call("sessions.start", {
                    "key": "child", "configuration": d.manifest, "name": "shell",
                    "parent": parent["session"], "rows": 24, "cols": 80,
                })
                d.wait(lambda: d.get(child["session"])["state"] == "running")
                peer, record = d.pending(parent["session"])
                self.addCleanup(peer.close)
                if kind == "build":
                    d.decide(record, True)
                d.wait(marker.exists)
                with d.terminal(parent) as terminal:
                    terminal.sendall(b"go\n")
                    d.wait(lambda: d.get(parent["session"])["state"] == "stopped", timeout=5)
                d.wait(lambda: d.get(child["session"])["state"] == "stopped", timeout=2)
                self.assertEqual(d.get(parent["session"])["exit_code"], 7)
                self.assert_reaped(d, marker)

    def test_slow_subscriber_disconnects_and_fresh_snapshot_resynchronizes(self):
        d = Daemon(); self.addCleanup(d.close); a = d.start()
        slow = d.host(); self.addCleanup(slow.close); slow.peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        initial = slow.call("state.subscribe", {})
        for index in range(110):
            peer = RPC(d.state / a["session"] / "resources/request.sock")
            peer.send("permissions.request", {"kind":"package","package":"hello","reason":"x" * 1024})
            record = d.wait(lambda: next(iter(d.permissions(state="pending")), None))
            d.decide(record, False); self.assertEqual(receive(peer.peer)["result"]["status"], "denied"); peer.close()
        # Drain until disconnected; any complete frames before it must be ordered.
        sequence = initial["sequence"]
        while True:
            try:
                event = receive(slow.peer)["params"]
            except EOFError:
                break
            sequence += 1; self.assertEqual(event["sequence"], sequence)
        fresh = d.host(); self.addCleanup(fresh.close)
        result = fresh.call("state.subscribe", {})
        self.assertEqual(len(result["snapshot"]["permissions"]), 110)
        self.assertTrue(all(p["state"] == "denied" for p in result["snapshot"]["permissions"]))
        self.assertGreater(result["sequence"], sequence)
        print("EVIDENCE slow subscriber disconnects without a false continuous sequence; fresh subscription recovers retained outcomes", flush=True)

    def test_abrupt_daemon_death_kills_multiple_payloads(self):
        d = Daemon(); self.addCleanup(d.close)
        launches = [d.start(), d.start()]
        pids = [d.get(a["session"])["identity"]["pid"] for a in launches]
        os.kill(d.process.pid, signal.SIGKILL); d.process.wait()
        deadline = time.monotonic() + 3
        while any(Path(f"/proc/{pid}").exists() for pid in pids) and time.monotonic() < deadline: time.sleep(.02)
        self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in pids))
        self.assertTrue(all((d.state / a["session"] / "resources").exists() for a in launches))
        print("EVIDENCE daemon SIGKILL kills both payload namespaces; crash-only filesystem leftovers remain", flush=True)

if __name__ == "__main__": unittest.main(verbosity=2)
