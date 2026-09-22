"""Private network namespaces with open TCP access; no external services/auth."""
import http.server
import json
import multiprocessing
import os
from pathlib import Path
import socket
import unittest

from daemon_support import Daemon, ROOT, RPC
from support import command
from terminal_support import Terminal


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"open-network-ok\n")

    def log_message(self, *_):
        pass


class NetworkTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                               f"git+file://{ROOT}#checks.x86_64-linux.network-goblins"])) / "bin/goblins"

    def setUp(self):
        self.d = Daemon(self.app)
        self.addCleanup(self.d.close)
        # Selecting a route does not send any packet to this address.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect(("192.0.2.1", 9))
            address = probe.getsockname()[0]
        self.server = http.server.ThreadingHTTPServer((address, 0), Handler)
        self.addCleanup(self.server.server_close)
        server_process = multiprocessing.get_context("fork").Process(target=self.server.serve_forever, daemon=True)
        server_process.start()
        self.addCleanup(server_process.join, 3)
        self.addCleanup(server_process.terminate)
        self.url = f"http://{address}:{self.server.server_port}"

    def terminal(self, session):
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect("network-test>")
        return terminal

    def fetch(self, terminal):
        terminal.send(f"curl -sS --max-time 5 {self.url}; printf 'FETCH=%s\\n' \"$?\"\n")
        terminal.expect(r"(?:^|\n)open-network-ok\n")
        terminal.expect(r"(?:^|\n)FETCH=0\n")
        terminal.expect("network-test>")

    def test_open_network_child_inheritance_and_interrupt(self):
        parent = self.d.start("online", agent_name="chief")["session"]
        terminal = self.terminal(parent)
        self.fetch(terminal)
        terminal.send("readlink /proc/self/ns/net; cat /etc/resolv.conf; printf 'PROXY=%s\\n' \"${HTTPS_PROXY-unset}\"\n")
        parent_ns = terminal.expect(r"(?:^|\n)(net:\[\d+\])\n").group(1).decode()
        self.assertNotEqual(parent_ns, os.readlink("/proc/self/ns/net"))
        terminal.expect("nameserver 10.0.2.3")
        terminal.expect("PROXY=unset")
        terminal.expect("network-test>")
        terminal.send("\x03")
        terminal.expect("network-test>")
        self.fetch(terminal)
        peer = RPC(self.d.state / parent / "resources/request.sock")
        self.addCleanup(peer.close)
        child = peer.call("sessions.start", {"key": "child", "name": "online", "agent_name": "scout", "detached": True})["session"]
        self.d.wait(lambda: self.d.get(child)["state"] == "running")
        attached = self.terminal(child)
        self.fetch(attached)
        attached.send("readlink /proc/self/ns/net\n")
        child_ns = attached.expect(r"(?:^|\n)(net:\[\d+\])\n").group(1).decode()
        self.assertNotEqual(parent_ns, child_ns)
        attached.expect("network-test>")
        attached.send("exit\n")
        self.d.wait(lambda: self.d.get(child)["state"] == "stopped")
        self.fetch(terminal)

    def test_explicit_offline_mode(self):
        session = self.d.start("offline")["session"]
        terminal = self.terminal(session)
        terminal.send(f"curl -sS --max-time 2 {self.url} >/tmp/output 2>/tmp/error; printf 'FETCH=%s\\n' \"$?\"\n")
        terminal.expect(r"(?:^|\n)FETCH=[1-9]\d*\n")
        self.assertFalse((self.d.state / session / "resources/network.log").exists())

    def test_daemon_death_stops_network_processes(self):
        session = self.d.start("online")["session"]
        self.terminal(session)
        children = set()
        for task in Path(f"/proc/{self.d.process.pid}/task").iterdir():
            children.update((task / "children").read_text().split())
        names = {pid: Path(f"/proc/{pid}/comm").read_text().strip() for pid in children}
        # The Nix pasta entry point is a symlink to the passt executable.
        pasta = [pid for pid, name in names.items() if name.startswith(("pasta", "passt"))]
        self.assertEqual(len(pasta), 1, names)
        # The scope keeper is independent of pasta. Track the complete owned
        # tree so this also verifies namespace cleanup before Docker activation.
        descendants = set(children)
        pending = list(children)
        while pending:
            pid = pending.pop()
            for task in Path(f"/proc/{pid}/task").iterdir():
                for child in (task / "children").read_text().split():
                    if child not in descendants:
                        descendants.add(child)
                        pending.append(child)
        identities = {pid: Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
                      for pid in descendants}
        helpers = [pid for pid in children if Path(f"/proc/{pid}/comm").read_text().startswith("goblins-mount")]
        self.assertEqual(len(helpers), 1)
        events = (self.d.state / session / "resources/events.jsonl").read_text().splitlines()
        payload = str(next(json.loads(line)["fields"]["pid"] for line in events if json.loads(line)["event"] == "started"))
        self.assertIn(payload, identities)
        self.d.process.kill()
        self.d.process.wait(timeout=5)

        def stopped():
            for pid, start in identities.items():
                try:
                    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                    if fields[0] != "Z" and fields[19] == start:
                        return False
                except FileNotFoundError:
                    pass
            return True

        self.d.wait(stopped, timeout=5)
