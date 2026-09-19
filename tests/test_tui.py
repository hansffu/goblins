"""Real fullscreen navigation, popup layout, buttons and input identity."""
import os
import select
import termios
import time
import unittest
import uuid
from daemon_support import Daemon, receive
from terminal_support import Terminal, screen_wait



class TuiTests(unittest.TestCase):
    def test_ownership_tree_navigation_folding_and_request_paths(self):
        d = Daemon(); self.addCleanup(d.close)
        parent = d.start(agent_name="parent")
        other = d.start(agent_name="other")
        def child(owner, name):
            launch = d.rpc.call("sessions.start", {
                "key":uuid.uuid4().hex,"name":"shell","agent_name":name,
                "configuration":d.manifest,"parent":owner["session"],"rows":24,"cols":100,
            })
            d.wait(lambda: d.get(launch["session"])["state"] == "running")
            return launch
        kid = child(parent, "kid")
        leaf = child(kid, "leaf")
        child(other, "kid")
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "tui"])
        self.addCleanup(ui.close)
        text = screen_wait(ui, lambda text: "leaf (shell)" in text and text.count("kid (shell)") == 2)
        self.assertIn("host", text)
        self.assertIn("├─ [-] parent (shell)", text)
        self.assertIn("│  └─ [-] kid (shell)", text)
        self.assertIn("│     └─ leaf (shell)", text)
        self.assertLess(text.index("parent (shell)"), text.index("leaf (shell)"))
        self.assertLess(text.index("leaf (shell)"), text.index("other (shell)"))
        # Navigation follows displayed tree order rather than raw snapshot order.
        ui.send("\x1b[B\r")
        screen_wait(ui, lambda text: "Packages [focused] · parent/kid (shell)" in text)
        ui.send("\x7f\x1b[D")
        screen_wait(ui, lambda text: "[+] kid (shell)" in text and "leaf (shell)" not in text)
        ui.send("\x1b[C")
        screen_wait(ui, lambda text: "leaf (shell)" in text)
        pending, request = d.pending(leaf["session"])
        self.addCleanup(pending.close)
        text = screen_wait(ui, lambda text: "Sandbox: parent/kid/leaf" in text)
        self.assertIn("parent/kid/leaf", text)
        ui.send("\tn")
        self.assertEqual(receive(pending.peer)["result"]["status"], "denied")
        ui.send("\t\x1b[A\x1b[D")
        screen_wait(ui, lambda text: "[+] parent (shell)" in text and text.count("kid (shell)") == 1)
        # State updates preserve collapsed branches.
        d.rpc.call("sessions.stop", {"session":leaf["session"]})
        d.wait(lambda: d.get(leaf["session"])["state"] == "stopped")
        text = screen_wait(ui, lambda text: "[+] parent (shell)" in text)
        self.assertNotIn("leaf (shell)", text)
        ui.send("\x1b[C")
        screen_wait(ui, lambda text: "leaf (shell)" in text and "stopped" in text)
        ui.send("q"); self.assertEqual(ui.wait(), 0)

    def test_popup_list_focus_buttons_and_tty_restoration(self):
        d = Daemon(); self.addCleanup(d.close)
        a, b = d.start(agent_name="zoggit"), d.start()
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "tui"]); self.addCleanup(ui.close)
        before = screen_wait(ui, lambda text: "Sandboxes [focused]" in text)
        self.assertNotIn("Permission request", before)
        self.assertIn("zoggit (shell)", before)
        self.assertIn(b["agent_name"] + " (shell)", before)
        pa, ra = d.pending(a["session"]); self.addCleanup(pa.close)
        pb, rb = d.pending(b["session"], "tree"); self.addCleanup(pb.close)
        text = screen_wait(ui, lambda text: "Package: hello" in text and "tree" in text)
        self.assertIn("Reason: test", text)
        self.assertIn("Sandbox: zoggit", text)
        self.assertIn("[ No ]", text); self.assertIn("[ Yes ]", text)
        # The popup reserves rows beneath both lists, rather than overlaying them.
        rows = ui.screen.display
        self.assertLess(next(i for i,r in enumerate(rows) if "Requests" in r), next(i for i,r in enumerate(rows) if "Permission request" in r))
        ui.send("\t")
        screen_wait(ui, lambda text: "Requests [focused]" in text)
        ui.send("\x1b[B")
        screen_wait(ui, lambda text: "Package: tree" in text)
        ui.send("\x1b[A")
        screen_wait(ui, lambda text: "Package: hello" in text)
        ui.send("\x1b[200~y\r\x1b[201~")
        time.sleep(.2)
        self.assertEqual(d.rpc.call("permissions.get", {"request": ra["id"]})["state"], "pending")
        ui.send("\x1b")
        screen_wait(ui, lambda text: "Permission request" not in text)
        ui.send("y")  # A hidden popup cannot receive an approval.
        time.sleep(.1)
        self.assertEqual(d.rpc.call("permissions.get", {"request": ra["id"]})["state"], "pending")
        ui.send("\r")  # Reopen the selected request, without deciding it.
        screen_wait(ui, lambda text: "Package: hello" in text)
        ui.send("\r")  # Default button is No.
        self.assertEqual(receive(pa.peer)["result"]["status"], "denied")
        screen_wait(ui, lambda text: "denied" in text and "Permission request" not in text)
        ui.send("\x1b[B")
        screen_wait(ui, lambda text: "Package: tree" in text)
        ui.send("\x1b[C\r")  # Yes, then Enter.
        pb.peer.settimeout(30)
        self.assertEqual(receive(pb.peer)["result"]["status"], "ready")
        screen_wait(ui, lambda text: "ready" in text and "Permission request" not in text)
        pc, rc = d.pending(a["session"]); self.addCleanup(pc.close)
        screen_wait(ui, lambda text: text.count("hello") >= 2)
        ui.send("\x1b[B")
        screen_wait(ui, lambda text: "Package: hello" in text and "Status: pending" in text)
        ui.send("\x1b[A")
        screen_wait(ui, lambda text: "Package: tree" in text and "Status: ready" in text)
        ui.send("\x1b[B")
        screen_wait(ui, lambda text: "Package: hello" in text and "Status: pending" in text)
        ui.send("y")
        pc.peer.settimeout(30)
        self.assertEqual(receive(pc.peer)["result"]["status"], "ready")
        screen_wait(ui, lambda text: "ready" in text and "Permission request" not in text)
        ui.send("\t")
        screen_wait(ui, lambda text: "Sandboxes [focused]" in text)
        ui.send("q"); self.assertEqual(ui.wait(), 0)
        self.assertTrue(termios.tcgetattr(ui.fd)[3] & termios.ICANON)
        self.assertTrue(termios.tcgetattr(ui.fd)[3] & termios.ECHO)
        self.assertEqual(d.get(a["session"])["state"], "running")
        fresh = Terminal([str(d.app), "--state-dir", str(d.state), "tui"]); self.addCleanup(fresh.close)
        screen_wait(fresh, lambda text: "Status: ready" in text)
        fresh.send("q"); self.assertEqual(fresh.wait(), 0)

    def test_mouse_press_cannot_transfer_to_replacement(self):
        d = Daemon(); self.addCleanup(d.close); a = d.start()
        ui = Terminal([str(d.app), "--state-dir", str(d.state), "tui"]); self.addCleanup(ui.close)
        pa, ra = d.pending(a["session"]); self.addCleanup(pa.close)
        screen_wait(ui, lambda text: "Package: hello" in text and "[ Yes ]" in text)
        row = next(i for i,r in enumerate(ui.screen.display) if "[ Yes ]" in r)
        col = ui.screen.display[row].index("[ Yes ]")
        ui.send(f"\x1b[<0;{col+1};{row+1}M")
        time.sleep(.1)
        pa.peer.sendall(b"x"); pa.close()
        d.wait(lambda: d.rpc.call("permissions.get", {"request":ra["id"]})["state"] == "withdrawn")
        pb, rb = d.pending(a["session"], "tree"); self.addCleanup(pb.close)
        screen_wait(ui, lambda text: "Status: withdrawn" in text and "tree" in text)
        ui.send(f"\x1b[<0;{col+1};{row+1}m")
        time.sleep(.2)
        self.assertEqual(d.rpc.call("permissions.get", {"request":rb["id"]})["state"], "pending")
        ui.send("\t\x1b[B")
        screen_wait(ui, lambda text: "Package: tree" in text and "[ No ]" in text)
        row = next(i for i,r in enumerate(ui.screen.display) if "[ No ]" in r)
        col = ui.screen.display[row].index("[ No ]")
        ui.send(f"\x1b[<0;{col+1};{row+1}M\x1b[<0;{col+1};{row+1}m")
        self.assertEqual(receive(pb.peer)["result"]["status"], "denied")
        ui.send("q"); self.assertEqual(ui.wait(), 0)


if __name__ == "__main__": unittest.main(verbosity=2)
