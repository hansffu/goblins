"""Payload limits survive the foreground supervisor while its capacity is raised."""
import json
import os
from pathlib import Path
import resource
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon


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
