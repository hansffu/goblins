"""`goblins exec` as an editor or a person uses it: commands with pipes or a
terminal inside a running goblin, signals, cancellation and cleanup; and what
ends when the agent itself exits. Pinning, parameter validation and the exec
event log are Rust unit tests."""
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

from build_support import fixture_app
from daemon_support import Daemon
from support import step
from terminal_support import Terminal


def host_processes(marker):
    """PIDs of host-visible `sleep ARG` processes whose "sleep ARG" starts
    with MARKER: the sleeps themselves, never a shell or client whose
    command line merely mentions them."""
    found = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            try:
                argv = (entry / "cmdline").read_bytes().split(b"\0")[:-1]
            except OSError:
                continue
            if (len(argv) == 2 and os.path.basename(argv[0]) == b"sleep"
                    and f"sleep {argv[1].decode(errors='replace')}".startswith(marker)):
                found.append(int(entry.name))
    return found


class ExecFixture(unittest.TestCase):
    def setUp(self):
        self.d = Daemon(fixture_app("exec-goblins"))
        self.addCleanup(self.d.close)
        self.agent = self.d.start(agent_name="gribble")
        self.marker = str(time.time_ns() % 10**9)

    def command(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), "exec", *args]

    def run_exec(self, *args, **kwargs):
        return subprocess.run(self.command(*args), capture_output=True, text=True, timeout=30, **kwargs)

    def gone(self, marker):
        self.d.wait(lambda: not host_processes(marker), timeout=10)

    def execs(self):
        return self.d.get(self.agent["session"])["execs"]


class ExecWorkflow(ExecFixture):
    def test_commands_in_a_running_goblin(self):
        d, marker = self.d, self.marker
        with step("output, errors and exit codes pass through"):
            r = self.run_exec("gribble", "--", "sh", "-c", "echo out; echo err >&2; exit 4")
            self.assertEqual((r.returncode, r.stdout, r.stderr), (4, "out\n", "err\n"))
            r = subprocess.run(self.command("gribble", "--", "cat"), input=b"\x00\xff\r\nend",
                               capture_output=True, timeout=30)
            self.assertEqual((r.returncode, r.stdout), (0, b"\x00\xff\r\nend"))
            self.assertEqual(self.run_exec(self.agent["session"], "--", "true").returncode, 0)

        with step("a working directory and environment, and no terminal unless asked"):
            r = self.run_exec("-w", "/tmp", "-e", "GOBLIN_X=1 2", "gribble", "--", "sh", "-c",
                              'pwd; echo "$GOBLIN_X"; test -t 0 || echo no-tty')
            self.assertEqual(r.stdout, "/tmp\n1 2\nno-tty\n", r.stderr)

        with step("failures say what went wrong"):
            r = self.run_exec("gribble", "--", "no-such-command")
            self.assertEqual(r.returncode, 127)
            self.assertIn("no-such-command: command not found", r.stderr)
            r = self.run_exec("-w", "/no/such/dir", "gribble", "--", "true")
            self.assertEqual(r.returncode, 126)
            self.assertIn("cannot change directory", r.stderr)
            self.assertEqual(self.run_exec("nobody", "--", "true").returncode, 255)

        with step("without a command the goblin's own shell runs; the host's never crosses"):
            shell = self.run_exec("gribble", "--", "sh", "-c", 'echo "$SHELL"').stdout.strip()
            r = self.run_exec("gribble", input='echo "running $SHELL"\n',
                              env={**os.environ, "SHELL": "/host/only/shell"})
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertTrue(r.stdout.startswith(f"running {shell}\n"), r.stdout)

        with step("a UTF-8 locale"):
            r = self.run_exec("gribble", "--", "bash", "-c", 'echo "L=$LC_ALL M=$(printf \'\\303\\251\' | wc -m)"')
            self.assertEqual((r.returncode, r.stdout), (0, "L=C.UTF-8 M=1\n"), r.stderr)

        with step("an interactive shell on a terminal follows its size and exit code"):
            t = Terminal(self.command("-t", "gribble"))
            self.addCleanup(t.close)
            t.send("test -t 0 && echo TTY=yes; stty size; echo $((6*7))\n")
            t.expect(r"TTY=yes\n24 100\n42\n")
            t.resize(30, 120)
            t.send("stty size\n")
            t.expect(r"\n30 120\n")
            t.send("exit 3\n")
            self.assertEqual(t.wait(), 3)

        with step("killing a terminal client hangs up its shell"):
            t = Terminal(self.command("-t", "gribble", "--", "sh", "-c",
                                      f"trap '' TERM; trap 'echo hup > /tmp/{marker}; exit 0' HUP; echo READY; "
                                      "while :; do sleep 0.1; done"))
            self.addCleanup(t.close)
            t.expect("READY")
            os.kill(t.pid, signal.SIGKILL)
            d.wait(lambda: self.run_exec("gribble", "--", "cat", f"/tmp/{marker}").stdout == "hup\n")

        with step("killing a client ends its whole remote process group"):
            # As Emacs' delete-process does; the children ignore HUP and TERM.
            client = subprocess.Popen(self.command(
                "gribble", "--", "sh", "-c", f"trap '' HUP TERM; sleep {marker}1 & sleep {marker}2; wait"))
            d.wait(lambda: len(host_processes(f"sleep {marker}")) == 2)
            client.kill()
            client.wait()
            self.gone(f"sleep {marker}")
            d.wait(lambda: self.execs() == 0)

        with step("a signal to the client reaches the command"):
            client = subprocess.Popen(self.command("gribble", "--", "sh", "-c",
                                                   "trap 'echo got-INT; exit 9' INT; echo READY; "
                                                   "while :; do sleep 0.1; done"),
                                      stdout=subprocess.PIPE, text=True)
            self.assertEqual(client.stdout.readline(), "READY\n")
            client.send_signal(signal.SIGINT)
            self.assertEqual(client.wait(timeout=10), 9)
            self.assertEqual(client.stdout.read(), "got-INT\n")
            client.stdout.close()

        with step("closing stdin ends a command that reads it"):
            client = subprocess.Popen(self.command("gribble", "--", "sh", "-c",
                                                   "while read line; do echo \"<$line>\"; done; echo eof"),
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            client.stdin.write("a\nb\n")
            client.stdin.close()
            self.assertEqual(client.wait(timeout=10), 0)
            self.assertEqual(client.stdout.read(), "<a>\n<b>\neof\n")
            client.stdout.close()

        with step("killing every exec ends what they started, even escaped jobs, but not the agent"):
            agent = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "gribble"])
            self.addCleanup(agent.close)
            agent.send(f"sleep {marker}5 &\n")
            agent.send("echo AGENT_STARTED\n")
            agent.expect(r"\nAGENT_STARTED\n")
            d.wait(lambda: host_processes(f"sleep {marker}5"))
            # `set -m` puts a background job in a group of its own.
            self.assertEqual(self.run_exec("gribble", "--", "sh", "-c",
                                           f"set -m; sleep {marker}6 & sleep 0.2").returncode, 0)
            clients = [subprocess.Popen(self.command("gribble", "--", "sh", "-c",
                                                     f"set -m; sleep {marker}{i} & set +m; sleep {marker}{i}"))
                       for i in (3, 4)]
            d.wait(lambda: self.execs() == 2)
            d.wait(lambda: all(len(host_processes(f"sleep {marker}{i}")) == 2 for i in (3, 4)))
            self.assertTrue(host_processes(f"sleep {marker}6"))
            d.rpc.call("sessions.kill_execs", {"session": "gribble"})
            for client in clients:
                self.assertEqual(client.wait(timeout=10), 128 + signal.SIGHUP)
            for i in (3, 4, 6):
                self.gone(f"sleep {marker}{i}")
            self.assertTrue(host_processes(f"sleep {marker}5"))
            self.assertEqual(d.get(self.agent["session"])["state"], "running")


