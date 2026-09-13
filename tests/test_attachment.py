"""Editor terminal relay uses exact session and daemon identities."""
import json
import subprocess
import unittest

from daemon_support import Daemon
from terminal_support import Terminal


class AttachmentTests(unittest.TestCase):
    def test_configurations_and_exact_attachment(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        options = json.loads(subprocess.check_output(
            [str(daemon.app), "configurations"], text=True))
        self.assertEqual(options["configuration"], daemon.manifest)
        self.assertIn("shell", options["names"])
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
        # Rejected identities did not consume the one-lifetime terminal stream.
        terminal = Terminal([str(daemon.app), "--state-dir", str(daemon.state),
                             "attach", agent["session"], "--instance", daemon.rpc.init["instance"]])
        self.addCleanup(terminal.close)
        terminal.send("printf 'ATTACHED=%s\\n' yes; exit 7\n")
        terminal.expect(r"(?:^|\n)ATTACHED=yes\n")
        self.assertEqual(terminal.wait(), 7)


if __name__ == "__main__":
    unittest.main(verbosity=2)
