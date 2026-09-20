"""Editor terminal relay uses exact session and daemon identities."""
import json
import subprocess
import unittest

from daemon_support import Daemon, RPC
from terminal_support import Terminal


class AttachmentTests(unittest.TestCase):
    def test_host_detach_by_name_and_id_leave_processes_running(self):
        d = Daemon()
        self.addCleanup(d.close)
        a = d.start(agent_name="snikk")
        b = d.start(agent_name="grub")
        command = [str(d.app), "--state-dir", str(d.state)]
        for target in ("missing-agent", "snikk"):
            result = subprocess.run([*command, "detach", target], text=True, capture_output=True)
            self.assertEqual(result.returncode, 1)
        other = Terminal([*command, "attach", "grub"])
        self.addCleanup(other.close)
        other.send("echo OTHER_READY\n")
        other.expect(r"(?:^|\n)OTHER_READY\n")
        client = Terminal([*command, "attach", "snikk"])
        self.addCleanup(client.close)
        client.send("set -g saved_value retained; printf 'PID=%s\\n' $fish_pid\n")
        pid = client.expect(r"(?:^|\n)PID=(\d+)\n").group(1)
        identity = d.get(a["session"])["identity"]
        for target in ("snikk", a["session"]):
            # Detach from the host while the sandbox is busy in a foreground job.
            client.send("echo BUSY; sleep 60\n")
            client.expect(r"(?:^|\n)BUSY\n")
            reply = json.loads(subprocess.check_output([*command, "detach", target], text=True))
            self.assertTrue(reply["accepted"])
            self.assertEqual(client.wait(), 0)
            record = d.get(a["session"])
            self.assertEqual(record["state"], "running")
            self.assertEqual(record["identity"], identity)
            self.assertTrue(record["terminal_detached"])
            self.assertTrue(d.get(b["session"])["terminal_attached"])
            result = subprocess.run([*command, "detach", target], text=True, capture_output=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("terminal is not attached", result.stderr)
            client = Terminal([*command, "attach", "snikk"])
            self.addCleanup(client.close)
            d.wait(lambda: d.get(a["session"])["terminal_attached"])
            client.send("\x03")
            client.send("printf 'STATE=%s,%s\\n' $fish_pid $saved_value\n")
            client.expect(r"(?:^|\n)STATE=" + pid.decode() + r",retained\n")
        other.send("echo OTHER_ALIVE; exit\n")
        other.expect(r"(?:^|\n)OTHER_ALIVE\n")
        self.assertEqual(other.wait(), 0)
        client.send("exit 7\n")
        self.assertEqual(client.wait(), 7)

    def test_configurations_and_exact_attachment(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        options = json.loads(subprocess.check_output(
            [str(daemon.app), "configurations"], text=True))
        self.assertEqual(options["configuration"], daemon.manifest)
        self.assertIn("shell", options["names"])
        self.assertEqual(
            options["descriptions"]["shell"],
            "fish running in a Goblins sandbox",
        )
        agent = daemon.start()
        for session, instance, error in (
            (agent["session"], "wrong-instance", "daemon instance changed"),
            (agent["agent_name"], daemon.rpc.init["instance"], "session identity mismatch"),
        ):
            terminal = Terminal([str(daemon.app), "--state-dir", str(daemon.state),
                                 "attach", session, "--instance", instance])
            self.addCleanup(terminal.close)
            terminal.expect(error)
            self.assertEqual(terminal.wait(), 1)
        # Rejected identities did not consume the terminal stream.
        terminal = Terminal([str(daemon.app), "--state-dir", str(daemon.state),
                             "attach", agent["session"], "--instance", daemon.rpc.init["instance"]])
        self.addCleanup(terminal.close)
        terminal.send("printf 'ATTACHED=%s\\n' yes; exit 7\n")
        terminal.expect(r"(?:^|\n)ATTACHED=yes\n")
        self.assertEqual(terminal.wait(), 7)

    def test_detach_and_reattach_keep_shell_state_and_exit_status(self):
        d = Daemon()
        self.addCleanup(d.close)
        client = Terminal([str(d.app), "--state-dir", str(d.state),
                           "run", "shell", "--name", "snikk"])
        self.addCleanup(client.close)
        client.send("set -g saved_value retained; cd /tmp; printf 'PID=%s\\n' $fish_pid\n")
        pid = client.expect(r"(?:^|\n)PID=(\d+)\n").group(1)
        session = d.rpc.call("sessions.list", {})[0]["id"]
        identity = d.get(session)["identity"]
        for _ in range(2):
            client.send("goblins detach; printf 'DETACHED_CODE=%s\\n' $status\n")
            self.assertEqual(client.wait(), 0)
            self.assertEqual(d.get(session)["state"], "running")
            self.assertEqual(d.get(session)["identity"], identity)
            client = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
            self.addCleanup(client.close)
            client.expect(r"(?:^|\n)DETACHED_CODE=0\n")
            client.send("printf 'STATE=%s,%s,%s\\n' $fish_pid $saved_value $PWD\n")
            client.expect(r"(?:^|\n)STATE=" + pid.decode() + r",retained,/tmp\n")
        client.send("exit 7\n")
        self.assertEqual(client.wait(), 7)

    def test_competing_attachment_and_reconnect_after_client_loss(self):
        d = Daemon()
        self.addCleanup(d.close)
        launch = d.start(agent_name="snikk")
        first = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
        self.addCleanup(first.close)
        first.send("echo FIRST_READY\n")
        first.expect(r"(?:^|\n)FIRST_READY\n")
        other = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
        self.addCleanup(other.close)
        other.expect("terminal is already attached")
        self.assertEqual(other.wait(), 1)
        first.close()
        d.wait(lambda: not d.get(launch["session"])["terminal_attached"])
        again = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
        self.addCleanup(again.close)
        again.send("echo RECONNECTED; exit 0\n")
        again.expect(r"(?:^|\n)RECONNECTED\n")
        self.assertEqual(again.wait(), 0)

    def test_background_output_continues_while_detached_and_attached_kill(self):
        d = Daemon()
        self.addCleanup(d.close)
        client = Terminal([str(d.app), "--state-dir", str(d.state),
                           "run", "shell", "--name", "snikk"])
        self.addCleanup(client.close)
        client.send("echo READY\n")
        client.expect(r"(?:^|\n)READY\n")
        session = d.rpc.call("sessions.list", {})[0]["id"]
        client.send("goblins detach; string repeat -n 200000 x; touch /tmp/output-finished\n")
        self.assertEqual(client.wait(), 0)
        # /proc sees the sandbox root without adding host authority to payloads.
        from pathlib import Path
        pid = d.get(session)["identity"]["pid"]
        d.wait(lambda: Path(f"/proc/{pid}/root/tmp/output-finished").exists())
        again = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
        self.addCleanup(again.close)
        again.send("echo OUTPUT_FINISHED\n")
        again.expect(r"(?:^|\n)OUTPUT_FINISHED\n")
        subprocess.check_call([str(d.app), "--state-dir", str(d.state), "kill", "snikk"],
                              stdout=subprocess.DEVNULL)
        d.wait(lambda: d.get(session)["state"] == "stopped")
        # A killed payload has no normal exit status; its relay must still exit.
        self.assertNotEqual(again.wait(), 0)

    def test_detach_is_session_local_and_kill_cleans_up_detached_session(self):
        d = Daemon()
        self.addCleanup(d.close)
        a = d.start(agent_name="snikk")
        b = d.start(agent_name="grub")
        client = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "snikk"])
        self.addCleanup(client.close)
        client.send("echo READY\n")
        client.expect(r"(?:^|\n)READY\n")
        endpoint = d.state / b["session"] / "resources/request.sock"
        for params in ({"session": a["session"]}, {}):
            rpc = RPC(endpoint)
            self.addCleanup(rpc.close)
            with self.assertRaises(ValueError):
                rpc.call("sessions.detach", params)
        self.assertTrue(d.get(a["session"])["terminal_attached"])
        # Detach does not cancel a concurrent package approval.
        pending, permission = d.pending(a["session"])
        self.addCleanup(pending.close)
        client.send("goblins detach\n")
        self.assertEqual(client.wait(), 0)
        self.assertEqual(d.rpc.call("permissions.get", {"request": permission["id"]})["state"], "pending")
        killed = json.loads(subprocess.check_output(
            [str(d.app), "--state-dir", str(d.state), "kill", "snikk"], text=True))
        self.assertTrue(killed["accepted"])
        d.wait(lambda: d.get(a["session"])["state"] == "stopped")
        d.wait(lambda: not (d.state / a["session"] / "resources").exists())
        self.assertEqual(d.get(b["session"])["state"], "running")


if __name__ == "__main__":
    unittest.main(verbosity=2)
