"""Daemon authority, concurrency, reconnect and required review regressions."""
import contextlib
import errno
import json
import os
from pathlib import Path
import select
import signal
import socket
import time
import unittest
from daemon_support import Daemon, RPC, frame, receive
from terminal_support import Terminal, screen_wait

class DaemonTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def test_simultaneous_sessions_reconnect_grant_and_decision_identity(self):
        d = self.d
        a, b = d.start(), d.start()
        self.assertNotEqual(d.get(a["session"])["identity"]["mountns"], d.get(b["session"])["identity"]["mountns"])
        pa, ra = d.pending(a["session"])
        pb, rb = d.pending(b["session"])
        self.addCleanup(pa.close); self.addCleanup(pb.close)
        d.rpc.close()  # No frontend remains. Requests and sessions must survive.
        d.rpc = d.host()
        sub = d.host(); self.addCleanup(sub.close)
        initial = sub.call("state.subscribe", {})
        self.assertEqual(len(initial["snapshot"]["sessions"]), 2)
        self.assertEqual(len(initial["snapshot"]["permissions"]), 2)
        self.assertEqual(sub.init["instance"], d.rpc.init["instance"])
        d.decide(rb, False)
        self.assertEqual(receive(pb.peer)["result"]["status"], "denied")
        event = receive(sub.peer)["params"]
        self.assertEqual(event["sequence"], initial["sequence"] + 1)
        self.assertTrue(any(p["state"] == "denied" for p in event["snapshot"]["permissions"]))
        with self.assertRaises(ValueError):
            d.decide(rb, True)
        pa.peer.sendall(b"x"); pa.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request": ra["id"]})["state"] == "withdrawn")
        with self.assertRaises(ValueError):
            d.decide(ra, True)
        pc, rc = d.pending(a["session"])
        self.addCleanup(pc.close)
        other = d.host(); self.addCleanup(other.close)
        d.decide(rc, True)
        with self.assertRaises(ValueError):
            d.decide(rc, True, other)
        self.assertEqual(receive(pc.peer)["result"]["status"], "ready")
        self.assertEqual(d.get(a["session"])["packages"], ["hello"])
        self.assertEqual(d.get(b["session"])["packages"], [])
        for launch, expected in [(a, "0"), (b, "127")]:
            with d.terminal(launch) as terminal:
                data = b""
                while b"workspace" not in data:
                    data += terminal.recv(8192)
                terminal.sendall(b"command -q hello; printf 'GRANT=%s\\n' $status\n")
                data = b""
                while f"GRANT={expected}\n".encode() not in data.replace(b"\r",b""):
                    try: data += terminal.recv(8192)
                    except TimeoutError: self.fail(repr(data))
            self.assertEqual(d.get(launch["session"])["state"], "running")
        d.rpc.call("sessions.stop", {"session": b["session"]})
        d.wait(lambda: d.get(b["session"])["state"] == "stopped")
        self.assertEqual(d.get(a["session"])["state"], "running")
        print("EVIDENCE simultaneous independent namespaces, requests, decisions, grants and frontend reconnect", flush=True)

    def test_required_stale_input_in_actual_frontends(self):
        d = self.d
        launch = d.start()
        for plain in (True, False):
            ui = Terminal([str(d.app), "--state-dir", str(d.state), "serve", *(["--plain"] if plain else [])])
            self.addCleanup(ui.close)
            pa, a = d.pending(launch["session"])
            self.addCleanup(pa.close)
            if plain:
                ui.expect(a["approval"])
            else:
                screen_wait(ui, lambda text: "Package: hello" in text)
                ui.send("\t")
                screen_wait(ui, lambda text: "Requests [focused]" in text)
            os.kill(ui.pid, signal.SIGSTOP)
            ui.send("approve " + a["approval"][:-2] if plain else "y")  # input for displayed A
            pa.peer.sendall(b"x"); pa.close()
            d.wait(lambda: d.rpc.call("permissions.get", {"request": a["id"]})["state"] == "withdrawn")
            pb, b = d.pending(launch["session"], "tree")
            self.addCleanup(pb.close)
            if plain: ui.send(a["approval"][-2:] + "\r")
            os.kill(ui.pid, signal.SIGCONT)
            time.sleep(.3)
            self.assertEqual(d.rpc.call("permissions.get", {"request": b["id"]})["state"], "pending")
            ui.send("approve\ry\r" if plain else "y\r")
            time.sleep(.2)
            self.assertEqual(d.rpc.call("permissions.get", {"request": b["id"]})["state"], "pending")
            if not plain:
                # Navigation and approval queued together must not approve an
                # unseen selection either. Only a later, fresh interaction may.
                ui.send("\x1b[By")
                time.sleep(.2)
                self.assertEqual(d.rpc.call("permissions.get", {"request": b["id"]})["state"], "pending")
                screen_wait(ui, lambda text: "Package: tree" in text)
            else:
                ui.expect(b["approval"])
            # A valid fresh command is still usable in the actual frontend.
            ui.send("deny " + b["approval"] + "\r" if plain else "n")
            def decided():
                while select.select([ui.fd], [], [], 0)[0]:
                    ui.buffer += os.read(ui.fd, 65536)
                return d.rpc.call("permissions.get", {"request": b["id"]})["state"] == "denied"
            try:
                d.wait(decided, timeout=10)
            except AssertionError:
                self.fail(f"frontend {plain=}, record={d.rpc.call('permissions.get', {'request':b['id']})}, output={ui.buffer[-8000:]!r}")
            self.assertEqual(receive(pb.peer)["result"]["status"], "denied")
            ui.send("quit\r" if plain else "q"); self.assertEqual(ui.wait(), 0)
            self.assertEqual(d.get(launch["session"])["state"], "running")
        print("EVIDENCE queued token text / fullscreen y for displayed A cannot approve replacement B", flush=True)

    def test_terminal_slow_reader_and_immediate_exit(self):
        d = self.d
        for count in (50000, 5):
            config = json.loads(Path(d.manifest).read_text())
            config["goblins"]["shell"]["args"] = ["--no-config", "-c", f"string repeat -n {count} x; printf 'FINAL-MARKER\\n'"]
            path = Path(d.temp.name) / f"short-{count}.json"
            path.write_text(json.dumps(config))
            ui = Terminal([d.binary, "--runtime", str(path), "--state-dir", str(d.state), "run", "shell"])
            self.addCleanup(ui.close)
            exited_before_drain = False
            data = bytearray(); deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if not select.select([ui.fd], [], [], .1)[0]:
                    continue
                try:
                    chunk = os.read(ui.fd, 512)
                except OSError as e:
                    if e.errno != errno.EIO: raise
                    break
                if not chunk: break
                data.extend(chunk)
                if count == 50000 and len(data) < count:
                    exited_before_drain |= any(r["state"] == "stopped" for r in d.rpc.call("sessions.list", {}))
                time.sleep(.01)  # partial stdout writes and backpressure
            if count == 50000: self.assertTrue(exited_before_drain)
            self.assertEqual(data.count(b"x"), count, bytes(data[-200:]))
            self.assertIn(b"FINAL-MARKER", data)
            self.assertEqual(ui.wait(), 0)
        print("EVIDENCE slow local PTY reader received all 50,000 bytes and final marker; immediate short command drained", flush=True)

    def test_broken_stdout_is_not_success(self):
        d = self.d
        config = json.loads(Path(d.manifest).read_text())
        config["goblins"]["shell"]["args"] = ["--no-config", "-c", "printf 'FINAL-MARKER\\n'"]
        path = Path(d.temp.name) / "broken-output.json"; path.write_text(json.dumps(config))
        ui = Terminal([d.binary,"--runtime",str(path),"--state-dir",str(d.state),"run","shell"], output="/dev/full")
        self.addCleanup(ui.close)
        ui.expect("terminal output incomplete")
        self.assertNotEqual(ui.wait(), 0)

    def test_all_concurrent_private_paths_are_protected_from_startup_binds(self):
        d = self.d; a = d.start(); b = d.start()
        alias = Path(d.temp.name) / "alias"; alias.symlink_to(d.state / a["session"] / "resources")
        for source in (d.state, d.state / a["session"] / "resources", d.state.parent, alias):
            config = json.loads(Path(d.manifest).read_text())
            spec = json.loads(Path(config["goblins"]["shell"]["build_spec"]).read_text())
            spec["ro_dirs"] = [str(source)]
            spec_path = Path(d.temp.name) / "bind-spec.json"; spec_path.write_text(json.dumps(spec))
            config["goblins"]["shell"]["build_spec"] = str(spec_path)
            path = Path(d.temp.name) / "bound.json"; path.write_text(json.dumps(config))
            launch = d.start(manifest=path, wait=False)
            record = d.wait(lambda: (r if (r := d.get(launch["session"]))["state"] == "failed" else None))
            self.assertIn("protected path", record["detail"])
            self.assertEqual(d.get(a["session"])["state"], "running")
            self.assertEqual(d.get(b["session"])["state"], "running")

    def test_postapproval_disconnect_and_normal_shutdown(self):
        d = self.d; a = d.start(); b = d.start()
        pids = [d.get(x["session"])["identity"]["pid"] for x in (a,b)]
        peer, record = d.pending(a["session"])
        d.decide(record, True); peer.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request":record["id"]})["state"] == "ready")
        self.assertEqual(d.get(a["session"])["packages"], ["hello"])
        d.process.terminate(); self.assertEqual(d.process.wait(timeout=10),0)
        self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in pids))
        self.assertTrue(all(not (d.state / x["session"]).exists() for x in (a,b)))

    def test_framing_authority_timeouts_and_launch_duplicates(self):
        d = self.d
        launch = d.start(key="launch_once")
        self.assertEqual(d.start(key="launch_once"), launch)
        with self.assertRaises(ValueError): d.start(name="other", key="launch_once")
        endpoint = d.state / launch["session"] / "resources/request.sock"
        for method, params in [("sessions.list", {}), ("permissions.decide", {}), ("permissions.request", {"kind": "package", "package": "hello", "reason": "test", "session": "other"})]:
            peer = RPC(endpoint)
            with self.assertRaises(ValueError): peer.call(method, params)
            peer.close()
        for mode in ("silent", "drip"):
            peer = socket.socket(socket.AF_UNIX); self.addCleanup(peer.close); peer.settimeout(5); peer.connect(str(endpoint))
            start = time.monotonic()
            if mode == "drip":
                for delay in (0, .85, 1.7, 2.55):
                    time.sleep(max(0, start + delay - time.monotonic())); peer.sendall(b"C")
            self.assertIn("error", receive(peer))
            self.assertLess(time.monotonic() - start, 3.6)
        # Fragmented initialization/request remains interoperable.
        peer = RPC(endpoint); self.addCleanup(peer.close)
        wire = frame({"jsonrpc": "2.0", "id": "same-id", "method": "permissions.request", "params": {"kind": "package", "package": "hello", "reason": "fragmented"}})
        for b in wire: peer.peer.sendall(bytes([b]))
        record = d.wait(lambda: next(iter(d.permissions(state="pending")), None))
        peer.peer.sendall(b"x"); peer.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request": record["id"]})["state"] == "withdrawn")
        fresh, record = d.pending(launch["session"]); self.addCleanup(fresh.close); d.decide(record, False)
        self.assertEqual(receive(fresh.peer)["result"]["status"], "denied")

if __name__ == "__main__": unittest.main(verbosity=2)
