"""goblins exec against a real daemon: goblins-init runs host commands inside
running sandboxes, with pipes or a host PTY, cancellation and pinning."""
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest
import uuid

from daemon_support import Daemon
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


class ExecTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)
        self.agent = self.d.start(agent_name="gribble")

    def command(self, *args):
        return [str(self.d.app), "--state-dir", str(self.d.state), "exec", *args]

    def run_exec(self, *args, env=None, **kw):
        return subprocess.run(self.command(*args), capture_output=True, text=True, timeout=30,
                              env=env, **kw)

    def try_start(self, name):
        try:
            return self.d.start(agent_name=name)
        except ValueError:
            return None

    def wait_gone(self, marker, timeout=10):
        self.d.wait(lambda: not host_processes(marker), timeout=timeout)

    def test_streams_exit_codes_environment_and_errors(self):
        r = self.run_exec("gribble", "--", "sh", "-c", "echo out; echo err >&2; exit 4")
        self.assertEqual((r.returncode, r.stdout, r.stderr), (4, "out\n", "err\n"))
        # Bytes pass through exactly, including binary data and no newline.
        r = subprocess.run(self.command("gribble", "--", "cat"), input=b"\x00\xff\r\nend",
                           capture_output=True, timeout=30)
        self.assertEqual((r.returncode, r.stdout), (0, b"\x00\xff\r\nend"))
        r = self.run_exec("-w", "/tmp", "-e", "GOBLIN_X=1 2", "gribble", "--", "sh", "-c",
                          'pwd; echo "$GOBLIN_X"; test -t 0 || echo no-tty')
        self.assertEqual(r.stdout, "/tmp\n1 2\nno-tty\n", r.stderr)
        self.assertEqual(self.run_exec(self.agent["session"], "--", "true").returncode, 0)
        # A request near the frame limit arrives in several reads.
        r = self.run_exec("gribble", "--", "sh", "-c", 'echo "${#1}"', "sh", "y" * 15000)
        self.assertEqual((r.returncode, r.stdout), (0, "15000\n"), r.stderr)
        r = self.run_exec("gribble", "--", "no-such-command")
        self.assertEqual(r.returncode, 127)
        self.assertIn("no-such-command: command not found", r.stderr)
        r = self.run_exec("-w", "/no/such/dir", "gribble", "--", "true")
        self.assertEqual(r.returncode, 126)
        self.assertIn("cannot change directory", r.stderr)
        r = self.run_exec("nobody", "--", "true")
        self.assertEqual(r.returncode, 255)
        # With no command, the sandbox's own SHELL runs; the host's never crosses.
        shell = self.run_exec("gribble", "--", "sh", "-c", 'echo "$SHELL"').stdout.strip()
        r = self.run_exec("gribble", input='echo "running $SHELL"\n',
                          env={**os.environ, "SHELL": "/host/only/shell"})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(r.stdout.startswith(f"running {shell}\n"), r.stdout)

    def test_terminal_shell_resize_hangup_and_log(self):
        log = self.d.state / self.agent["session"] / "resources/events.jsonl"
        before = log.read_text().count("exec-started")
        # No command and -t: an interactive shell on a fresh host PTY.
        t = Terminal(self.command("-t", "gribble"))
        self.addCleanup(t.close)
        t.send("test -t 0 && echo TTY=yes; stty size; echo $((6*7))\n")
        t.expect(r"TTY=yes\n24 100\n42\n")
        self.d.wait(lambda: self.d.get(self.agent["session"])["shells"] == 1)
        t.resize(30, 120)
        t.send("stty size\n")
        t.expect(r"\n30 120\n")
        t.send("exit 3\n")
        self.assertEqual(t.wait(), 3)
        self.d.wait(lambda: self.d.get(self.agent["session"])["shells"] == 0)
        events = [json.loads(line) for line in log.read_text().splitlines()]
        started = [e for e in events if e["event"] == "exec-started"]
        self.assertEqual(len(started), before + 1)
        self.assertEqual(events[-1]["event"], "exec-exited")
        self.assertEqual(events[-1]["fields"]["outcome"]["code"], 3)
        # Non-interactive execs are not logged.
        self.run_exec("gribble", "--", "true")
        self.assertEqual(log.read_text().count("exec-"), 2 * (before + 1))
        # Killing the client hangs up its terminal: the shell gets SIGHUP.
        marker = uuid.uuid4().hex
        t = Terminal(self.command("-t", "gribble", "--", "sh", "-c",
                                  f"trap '' TERM; trap 'echo hup > /tmp/{marker}; exit 0' HUP; echo READY; "
                                  "while :; do sleep 0.1; done"))
        self.addCleanup(t.close)
        t.expect("READY")
        os.kill(t.pid, signal.SIGKILL)
        self.d.wait(lambda: self.run_exec("gribble", "--", "cat", f"/tmp/{marker}").stdout == "hup\n")

    def test_cancellation_signals_and_stdin_end(self):
        marker = "3" + str(time.time_ns() % 10**8)
        # SIGKILL of the client (Emacs' delete-process) leaves nothing of its
        # process group, even children that ignore SIGHUP and SIGTERM.
        client = subprocess.Popen(self.command(
            "gribble", "--", "sh", "-c",
            f"trap '' HUP TERM; sleep {marker}1 & sleep {marker}2; wait"))
        self.d.wait(lambda: len(host_processes(f"sleep {marker}")) == 2)
        self.d.wait(lambda: self.d.get(self.agent["session"])["execs"] == 1)
        client.kill()
        client.wait()
        self.wait_gone(f"sleep {marker}")
        self.d.wait(lambda: self.d.get(self.agent["session"])["execs"] == 0)
        # SIGINT, SIGTERM and SIGHUP reach the remote process group.
        for sig, code in ((signal.SIGINT, 9), (signal.SIGTERM, 10), (signal.SIGHUP, 11)):
            name = sig.name[3:]
            client = subprocess.Popen(
                self.command("gribble", "--", "sh", "-c",
                             f"trap 'echo got-{name}; exit {code}' {name}; echo READY; "
                             "while :; do sleep 0.1; done"),
                stdout=subprocess.PIPE, text=True)
            self.assertEqual(client.stdout.readline(), "READY\n")
            client.send_signal(sig)
            self.assertEqual(client.wait(timeout=10), code)
            self.assertEqual(client.stdout.read(), f"got-{name}\n")
            client.stdout.close()
        # Closing stdin ends a stdin-driven command, as eglot's shutdown does.
        client = subprocess.Popen(self.command("gribble", "--", "sh", "-c",
                                               "while read line; do echo \"<$line>\"; done; echo eof"),
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        client.stdin.write("a\nb\n")
        client.stdin.close()
        self.assertEqual(client.wait(timeout=10), 0)
        self.assertEqual(client.stdout.read(), "<a>\n<b>\neof\n")
        client.stdout.close()
        # Kill all execs: every client ends, and so does everything the execs
        # started, even processes that left their group (`set -m` puts
        # background jobs in groups of their own) or outlived their command. The agent and what it started keep running.
        agent = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", "gribble"])
        self.addCleanup(agent.close)
        # The terminal answers fish's queries only while it is read.
        agent.send(f"sleep {marker}5 &\n")
        agent.send("echo AGENT_STARTED\n")
        agent.expect(r"\nAGENT_STARTED\n")
        self.d.wait(lambda: host_processes(f"sleep {marker}5"))
        self.assertEqual(self.run_exec("gribble", "--", "sh", "-c",
                                       f"set -m; sleep {marker}6 & sleep 0.2").returncode, 0)
        clients = [subprocess.Popen(self.command("gribble", "--", "sh", "-c",
                                                 f"set -m; sleep {marker}{i} & set +m; sleep {marker}{i}"))
                   for i in (3, 4)]
        self.d.wait(lambda: self.d.get(self.agent["session"])["execs"] == 2)
        self.d.wait(lambda: all(len(host_processes(f"sleep {marker}{i}")) == 2 for i in (3, 4)))
        self.assertTrue(host_processes(f"sleep {marker}6"))
        self.d.rpc.call("sessions.kill_execs", {"session": "gribble"})
        for client in clients:
            self.assertEqual(client.wait(timeout=10), 128 + signal.SIGHUP)
        for i in (3, 4, 6):
            self.wait_gone(f"sleep {marker}{i}")
        self.assertTrue(host_processes(f"sleep {marker}5"))
        self.assertEqual(self.d.get(self.agent["session"])["state"], "running")

    def test_pinning_never_reaches_a_namesake(self):
        old = self.agent["session"]
        instance = self.d.rpc.init["instance"]
        pinned = ("--instance", instance, "--session", old, "gribble", "--", "true")
        self.assertEqual(self.run_exec(*pinned).returncode, 0)
        # The pin names one session: another goblin's designator is refused.
        self.d.start(agent_name="snaga")
        r = self.run_exec("--session", old, "snaga", "--", "true")
        self.assertEqual(r.returncode, 255)
        self.assertIn("is not session", r.stderr)
        self.d.rpc.call("sessions.stop", {"session": old})
        self.d.wait(lambda: self.d.get(old)["state"] == "stopped")
        # The name is reserved until the old worker has finished cleaning up.
        new = self.d.wait(lambda: self.try_start("gribble"))["session"]
        r = self.run_exec(*pinned)
        self.assertEqual(r.returncode, 255)
        self.assertIn(f"gribble ({old}) has stopped", r.stderr)
        # Unpinned, the name now means the new goblin.
        r = self.run_exec("gribble", "--", "true")
        self.assertEqual(r.returncode, 0)
        # Another daemon (a restart, or Emacs pointed elsewhere) is refused.
        other = Daemon()
        self.addCleanup(other.close)
        other.start(agent_name="gribble")
        r = subprocess.run([str(other.app), "--state-dir", str(other.state), "exec",
                            "--instance", instance, "--session", new, "gribble", "--", "true"],
                           capture_output=True, text=True, timeout=30)
        self.assertEqual(r.returncode, 255)
        self.assertIn("daemon instance changed", r.stderr)

    def test_agent_exit_tears_down_execs_and_reports_its_status(self):
        marker = "4" + str(time.time_ns() % 10**8)
        agent = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", "gribble"])
        self.addCleanup(agent.close)
        background = subprocess.Popen(self.command(
            "gribble", "--", "sh", "-c",
            f"trap '' HUP TERM; set -m; sleep {marker}1 & set +m; sleep {marker}2; wait"),
            stderr=subprocess.PIPE, text=True)
        self.d.wait(lambda: len(host_processes(f"sleep {marker}")) == 2)
        agent.send("exit 5\n")
        self.assertEqual(agent.wait(), 5)
        record = self.d.wait(lambda: (r if (r := self.d.get(self.agent["session"]))["state"] == "stopped"
                                      else None))
        self.assertEqual(record["exit_code"], 5)
        # Even the process that left the exec's group (job control) is gone.
        self.wait_gone(f"sleep {marker}")
        self.assertNotEqual(background.wait(timeout=10), 0)
        background.stderr.close()

    def test_control_channel_is_invisible_to_the_goblin(self):
        script = r"""
for d in /proc/[0-9]*; do
  if [ "$(head -c 29 "$d/cmdline" 2>/dev/null)" = /run/goblins/bin/goblins-init ]; then
    init=${d#/proc/}
    ls "$d/fd" >/dev/null 2>&1 && echo INIT_FD_READABLE || echo init-fds-hidden
  fi
  for f in "$d"/fd/*; do
    case "$(readlink "$f" 2>/dev/null)" in socket:*) echo "SOCKET $d $f";; esac
  done
done
ls /run/goblins
"""
        r = self.run_exec("gribble", "--", "sh", "-c", script)
        # Init and the keepers of execs (this one's too) are forks of init:
        # none is dumpable.
        self.assertGreaterEqual(r.stdout.count("init-fds-hidden"), 2, r.stdout)
        self.assertNotIn("INIT_FD_READABLE", r.stdout)
        self.assertNotIn("SOCKET", r.stdout)
        # Only the request socket is a file; init's channel has no name.
        listing = set(r.stdout.splitlines()) - {"init-fds-hidden"}
        self.assertEqual(listing, {"bin", "packages", "request.sock"})

    def test_editor_open_is_refused_without_an_editor(self):
        r = self.run_exec("gribble", "--", "goblins", "editor", "open", "--no-wait", "/tmp/x")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no editor is configured", r.stderr)


if __name__ == "__main__":
    unittest.main()
