"""Fullscreen API presentation: paste cannot decide; terminal modes restore."""
import os
import select
import termios
import time
import unittest
from daemon_support import Daemon, receive
from terminal_support import Terminal

class TuiTests(unittest.TestCase):
    def test_paste_is_not_approval_and_tty_is_restored(self):
        d = Daemon(); self.addCleanup(d.close); a = d.start()
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "serve"]); self.addCleanup(ui.close)
        peer, request = d.pending(a["session"]); self.addCleanup(peer.close)
        ui.expect(request["approval"])
        ui.send("\x1b[200~approve " + request["approval"] + "\n\x1b[201~")
        time.sleep(.2)
        self.assertEqual(d.rpc.call("permissions.get", {"request": request["id"]})["state"], "pending")
        ui.send("deny " + request["approval"] + "\r")
        self.assertEqual(receive(peer.peer)["result"]["status"], "denied")
        ui.send("quit\r"); self.assertEqual(ui.wait(), 0)
        self.assertTrue(termios.tcgetattr(ui.fd)[3] & termios.ICANON)
        self.assertTrue(termios.tcgetattr(ui.fd)[3] & termios.ECHO)
        self.assertEqual(d.get(a["session"])["state"], "running")
        fresh = Terminal([str(d.app), "--state-dir", str(d.state), "serve"]); self.addCleanup(fresh.close)
        fresh.expect("denied")
        fresh.send("quit\r"); self.assertEqual(fresh.wait(), 0)

if __name__ == "__main__": unittest.main(verbosity=2)
