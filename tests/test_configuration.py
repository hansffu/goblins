"""Different Nix-built host clients launch through one foreground server."""
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest

from support import command
from test_connected import Terminal


class ConfigurationTests(unittest.TestCase):
    def test_client_selects_each_launch_without_restarting_server(self):
        with tempfile.TemporaryDirectory(prefix="goblins-configuration-test-") as directory:
            root = Path(directory)
            flake = "path:" + str(Path(__file__).resolve().parents[1])
            apps = [Path(command(["nix", "build", "--print-out-paths", "--out-link", root / name,
                                  flake + "#checks.x86_64-linux." + name])) / "bin/goblins"
                    for name in ("named-goblins", "updated-goblins")]
            original, updated = apps
            configuration = command(["nix", "eval", "--raw", flake + "#checks.x86_64-linux.named-goblins.config"])
            state = root / "control"
            server = Terminal([str(original), "--state-dir", str(state), "serve"])
            self.addCleanup(server.close)
            server.expect("Goblins: fishy, utility")
            server_pid = server.pid
            control_inode = (state / "serve.sock").stat().st_ino

            def request(path, name="fishy"):
                with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as peer:
                    peer.settimeout(3)
                    peer.connect(str(state / "serve.sock"))
                    peer.send(json.dumps({"v": 1, "op": "run", "name": name,
                                          "configuration": str(path), "rows": 24, "cols": 80}).encode())
                    reply = json.loads(peer.recv(4096))
                    self.assertEqual(reply["status"], "error")
                    return reply["message"]

            malformed = root / "malformed.json"
            malformed.write_text('{"goblins":')
            fifo = root / "fifo"
            os.mkfifo(fifo)
            oversized = root / "oversized.json"
            oversized.write_bytes(b" " * (1024 * 1024 + 1))
            for path, expected, name in (
                (root / "missing", "cannot open configuration", "fishy"),
                (malformed, "invalid configuration", "fishy"),
                (fifo, "regular file", "fishy"),
                (root, "regular file", "fishy"),
                (oversized, "exceeds 1 MiB", "fishy"),
                (configuration, "unknown goblin", "missing"),
            ):
                self.assertIn(expected, request(path, name))
                server.expect("Goblin stopped")
            self.assertIn("must be absolute", request("relative.json"))

            first = Terminal([str(original), "--state-dir", str(state), "run", "fishy"])
            self.addCleanup(first.close)
            server.expect("fishy connected")
            first.expect("workspace[>#]")
            first.send("printf 'BEFORE=%s\\n' $fish_pid; printf 'ORIGINAL=%s\\n' $GOBLIN_MARKER\n")
            fish_pid = first.expect(r"(?:^|\n)BEFORE=(\d+)\n").group(1)
            first.expect(r"(?:^|\n)ORIGINAL=literal \$HOME \$\(false\)\n")

            concurrent = Terminal([str(updated), "--state-dir", str(state), "run", "fishy"])
            self.addCleanup(concurrent.close)
            concurrent.expect("one goblin at a time")
            self.assertNotEqual(concurrent.wait(), 0)
            first.send("printf 'AFTER=%s\\n' $fish_pid; exit\n")
            self.assertEqual(first.expect(r"(?:^|\n)AFTER=(\d+)\n").group(1), fish_pid)
            self.assertEqual(first.wait(), 0)
            server.expect("Goblin stopped")

            # Both a changed definition of the same name and a newly added name
            # come from the run client's package, not the server's original one.
            for name in ("fishy", "added"):
                client = Terminal([str(updated), "--state-dir", str(state), "run", name])
                self.addCleanup(client.close)
                server.expect(name + " connected")
                client.expect("updated>")
                client.send("printf 'UPDATED=%s\\n' \"$GOBLIN_MARKER\"; test -n \"$BASH_VERSION\"; printf 'BASH=%s\\n' \"$?\"; tree --version; exit\n")
                client.expect(r"(?:^|\n)UPDATED=updated\n")
                client.expect(r"(?:^|\n)BASH=0\n")
                client.expect(r"(?:^|\n)tree v")
                self.assertEqual(client.wait(), 0)
                server.expect("Goblin stopped")
                os.kill(server_pid, 0)
                self.assertEqual((state / "serve.sock").stat().st_ino, control_inode)
            server.send("quit\n")
            self.assertEqual(server.wait(), 0)
            print("EVIDENCE one serve PID/socket: original Fish, changed Bash/env/packages, added name; invalid manifests rejected and active session preserved", flush=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
