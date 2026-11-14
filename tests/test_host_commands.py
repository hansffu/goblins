"""Host commands: `goblin host run`, approved by the host, runs outside the sandbox.

The jobs run as this user on this machine. Each test uses its own daemon and
state directory, so no job reaches the user's own server.
"""
import json
import os
from pathlib import Path
import re
import signal
import tempfile
import time
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, ROOT
from support import step
from terminal_support import Terminal

PROMPT = "host-test>"


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    stat = Path(f"/proc/{pid}/stat").read_text()
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


class HostCommandFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("host-goblins")

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblins-host-test-")
        self.addCleanup(temp.cleanup)
        # Under /tmp: a shared project there is still shared with the host.
        self.project = Path(temp.name) / "project"
        self.project.mkdir()
        self.d = Daemon(self.app)
        self.addCleanup(self.d.close)

    def launch(self, name="host"):
        params = {"name": name, "configuration": self.d.manifest, "key": uuid.uuid4().hex,
                  "rows": 24, "cols": 100, "cwd": str(self.project)}
        session = self.d.rpc.call("sessions.start", params)["session"]
        record = self.d.wait(lambda: (r if (r := self.d.get(session))["state"] != "starting" else None), timeout=60)
        self.assertEqual(record["state"], "running", record)
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect(PROMPT, timeout=60)
        return session, terminal

    def send(self, terminal, text):
        marker = uuid.uuid4().hex[:8]
        # The terminal echoes the command line; the printf formats keep the
        # markers out of the echo, so only the command's own output is captured.
        terminal.send(f"printf 'BEGIN-%s\\n' {marker}; " + text +
                      f"; printf '\\nDONE-%s=%s\\n' {marker} \"$?\"\n")
        return marker

    def done(self, terminal, marker, timeout=60):
        result = terminal.expect(rf"BEGIN-{marker}\n([\s\S]*?)\nDONE-{marker}=(\d+)\n", timeout=timeout)
        terminal.expect(PROMPT)
        return result[1].decode(errors="replace"), int(result[2])

    def run_in(self, terminal, text, timeout=60):
        return self.done(terminal, self.send(terminal, text), timeout)

    def pending(self, session):
        return self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))

    def record(self, request):
        return next(r for r in self.d.permissions() if r["id"] == request)

    def events(self):
        """The whole communications log, page by page."""
        events, after = [], 0
        while True:
            page = self.d.rpc.call("communications.list", {"after": after})
            events += page["events"]
            after = page["cursor"]
            if after >= page["latest"]:
                return events

    def completed(self, request):
        """The communications log's completion of a request, if any."""
        return next((e for e in self.events() if e["kind"] == "operation.completed"
                     and e["data"]["request"] == request), None)

