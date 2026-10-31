"""Actual Supervised children: isolated death, terminal retirement and reaping."""
import fcntl
import json
import os
from pathlib import Path
import resource
import signal
import socket
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, frame, receive


class SupervisedTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.application = fixture_app("exec-goblins")

    def setUp(self):
        env = dict(os.environ)
        env.pop("GOBLINS_DEBUG_LOCAL_RUNTIME", None)
        self.d = Daemon(self.application, env=env)
        self.addCleanup(self.d.close)
        self.assertNotIn("GOBLINS_DEBUG_LOCAL_RUNTIME", self.d.env)

    def children(self):
        return set(map(int, Path(f"/proc/{self.d.process.pid}/task/{self.d.process.pid}/children").read_text().split()))

    def launch(self, manifest=None, name=None, wait=True):
        before = self.children()
        launch = self.d.start(manifest=manifest, agent_name=name, wait=False)
        children = self.d.wait(lambda: self.children() - before)
        self.assertEqual(len(children), 1, children)  # Fixture is unscoped.
        [pid] = children
        self.assertIn(b"goblins-supervisor-bwrap", Path(f"/proc/{pid}/cmdline").read_bytes())
        if wait:
            self.d.wait(lambda: self.d.get(launch["session"])["state"] == "running", timeout=60)
        return launch, pid

    def manifest(self, script, on_attach=None, on_detach=None):
        data = json.loads(Path(self.d.manifest).read_text())
        config = data["goblins"]["shell"]
        config["args"] = ["--noprofile", "--norc", "-c", script]
        config["on_attach"] = on_attach
        config["on_detach"] = on_detach
        path = Path(self.d.temp.name) / f"supervised-{uuid.uuid4().hex}.json"
        path.write_text(json.dumps(data))
        return path

    def assert_reaped(self, pid):
        self.d.wait(lambda: not Path(f"/proc/{pid}").exists())

    def test_former_debug_local_variable_cannot_select_an_inprocess_runtime(self):
        env = {**os.environ, "GOBLINS_DEBUG_LOCAL_RUNTIME": "1"}
        self.d = Daemon(self.application, env=env)
        self.addCleanup(self.d.close)
        self.assertEqual(self.d.env["GOBLINS_DEBUG_LOCAL_RUNTIME"], "1")
        # tests/run.py supplies a freshly built Cargo debug controller; direct
        # packaged invocations additionally check release behavior. Source
        # boundary tests reject the retired debug-only selector independently.
        if controller := os.environ.get("GOBLINS_LOCAL_CONTROLLER"):
            self.assertEqual(Path(f"/proc/{self.d.process.pid}/exe").resolve(), Path(controller).resolve())
        launch, supervisor = self.launch(name="always-supervised")
        self.assertNotEqual(supervisor, self.d.process.pid)
        terminal = self.d.terminal(launch)
        self.addCleanup(terminal.close)
        self.d.rpc.call("sessions.stop", {"session": launch["session"]})
        self.d.wait(lambda: not (self.d.state / launch["session"] / "resources").exists())
        while terminal.recv(4096):
            pass
        terminal.close()
        self.assert_reaped(supervisor)

    def test_foreground_supervisor_preserves_payload_nofile_and_ambient_capacity(self):
        parent_limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        for soft in (128, 512):
            with self.subTest(soft=soft):
                self.d = Daemon(self.application,
                                preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (soft, 512)))
                self.addCleanup(self.d.close)
                self.assertEqual(resource.prlimit(self.d.process.pid, resource.RLIMIT_NOFILE), (512, 512))
                manifest = self.manifest(
                    'printf "NOFILE:%s:%s\\n" "$(ulimit -Sn)" "$(ulimit -Hn)"; exec sleep 600')
                launch, supervisor = self.launch(manifest, name=f"nofile-{soft}")
                self.assertEqual(resource.prlimit(supervisor, resource.RLIMIT_NOFILE), (512, 512))
                payload = self.d.get(launch["session"])["identity"]["pid"]
                terminal = self.d.terminal(launch)
                self.addCleanup(terminal.close)
                output = bytearray()
                while b"\n" not in output:
                    chunk = terminal.recv(4096)
                    self.assertTrue(chunk, "payload exited before reporting its limits")
                    output.extend(chunk)
                self.assertIn(f"NOFILE:{soft}:512".encode(), bytes(output).splitlines())
                self.assertEqual(resource.prlimit(self.d.process.pid, resource.RLIMIT_NOFILE), (512, 512))
                self.assertEqual(resource.prlimit(supervisor, resource.RLIMIT_NOFILE), (512, 512))
                self.d.rpc.call("sessions.stop", {"session": launch["session"]})
                self.d.wait(lambda: not (self.d.state / launch["session"] / "resources").exists())
                while terminal.recv(4096):
                    pass
                terminal.close()
                self.assert_reaped(supervisor)
                self.d.wait(lambda: not Path(f"/proc/{payload}").exists())
                self.assertEqual(resource.getrlimit(resource.RLIMIT_NOFILE), parent_limits)

    def test_killing_one_supervisor_fails_only_its_goblin_and_releases_leases(self):
        victim, supervisor = self.launch(name="victim")
        other, healthy = self.launch(name="healthy")
        payload = self.d.get(victim["session"])["identity"]["pid"]
        os.kill(supervisor, signal.SIGKILL)
        failed = self.d.wait(lambda: (r if (r := self.d.get(victim["session"]))["state"] == "failed" else None))
        self.assertIn("supervisor", failed["detail"])
        self.assertTrue(failed["terminal_interrupted"])
        self.assertFalse(failed["terminal_attached"])
        self.assertFalse(failed["terminal_detached"])
        self.assertEqual(self.d.get(other["session"])["state"], "running")
        self.assertTrue(Path(f"/proc/{healthy}").exists())
        self.assert_reaped(supervisor)
        self.d.wait(lambda: not Path(f"/proc/{payload}").exists())
        lease = self.d.state / victim["session"] / "resources/.gc-lease"
        def unlocked():
            if not lease.exists():
                return True
            with lease.open("r+") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return True
                except BlockingIOError:
                    return False
        self.d.wait(unlocked, timeout=20)
        terminal = self.d.terminal(other)
        self.addCleanup(terminal.close)
        self.d.rpc.call("sessions.stop", {"session": other["session"]})
        self.d.wait(lambda: self.d.get(other["session"])["state"] == "stopped")
        while terminal.recv(4096):
            pass
        terminal.close()
        self.assert_reaped(healthy)

    def test_killed_stopped_terminal_does_not_revive_name_or_history(self):
        launch, supervisor = self.launch(self.manifest("printf unread-terminal-output"), name="kid", wait=False)
        session = launch["session"]
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped", timeout=60)
        self.assertTrue(Path(f"/proc/{supervisor}").exists(), "unread output must retain a terminal supervisor")
        record = self.d.wait(lambda: self.d.record(session))
        self.d.wait_record_released(record["id"])
        replacement, _ = self.launch(name="kid")
        self.assertNotEqual(replacement["session"], session)
        os.kill(supervisor, signal.SIGKILL)
        result = self.d.wait(lambda: (r if (r := self.d.get(session))["terminal_interrupted"] else None))
        self.assertEqual(result["state"], "stopped")
        self.assertFalse(result["terminal_attached"])
        self.assertFalse(result["terminal_detached"])
        self.assertEqual(self.d.get(replacement["session"])["state"], "running")
        self.d.wait_record_released(record["id"])
        self.assert_reaped(supervisor)

    def test_normal_terminal_drain_reaps_supervisor_without_interruption(self):
        launch, supervisor = self.launch(self.manifest("printf 'final-output\\n'"), wait=False)
        session = launch["session"]
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped", timeout=60)
        peer = socket.socket(socket.AF_UNIX)
        self.addCleanup(peer.close)
        peer.settimeout(10)
        peer.connect(launch["terminal"])
        self.assertTrue(receive(peer)["result"]["attached"])
        output = bytearray()
        while data := peer.recv(4096):
            output.extend(data)
        self.assertIn(b"final-output", output)
        peer.close()
        self.assert_reaped(supervisor)
        result = self.d.get(session)
        self.assertEqual(result["state"], "stopped")
        self.assertTrue(result["terminal_complete"])
        self.assertFalse(result["terminal_interrupted"])

    def test_supervisor_death_interrupts_client_without_running_on_detach(self):
        manifest = self.manifest("echo SERVER_UP; exec sleep 600", on_attach={
            "command": ["bash", "--noprofile", "--norc", "-c", "echo CLIENT_UP; exec sleep 601"],
            "timeout": 10, "wait_for": []}, on_detach=[
                "bash", "--noprofile", "--norc", "-c", "echo unexpected > /workspace/on-detach"])
        launch, supervisor = self.launch(manifest)
        peer = socket.socket(socket.AF_UNIX)
        self.addCleanup(peer.close)
        peer.settimeout(10)
        peer.connect(launch["attach"])
        peer.sendall(frame({"jsonrpc": "2.0", "method": "attach.hello", "params": {"version": 1, "rows": 24, "cols": 80}}))
        while (hello := receive(peer)).get("method") == "attach.waiting":
            pass
        self.assertTrue(hello["result"]["attached"])
        self.d.wait(lambda: self.d.get(launch["session"])["terminal_attached"])
        os.kill(supervisor, signal.SIGKILL)
        result = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] == "failed" else None))
        self.assertFalse(result["terminal_attached"])
        self.assertFalse(result["terminal_detached"])
        self.assertFalse((self.d.state / launch["session"] / "resources/workspace/on-detach").exists())
        self.assert_reaped(supervisor)
