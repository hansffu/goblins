"""allowNix: a person or an agent uses ordinary Nix commands inside a goblin, through the host's daemon.

Run on a host with a multi-user Nix install; builds go through its daemon.
"""
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon
from support import step
from terminal_support import Terminal

PROMPT = "nix-test>"
DEFAULT_SOCKET = "/nix/var/nix/daemon-socket/socket"


class Forwarder:
    """A Unix socket that relays each connection to the host daemon's socket."""

    def __init__(self, path):
        self.connections = 0
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        self.listener.listen()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                client, _ = self.listener.accept()
            except OSError:
                return
            self.connections += 1
            upstream = socket.socket(socket.AF_UNIX)
            upstream.connect(DEFAULT_SOCKET)
            for source, target in ((client, upstream), (upstream, client)):
                threading.Thread(target=self.pipe, args=(source, target), daemon=True).start()

    @staticmethod
    def pipe(source, target):
        try:
            while data := source.recv(65536):
                target.sendall(data)
        except OSError:
            pass
        finally:
            for end in (source, target):
                try:
                    end.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def close(self):
        self.listener.close()


class NixWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("nix-goblins")

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblins-nix-test-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.project = self.root / "project"
        self.project.mkdir()

    def daemon(self, env=None):
        d = Daemon(self.app, env=env)
        self.addCleanup(d.close)
        return d

    def launch(self, d, name="nix", wait=True):
        params = {"name": name, "configuration": d.manifest, "key": uuid.uuid4().hex,
                  "rows": 24, "cols": 100, "cwd": str(self.project)}
        session = d.rpc.call("sessions.start", params)["session"]
        if wait:
            record = d.wait(lambda: (r if (r := d.get(session))["state"] != "starting" else None), timeout=60)
            self.assertEqual(record["state"], "running", record)
        return session

    def attach(self, d, session):
        terminal = Terminal([str(self.app), "--state-dir", str(d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect(PROMPT, timeout=60)
        return terminal

    def run_in(self, terminal, text, timeout=60):
        terminal.send(text + "; printf '\\nNIX_TEST_STATUS=%s\\n' \"$?\"\n")
        result = terminal.expect(r"([\s\S]*?)\nNIX_TEST_STATUS=(\d+)\n", timeout=timeout)
        terminal.expect(PROMPT)
        return result[1].decode(errors="replace"), int(result[2])

    def run_ok(self, terminal, text, timeout=60):
        output, code = self.run_in(terminal, text, timeout)
        self.assertEqual(code, 0, output)
        return output

    def test_use_nix_in_a_goblin(self):
        d = self.daemon()
        owner = self.launch(d)
        terminal = self.attach(d, owner)

        with step("an allowNix goblin has Nix, talking to the host's daemon"):
            self.assertIn("Nix: available", self.run_ok(terminal, "goblin status"))
            self.run_ok(terminal, "command -v nix && test -S \"$NIX_DAEMON_SOCKET_PATH\" && test \"$NIX_REMOTE\" = daemon")

        with step("nix build makes a fresh output whose result link is a root on the host"):
            nonce = uuid.uuid4().hex
            (self.project / "nonce.nix").write_text(
                f'(import (builtins.getEnv "NIXPKGS") {{ }}).runCommand "goblins-nix-{nonce}" {{ }} "echo {nonce} > $out"\n')
            self.run_ok(terminal, "nix build --impure -f nonce.nix --out-link result", timeout=600)
            self.assertIn(nonce, self.run_ok(terminal, "cat result"))
            link = self.project / "result"
            self.assertEqual(Path(os.readlink(link)).read_text().strip(), nonce)
            roots = subprocess.run(["nix-store", "--gc", "--print-roots"], text=True, capture_output=True).stdout
            self.assertIn(str(link), roots)

        with step("nix shell runs a tool from nixpkgs"):
            self.assertIn("jq-", self.run_ok(terminal, "nix shell \"path:$NIXPKGS#jq\" -c jq --version", timeout=600))

        with step("asking for a package or a dev shell points the agent at Nix instead"):
            output, code = self.run_in(terminal, "goblin request-package jq --reason 'nix test'")
            self.assertEqual(code, 1, output)
            self.assertIn("nix shell nixpkgs#NAME", output)
            self.assertFalse(d.permissions(state="pending"))
            output, code = self.run_in(terminal, "goblin flake run '#app'")
            self.assertNotEqual(code, 0, output)
            self.assertIn("nix develop", output)

        with step("a member with another configuration has no Nix"):
            nixpkgs = self.run_ok(terminal, "printf '%s\\n' \"$NIXPKGS\"").split()[-1]
            self.assertTrue(nixpkgs.startswith("/nix/store/"), nixpkgs)
            member = d.member(owner, "plain", agent_name="kid", key="kid")["session"]
            plain = self.attach(d, member)
            self.assertIn("Nix: none", self.run_ok(plain, "goblin status"))
            self.run_ok(plain, f"! command -v nix && ! test -e {DEFAULT_SOCKET} && ! test -e {nixpkgs}")

    def test_a_nix_daemon_listening_elsewhere(self):
        socket_dir = self.root / "socket"
        socket_dir.mkdir()
        (socket_dir / "neighbour").write_text("host only\n")
        forwarder = Forwarder(socket_dir / "daemon.sock")
        self.addCleanup(forwarder.close)
        d = self.daemon({**os.environ, "NIX_DAEMON_SOCKET_PATH": str(socket_dir / "daemon.sock")})
        terminal = self.attach(d, self.launch(d))
        # Only the socket is bound, not the files beside it.
        self.assertIn(str(socket_dir / "daemon.sock"), self.run_ok(terminal, "printf '%s\\n' \"$NIX_DAEMON_SOCKET_PATH\""))
        self.run_ok(terminal, f"! test -e {socket_dir}/neighbour && ! test -e {DEFAULT_SOCKET}")
        self.run_ok(terminal, "nix store info", timeout=120)
        self.assertGreater(forwarder.connections, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
