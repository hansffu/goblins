"""Configuration-only Nix rebuilds launch concurrently on one live daemon."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from support import command
from terminal_support import Terminal
from daemon_support import Daemon, ROOT

class ConfigurationTests(unittest.TestCase):
    def test_client_selects_each_launch_without_restarting_daemon(self):
        with tempfile.TemporaryDirectory(prefix="gc-") as directory:
            root = Path(directory)
            apps = [Path(command(["nix", "build", "--print-out-paths", "--out-link", root / name,
                                  "path:" + str(ROOT) + "#checks.x86_64-linux." + name])) / "bin/goblins"
                    for name in ("named-goblins", "updated-goblins")]
            original, updated = apps
            d = Daemon(original); self.addCleanup(d.close)
            inode = (d.state / "host.sock").stat().st_ino
            malformed = root / "malformed.json"; malformed.write_text('{"goblins":')
            fifo = root / "fifo"; os.mkfifo(fifo)
            oversized = root / "oversized.json"; oversized.write_bytes(b" " * (1024 * 1024 + 1))
            incompatible = root / "incompatible.json"
            config = json.loads(Path(d.manifest).read_text()); config["helper_api"] = 999
            incompatible.write_text(json.dumps(config))
            for path, expected, name in [(root / "missing", "cannot open configuration", "fishy"),
                                         (malformed, "invalid configuration", "fishy"), (fifo, "regular file", "fishy"),
                                         (root, "regular file", "fishy"), (oversized, "exceeds 1 MiB", "fishy"),
                                         (d.manifest, "unknown goblin", "missing"), (incompatible, "incompatible", "fishy")]:
                launch = d.start(name, path, wait=False)
                record = d.wait(lambda: (r if (r := d.get(launch["session"]))["state"] == "failed" else None))
                self.assertIn(expected, record["detail"])
            with self.assertRaises(ValueError): d.start("fishy", "relative.json")
            first = Terminal([str(original), "--state-dir", str(d.state), "run", "fishy", "--name", "snikk"]); self.addCleanup(first.close)
            first.expect("workspace[>#]")
            record = d.rpc.call("sessions.get", {"session": "snikk"})
            self.assertEqual(record["name"], "fishy")
            self.assertEqual(record["agent_name"], "snikk")
            self.assertEqual(record["configuration"], d.manifest)
            first.send("printf 'BEFORE=%s\\n' $fish_pid; printf 'ORIGINAL=%s\\n' $GOBLIN_MARKER\n")
            pid = first.expect(r"(?:^|\n)BEFORE=(\d+)\n").group(1)
            first.expect(r"(?:^|\n)ORIGINAL=literal \$HOME \$\(false\)\n")
            for name in ("fishy", "added"):
                client = Terminal([str(updated), "--state-dir", str(d.state), "run", name]); self.addCleanup(client.close)
                client.expect("updated>")
                client.send("printf 'UPDATED=%s\\n' \"$GOBLIN_MARKER\"; test -n \"$BASH_VERSION\"; printf 'BASH=%s\\n' \"$?\"; tree --version; exit\n")
                client.expect(r"(?:^|\n)UPDATED=updated\n"); client.expect(r"(?:^|\n)BASH=0\n"); client.expect(r"(?:^|\n)tree v")
                self.assertEqual(client.wait(), 0)
                self.assertEqual((d.state / "host.sock").stat().st_ino, inode)
            first.send("printf 'AFTER=%s\\n' $fish_pid; exit\n")
            self.assertEqual(first.expect(r"(?:^|\n)AFTER=(\d+)\n").group(1), pid)
            self.assertEqual(first.wait(), 0)
            print("EVIDENCE one daemon/socket accepts concurrent rebuilt configurations/new names; existing Fish retains identity and literal env", flush=True)

if __name__ == "__main__": unittest.main(verbosity=2)
