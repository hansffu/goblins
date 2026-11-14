"""Two goblins ask for packages and a person decides in the approval frontends.

Fish goblins request packages with the in-sandbox CLI; the host answers in
`goblins tui --plain` and in the fullscreen `goblins tui`. An approved package
appears in the running shell; nothing else does.
"""
import json
from pathlib import Path
import subprocess
import termios
import unittest

from daemon_support import Daemon
from support import command, step
from terminal_support import Terminal, screen_wait


class PackageApprovalWorkflow(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def host(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), *args]

    def goblin(self, name, *argv, **kwargs):
        """A command in a goblin, as its agent runs it."""
        return subprocess.run(self.host("exec", name, "--", *argv), capture_output=True, text=True,
                              timeout=120, **kwargs)

    def pending(self):
        return self.d.wait(lambda: next(iter(self.d.permissions(state="pending")), None), timeout=60)

    def test_two_goblins_ask_for_packages(self):
        d = self.d
        with step("two goblins run, one in a terminal"):
            snikk = Terminal(self.host("run", "shell", "--name", "snikk"))
            self.addCleanup(snikk.close)
            snikk.expect("[>#]", timeout=60)
            d.start(agent_name="grub")
            snikk.send("printf 'BEFORE=%s\\n' $fish_pid; command -q cowsay; printf 'ABSENT=%s\\n' $status\n")
            pid = snikk.expect(r"(?:^|\n)BEFORE=(\d+)\n").group(1)
            snikk.expect(r"(?:^|\n)ABSENT=127\n")

        with step("a request waits in the plain frontend, also after it is closed and reopened"):
            plain = Terminal(self.host("tui", "--plain"))
            self.addCleanup(plain.close)
            plain.expect("approval frontend")
            # The command's old name still works, for recorded conversations.
            snikk.send("goblins request-package cowsay --reason 'tell a story'; "
                       "printf 'RESULT_CODE=%s\\n' $status\n")
            record = self.pending()
            self.assertEqual((record["package"], record["agent_name"]), ("cowsay", "snikk"))
            plain.expect("Type approve " + record["approval"])
            plain.send("quit\n")
            self.assertEqual(plain.wait(), 0)
            plain = Terminal(self.host("tui", "--plain"))
            self.addCleanup(plain.close)
            plain.expect("Type approve " + record["approval"])

        with step("an approval reaches the same shell, with the package's interpreter and data"):
            plain.send(f"approve {record['approval']}\n")
            plain.expect("DECISION")
            plain.send("quit\n")
            self.assertEqual(plain.wait(), 0)
            snikk.expect(r'"status":"ready"', timeout=120)
            snikk.expect(r"(?:^|\n)RESULT_CODE=0\n")
            # cowsay is a Perl script that reads its cow from share/.
            snikk.send("cowsay granted-moo; printf 'AFTER=%s\\n' $fish_pid; "
                       "printf 'TOOL=%s\\n' (realpath (command -v cowsay))\n")
            snikk.expect("granted-moo")
            self.assertEqual(snikk.expect(r"(?:^|\n)AFTER=(\d+)\n").group(1), pid)
            tool = Path(snikk.expect(r"(?:^|\n)TOOL=(/nix/store/[^\n]+)\n").group(1).decode())
            output = Path(*tool.parts[:4])

        with step("the package is read-only, nosuid and nodev, like every store path in the goblin"):
            mounts = self.goblin("snikk", "cat", "/proc/self/mountinfo")
            self.assertEqual(mounts.returncode, 0, mounts.stderr)
            store = {fields[4]: fields[5].split(",") for fields in map(str.split, mounts.stdout.splitlines())
                     if fields[4].startswith("/nix/store/")}
            self.assertIn(str(output), store)
            for path, options in store.items():
                self.assertTrue({"ro", "nosuid", "nodev"} <= set(options), (path, options))
            denied = self.goblin("snikk", "touch", f"{output}/written")
            self.assertNotEqual(denied.returncode, 0)

        with step("the host's garbage collection keeps it while the goblin runs"):
            roots = command(["nix-store", "--query", "--roots", output])
            self.assertIn(str(d.state), roots)

        with step("the other goblin does not see it"):
            self.assertEqual(self.goblin("grub", "test", "-e", str(output)).returncode, 1)

        with step("the fullscreen frontend denies by default"):
            ui = Terminal(self.host("tui"))
            self.addCleanup(ui.close)
            screen_wait(ui, lambda text: "grub (shell)" in text and "snikk (shell)" in text)
            asking = subprocess.Popen(self.host("exec", "grub", "--", "goblin", "request-package", "hello",
                                                "--reason", "say hello"), stdout=subprocess.PIPE, text=True)
            self.addCleanup(asking.kill)
            record = self.pending()
            self.assertEqual(record["agent_name"], "grub")
            screen_wait(ui, lambda text: "hello" in text)
            ui.send("\t")
            screen_wait(ui, lambda text: "Requests [focused]" in text)
            ui.send("\x1b[B")
            screen_wait(ui, lambda text: all(s in text for s in ("Sandbox: grub", "Package: hello",
                                                                  "Reason: say hello", "[ No ]", "[ Yes ]")))
            ui.send("\r")
            output_text, _ = asking.communicate(timeout=60)
            self.assertEqual(asking.returncode, 1)
            self.assertEqual(json.loads(output_text)["status"], "denied")
            screen_wait(ui, lambda text: "denied" in text and "Permission request" not in text)

        with step("an approved package that does not exist shows the host's diagnostic"):
            asking = subprocess.Popen(self.host("exec", "grub", "--", "goblin", "request-package",
                                                "goblinsMissingPackageForTest", "--reason", "missing"),
                                      stdout=subprocess.PIPE, text=True)
            self.addCleanup(asking.kill)
            record = self.pending()
            # Down selects the new row once the frontend shows it.
            screen_wait(ui, lambda text: "goblinsMissingPackageForTest" in text)
            ui.send("\x1b[B")
            screen_wait(ui, lambda text: "Package: goblinsMissingPackageForTest" in text)
            ui.send("y")
            output_text, _ = asking.communicate(timeout=120)
            self.assertEqual(asking.returncode, 1)
            self.assertEqual(json.loads(output_text)["status"], "error")
            screen_wait(ui, lambda text: "failed" in text and "Permission request" not in text)
            ui.send("\r")
            text = screen_wait(ui, lambda text: "Status: failed" in text and "cannot resolve package" in text)
            self.assertNotIn("see request details", text)
            self.assertEqual(d.get(record["session"])["state"], "running")

        with step("quitting the fullscreen frontend restores the terminal"):
            ui.send("q")
            self.assertEqual(ui.wait(), 0)
            modes = termios.tcgetattr(ui.fd)[3]
            self.assertTrue(modes & termios.ICANON and modes & termios.ECHO)

        with step("the goblins outlive their frontends"):
            snikk.send("echo FRONTENDS_CLOSED; exit\n")
            snikk.expect(r"(?:^|\n)FRONTENDS_CLOSED\n")
            self.assertEqual(snikk.wait(), 0)
            self.assertEqual(self.goblin("grub", "true").returncode, 0)


if __name__ == "__main__":
    unittest.main()