class HostCommandWorkflow(HostCommandFixture):
    def test_an_agent_runs_a_command_on_the_host(self):
        session, terminal = self.launch()

        with step("the goblin may ask for host commands"):
            self.assertIn("Host commands: available after approval", self.run_in(terminal, "goblin status")[0])

        with step("an approved command runs on the host and streams its output and exit code"):
            (self.project / "job.sh").write_text(
                'echo out; echo err >&2; pwd; echo "JOB=$GOBLINS_HOST_JOB"; echo "EXTRA=$EXTRA"\n'
                "test -e /run/goblins/request.sock && echo IN-SANDBOX || echo ON-HOST\n"
                'echo made-on-host > from-host; cat from-sandbox; echo "TMP=${TMPDIR-unset}"; exit 3\n')
            marker = self.send(terminal, "echo made-in-sandbox > from-sandbox; goblin host run --name 'host test' "
                                         "--reason 'host test' --env EXTRA='a b' -- bash job.sh")
            record = self.pending(session)
            self.assertEqual(record["kind"], "host-command")
            # The name, and where the output will be, are known before approval;
            # the start time only once the job is sent.
            self.assertEqual((record["name"], record["preview"]["name"]), ("host test", "host test"))
            self.assertEqual(record["preview"]["description"].splitlines()[1], "Name: host test")
            self.assertTrue(record["package"].startswith("host test: "), record["package"])
            self.assertNotIn("started_at", record)
            self.assertEqual(record["log"], str(self.d.state / session / "resources" / "host-jobs" / record["id"]
                                                / "output.log"))
            before = int(time.time())
            self.assertIn("bash job.sh", record["package"])
            self.assertIn(str(self.project), record["package"])
            self.assertIn("Runs on the host as you", record["preview"]["description"])
            self.assertIn("EXTRA='a b'", record["preview"]["description"])
            self.d.decide(record, True)
            output, code = self.done(terminal, marker)
            self.assertEqual(code, 3, output)
            for line in ["out", "err", str(self.project), f"JOB={record['id']}", "EXTRA=a b", "ON-HOST",
                         "made-in-sandbox"]:
                self.assertIn(line, output)
            self.assertEqual((self.project / "from-host").read_text(), "made-on-host\n")
            # Plain /tmp: nested daemons' socket paths stay short.
            self.assertIn("TMP=unset", output)
            final = self.record(record["id"])
            self.assertEqual((final["state"], final["message"]), ("finished", "exit code 3"))
            self.assertTrue(before - 1 <= final["started_at"] <= int(time.time()) + 1, final)
            self.assertIn("made-in-sandbox", Path(final["log"]).read_text())
            # The agent got the result itself: no inbox notice.
            completed = self.d.wait(lambda: self.completed(record["id"]))
            self.assertIsNone(completed["data"].get("notice"))
            attached = record["id"]

        with step("its output stays readable in the goblin, which cannot change it"):
            log = f"/run/goblins/host-jobs/{attached}/output.log"
            output, code = self.run_in(terminal, f"cat {log}")
            self.assertEqual(code, 0, output)
            self.assertIn("made-in-sandbox", output)
            output, code = self.run_in(terminal, f"echo x >> {log}")
            self.assertNotEqual(code, 0, output)
            self.assertIn("Read-only file system", output)

        with step("a command the host denies does not run"):
            marker = self.send(terminal, "goblin host run --name job --reason denied -- touch ran")
            self.d.decide(self.pending(session), False)
            output, code = self.done(terminal, marker)
            self.assertEqual(code, 125, output)
            self.assertIn("denied", output)
            self.assertFalse((self.project / "ran").exists())

        with step("Ctrl-C withdraws a request"):
            terminal.send("goblin host run --name job --reason withdrawn -- touch ran\n")
            record = self.pending(session)
            terminal.send("\x03")
            terminal.expect(PROMPT)
            self.d.wait(lambda: self.record(record["id"])["state"] == "withdrawn")
            self.assertFalse((self.project / "ran").exists())

        with step("the goblin's own home and /tmp are not the host's"):
            for cwd in ["$HOME", "/tmp", "/run/goblins"]:
                output, code = self.run_in(terminal, f"goblin host run --name job --reason unshared --cwd {cwd} -- true")
                self.assertEqual(code, 125, output)
                self.assertIn("not shared with the host", output)
            self.assertFalse(self.d.permissions(session=session, state="pending"))

        with step("a detached job: logs, status, wait and cancel"):
            output, code = self.run_in(terminal, "goblin host run --name job --detach --reason long -- "
                                                 "bash -c 'echo started; exec sleep 300'")
            self.assertEqual(code, 0, output)
            request = json.loads(re.search(r"\{.*\}", output)[0])["request"]
            self.d.decide(self.pending(session), True)
            self.d.wait(lambda: self.record(request)["state"] == "running")
            self.d.wait(lambda: "started" in self.run_in(terminal, f"goblin host logs {request}")[0])
            status = json.loads(re.search(r"\{.*\}", self.run_in(terminal, f"goblin host status {request}")[0])[0])
            self.assertEqual(status["status"], "running")
            output, code = self.run_in(terminal, f"goblin host wait {request} --timeout 1")
            self.assertEqual(code, 124, output)
            listed = json.loads(re.search(r"\{.*\}", self.run_in(terminal, "goblin host list")[0])[0])["jobs"]
            self.assertEqual([j["request"] for j in listed if j["status"] == "running"], [request])
            self.run_in(terminal, f"goblin host cancel {request}")
            output, code = self.run_in(terminal, f"goblin host wait {request} --timeout 30")
            self.assertEqual(code, 128 + signal.SIGTERM, output)
            self.assertEqual(self.record(request)["state"], "cancelled")
            events = [e["kind"] for e in self.events() if request in json.dumps(e)]
            self.assertEqual(events[:3], ["operation.requested", "operation.decided", "operation.completed"])

        with step("the host lists and cancels running commands"):
            self.run_in(terminal, "goblin host run --name sleeper --detach --reason host-cancel -- sleep 300")
            record = self.pending(session)
            self.d.decide(record, True)
            self.d.wait(lambda: self.record(record["id"])["state"] == "running")
            listed = [j for j in json.loads(self.d.cli("host", "list").stdout)["jobs"] if j["status"] == "running"]
            self.assertEqual([(j["request"], j["name"]) for j in listed], [(record["id"], "sleeper")])
            self.assertIsInstance(listed[0]["started_at"], int)
            # The goblin sees the same name and start.
            status = self.run_in(terminal, f"goblin host status {record['id']}")[0]
            shown = json.loads(re.search(r"\{.*\}", status)[0])
            self.assertEqual((shown["name"], shown["started_at"]), ("sleeper", listed[0]["started_at"]))
            self.d.cli("host", "cancel", record["id"])
            self.d.wait(lambda: self.record(record["id"])["state"] == "cancelled")
            self.assertIn("cancelled by the host", self.record(record["id"])["message"])
            self.assertNotEqual(self.d.cli("host", "cancel", record["id"], check=False).returncode, 0)

        with step("processes a command leaves behind are stopped, whatever its ending"):
            leave = "setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > {name}.pid; exit {code}"
            for name, code in [("success", 0), ("failure", 4)]:
                marker = self.send(terminal, f"goblin host run --name job --reason cleanup -- "
                                             f"bash -c '{leave.format(name=name, code=code)}'")
                self.d.decide(self.pending(session), True)
                output, status = self.done(terminal, marker)
                self.assertEqual(status, code, output)
                pid = int((self.project / f"{name}.pid").read_text())
                self.assertFalse(alive(pid), f"{name}: background process {pid} survived")

        with step("orphans that exit are reaped while the job runs, as init would"):
            # An exiting PID namespace waits for its processes' reaping.
            marker = self.send(terminal, "goblin host run --name job --reason reap -- bash -c "
                                         "'(sleep 0.1 & echo $! > orphan.pid); "
                                         "while [ ! -e release ]; do sleep 0.05; done'")
            self.d.decide(self.pending(session), True)
            self.d.wait(lambda: (self.project / "orphan.pid").exists())
            orphan = int((self.project / "orphan.pid").read_text())
            self.d.wait(lambda: not Path(f"/proc/{orphan}").exists())
            (self.project / "release").touch()
            self.assertEqual(self.done(terminal, marker)[1], 0)

        with step("a host command can run a test server of its own"):
            cli = f"{self.d.binary} --runtime {self.d.manifest}"
            script = (f"state=$(mktemp -d); {cli} --state-dir $state server start >/dev/null && "
                      f"{cli} --state-dir $state server status && {cli} --state-dir $state server stop; "
                      "code=$?; rm -rf $state; exit $code")
            marker = self.send(terminal, f"goblin host run --name job --reason nested -- bash -c '{script}'")
            self.d.decide(self.pending(session), True)
            output, code = self.done(terminal, marker, timeout=120)
            self.assertEqual(code, 0, output)
            self.assertEqual(self.d.get(session)["state"], "running")

        with step("stopping the goblin stops its job and what the job started"):
            stopped = "setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > stopped.pid; exec sleep 300"
            self.run_in(terminal, f"goblin host run --name job --detach --reason stop -- bash -c '{stopped}'")
            record = self.pending(session)
            self.d.decide(record, True)
            self.d.wait(lambda: (self.project / "stopped.pid").exists())
            pid = int((self.project / "stopped.pid").read_text())
            self.assertTrue(alive(pid))
            self.d.rpc.call("sessions.stop", {"session": session})
            self.d.wait(lambda: not alive(pid), timeout=30)
            self.d.wait(lambda: self.record(record["id"])["state"] == "cancelled")


