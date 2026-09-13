"""Real terminal client and sandbox request executable through the daemon API."""
import json
import os
import re
import signal
import unittest
from daemon_support import Daemon
from terminal_support import Terminal

class ConnectedTests(unittest.TestCase):
    def test_connected_fish_live_grants_and_terminal_behavior(self):
        d = Daemon(); self.addCleanup(d.close)
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "serve", "--plain"]); self.addCleanup(ui.close)
        ui.expect("approval frontend")
        client = Terminal([str(d.app), "--state-dir", str(d.state), "shell"]); self.addCleanup(client.close)
        client.expect("workspace[>#]")
        client.send("printf 'BEFORE=%s\\n' $fish_pid; command -q hello; printf 'ABSENT=%s\\n' $status\n")
        pid = client.expect(r"(?:^|\n)BEFORE=(\d+)\n").group(1)
        client.expect(r"(?:^|\n)ABSENT=127\n")
        for package, approved in [("hello", False), ("hello", True), ("cowsay", True)]:
            client.send(f"goblins request-package {package} --reason 'actual sandbox client'; printf 'RESULT_CODE=%s\\n' $status\n")
            record = d.wait(lambda: next(iter(d.permissions(state="pending")), None))
            token = record["approval"]
            ui.expect("Type approve " + token)
            ui.send(("approve " if approved else "deny ") + token + "\n")
            ui.expect("DECISION")
            client.expect(r'"status":"' + ("ready" if approved else "denied") + '"')
            client.expect(r"(?:^|\n)RESULT_CODE=" + ("0" if approved else "1") + r"\n")
        client.send("hello; cowsay live-grant-ok; printf 'AFTER=%s\\n' $fish_pid\n")
        client.expect(r"(?:^|\n)Hello, world!\n"); client.expect("live-grant-ok")
        self.assertEqual(client.expect(r"(?:^|\n)AFTER=(\d+)\n").group(1), pid)
        client.send(f"goblins serve; printf 'RESTRICTED=%s\\n' $status; test -e {d.state}/host.sock; printf 'HOST_HIDDEN=%s\\n' $status; test -e /nix/var/nix/daemon-socket/socket; printf 'NIX_HIDDEN=%s\\n' $status\n")
        client.expect(r"(?:^|\n)RESTRICTED=2\n"); client.expect(r"(?:^|\n)HOST_HIDDEN=1\n"); client.expect(r"(?:^|\n)NIX_HIDDEN=1\n")
        client.send("sleep 60\n"); client.send("\x03"); client.send("echo INTERRUPTED\n"); client.expect(r"(?:^|\n)INTERRUPTED\n")
        import fcntl, struct, termios
        fcntl.ioctl(client.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 33, 91, 0, 0)); os.kill(client.pid, signal.SIGWINCH)
        client.send("sleep .2; stty size\n"); client.expect(r"(?:^|\n)33 91\n")
        ui.send("quit\n"); self.assertEqual(ui.wait(), 0)
        client.send("echo FRONTEND_CLOSED; exit\n"); client.expect(r"(?:^|\n)FRONTEND_CLOSED\n"); self.assertEqual(client.wait(), 0)
        print("EVIDENCE actual sandbox client denies/grants hello+cowsay in same Fish, retains isolation, Ctrl-C and resize; approval frontend closure leaves shell alive", flush=True)

if __name__ == "__main__": unittest.main(verbosity=2)
