"""Networking as a goblin sees it: online in a private namespace, or offline; no external services/auth."""
import http.server
import multiprocessing
import os
import socket
import unittest

from build_support import fixture_app
from daemon_support import Daemon
from support import step
from terminal_support import Terminal


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"open-network-ok\n")

    def log_message(self, *_):
        pass


class NetworkWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("network-goblins")

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

    def test_online_and_offline_goblins(self):
        with step("an online goblin reaches the host from its own network namespace"):
            owner = self.d.start("online", agent_name="chief")["session"]
            terminal = self.terminal(owner)
            self.fetch(terminal)
            terminal.send("QUOTING_STYLE=literal readlink /proc/self/ns/net; cat /etc/resolv.conf; "
                          "printf 'PROXY=%s\\n' \"${HTTPS_PROXY-unset}\"\n")
            owner_ns = terminal.expect(r"(?:^|\n)(net:\[\d+\])\n").group(1).decode()
            self.assertNotEqual(owner_ns, os.readlink("/proc/self/ns/net"))
            terminal.expect("nameserver 10.0.2.3")
            terminal.expect("PROXY=unset")
            terminal.expect("network-test>")

        with step("Ctrl-C at the prompt leaves the network working"):
            terminal.send("\x03")
            terminal.expect("network-test>")
            self.fetch(terminal)

        with step("a member has a network namespace of its own, and leaving does not break the owner's"):
            member = self.d.member(owner, "online", agent_name="scout", key="scout")["session"]
            attached = self.terminal(member)
            self.fetch(attached)
            attached.send("QUOTING_STYLE=literal readlink /proc/self/ns/net\n")
            member_ns = attached.expect(r"(?:^|\n)(net:\[\d+\])\n").group(1).decode()
            # Without a scope, each sandbox has its own network namespace; a
            # scope is how goblins share one.
            self.assertNotEqual(owner_ns, member_ns)
            attached.expect("network-test>")
            attached.send("exit\n")
            self.d.wait(lambda: self.d.get(member)["state"] == "stopped")
            self.fetch(terminal)

        with step("an offline goblin reaches nothing"):
            offline = self.terminal(self.d.start("offline")["session"])
            offline.send(f"curl -sS --max-time 2 {self.url} >/tmp/output 2>/tmp/error; printf 'FETCH=%s\\n' \"$?\"\n")
            offline.expect(r"(?:^|\n)FETCH=[1-9]\d*\n")