class HostJobCrashTests(HostCommandFixture):
    """A job dies with the daemon or the goblin supervisor that ran it."""

    def test_the_daemon_dying_stops_its_jobs(self):
        session, terminal = self.launch()
        self.run_in(terminal, "goblin host run --name job --detach --reason orphan -- bash -c 'setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > orphan.pid; exec sleep 300'")
        self.d.decide(self.pending(session), True)
        self.d.wait(lambda: (self.project / "orphan.pid").exists())
        pid = int((self.project / "orphan.pid").read_text())
        self.d.process.kill()
        self.d.process.wait(timeout=10)
        self.d.wait(lambda: not alive(pid), timeout=30)

    def test_the_supervisor_dying_stops_its_jobs(self):
        self.assertNotIn("GOBLINS_DEBUG_LOCAL_RUNTIME", self.d.env)
        session, terminal = self.launch()
        self.run_in(terminal, "goblin host run --name job --detach --reason supervisor-orphan -- bash -c 'setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > supervisor-orphan.pid; exec sleep 300'")
        record = self.pending(session)
        self.d.decide(record, True)
        self.d.wait(lambda: (self.project / "supervisor-orphan.pid").exists())
        orphan = int((self.project / "supervisor-orphan.pid").read_text())
        lease = str(self.d.state / session / "resources/.gc-lease")
        def supervisor():
            children = Path(f"/proc/{self.d.process.pid}/task/{self.d.process.pid}/children").read_text().split()
            found = []
            for child in children:
                try:
                    if any(str(fd.readlink()) == lease for fd in Path(f"/proc/{child}/fd").iterdir()):
                        found.append(int(child))
                except (FileNotFoundError, ProcessLookupError):
                    continue
            return found
        [pid] = self.d.wait(supervisor)
        self.assertIn(b"goblins-supervisor-bwrap", Path(f"/proc/{pid}/cmdline").read_bytes())
        os.kill(pid, signal.SIGKILL)
        self.d.wait(lambda: self.d.get(session)["state"] == "failed")
        self.d.wait(lambda: not alive(orphan), timeout=30)
        self.d.wait(lambda: not Path(f"/proc/{pid}").exists())
        self.d.wait(lambda: self.record(record["id"])["state"] == "cancelled")
        self.assertIsNone(self.d.process.poll(), "only the goblin supervisor should die")