class AgentExitWorkflow(ExecFixture):
    def test_the_agent_exits(self):
        d, marker, session = self.d, self.marker, self.agent["session"]
        resources = d.state / session / "resources"
        roots = list((resources / "roots").iterdir())
        self.assertTrue(roots)
        with step("the agent runs commands, escaped jobs and terminals"):
            agent = Terminal([str(d.app), "--state-dir", str(d.state), "attach", "gribble"])
            self.addCleanup(agent.close)
            background = subprocess.Popen(self.command(
                "gribble", "--", "sh", "-c",
                f"trap '' HUP TERM; set -m; sleep {marker}1 & set +m; sleep {marker}2; wait"),
                stderr=subprocess.PIPE, text=True)
            self.addCleanup(background.kill)
            created = subprocess.run([str(d.app), "--state-dir", str(d.state), "terminal", "create", "gribble",
                                      "--name", "long", "--", "sleep", f"{marker}3"],
                                     capture_output=True, text=True, timeout=30)
            self.assertEqual(created.returncode, 0, created.stderr)
            notified = self.run_exec("gribble", "--", "goblin", "terminal", "create", "--name", "watched",
                                     "--notify", "--", "sleep", f"{marker}4")
            self.assertEqual(notified.returncode, 0, notified.stderr)
            d.wait(lambda: len(host_processes(f"sleep {marker}")) == 4)

        with step("the agent exits 5, and its status is recorded"):
            agent.send("exit 5\n")
            self.assertEqual(agent.wait(), 5)
            record = d.wait(lambda: (r if (r := d.get(session))["state"] == "stopped" else None))
            self.assertEqual(record["exit_code"], 5)
            self.assertEqual(record["terminals"], [])

        with step("everything it started ends, even what left the exec's process group"):
            self.gone(f"sleep {marker}")
            self.assertNotEqual(background.wait(timeout=10), 0)
            background.stderr.close()

        with step("its resources and garbage-collection roots go, and its name is free again"):
            d.wait(lambda: not resources.exists())
            self.assertTrue(all(not root.exists() for root in roots))

            def again():
                try:
                    return d.start(agent_name="gribble")
                except ValueError:
                    return None
            self.assertNotEqual(d.wait(again)["session"], session)


if __name__ == "__main__":
    unittest.main()