@unittest.skipUnless(os.environ.get("GOBLINS_TEST_HOST_DOCKER"),
                     "set GOBLINS_TEST_HOST_DOCKER=1 to run the Docker suite through a host command")
class HostDockerAcceptance(unittest.TestCase):
    """Goblins' own Docker tests, run from a goblin through a host command.

    Needs what test_docker.py needs (newuidmap, subuid and subgid) and takes
    a few minutes. The goblin works in this checkout, so the tests see its
    git-tracked files.
    """

    def test_the_docker_suite_passes_through_a_host_command(self):
        app = fixture_app("host-goblins")
        d = Daemon(app)
        self.addCleanup(d.close)
        session = d.rpc.call("sessions.start", {"name": "host", "configuration": d.manifest, "key": uuid.uuid4().hex,
                                                "rows": 24, "cols": 200, "cwd": str(ROOT)})["session"]
        d.wait(lambda: d.get(session)["state"] == "running", timeout=60)
        terminal = Terminal([str(app), "--state-dir", str(d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect(PROMPT, timeout=60)
        terminal.send("goblin host run --name job --reason 'Docker tests' -- nix develop -c python3 -m unittest "
                      "discover -s tests -p test_docker.py -v; printf '\\nACCEPTANCE=%s\\n' $?\n")
        d.decide(d.wait(lambda: next(iter(d.permissions(session=session, state="pending")), None)), True)
        result = terminal.expect(r"([\s\S]*)\nACCEPTANCE=(\d+)\n", timeout=1800)
        self.assertEqual(result[2], b"0", result[1].decode(errors="replace")[-6000:])
        # The tests' own daemons never touched this one.
        self.assertEqual(d.get(session)["state"], "running")
        self.assertEqual(len(d.rpc.call("sessions.list", {})), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
