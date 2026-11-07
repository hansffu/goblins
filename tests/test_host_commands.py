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
from daemon_support import Daemon, ROOT, RPC
from terminal_support import Terminal

PROMPT = "host-test>"


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    stat = Path(f"/proc/{pid}/stat").read_text()
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


class HostCommandTests(unittest.TestCase):
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

    def test_an_approved_command_runs_on_the_host_and_streams_its_output(self):
        session, terminal = self.launch()
        self.assertIn("Host commands: available after approval", self.run_in(terminal, "goblin status")[0])
        (self.project / "job.sh").write_text(
            'echo out; echo err >&2; pwd; echo "JOB=$GOBLINS_HOST_JOB"; echo "EXTRA=$EXTRA"\n'
            "test -e /run/goblins/request.sock && echo IN-SANDBOX || echo ON-HOST\n"
            'echo made-on-host > from-host; cat from-sandbox; echo "TMP=${TMPDIR-unset}"; exit 3\n')
        marker = self.send(terminal, "echo made-in-sandbox > from-sandbox; goblin host run --name 'host test' --reason 'host test' --env EXTRA='a b' -- bash job.sh")
        record = self.pending(session)
        self.assertEqual(record["kind"], "host-command")
        # The name, and where the output will be, are known before approval;
        # the start time only once the job is sent.
        self.assertEqual(record["name"], "host test")
        self.assertEqual(record["preview"]["name"], "host test")
        self.assertEqual(record["preview"]["description"].splitlines()[1], "Name: host test")
        self.assertTrue(record["package"].startswith("host test: "), record["package"])
        self.assertNotIn("started_at", record)
        self.assertEqual(record["log"], str(self.d.state / session / "resources" / "host-jobs" / record["id"] / "output.log"))
        before = int(time.time())
        self.assertIn("bash job.sh", record["package"])
        self.assertIn(str(self.project), record["package"])
        self.assertIn("Runs on the host as you", record["preview"]["description"])
        self.assertIn("EXTRA='a b'", record["preview"]["description"])
        self.d.decide(record, True)
        output, code = self.done(terminal, marker)
        self.assertEqual(code, 3, output)
        for line in ["out", "err", str(self.project), f"JOB={record['id']}", "EXTRA=a b", "ON-HOST", "made-in-sandbox"]:
            self.assertIn(line, output)
        self.assertEqual((self.project / "from-host").read_text(), "made-on-host\n")
        # Plain /tmp: nested daemons' socket paths stay short.
        self.assertIn("TMP=unset", output)
        final = self.record(record["id"])
        self.assertEqual((final["state"], final["message"]), ("finished", "exit code 3"))
        self.assertTrue(before - 1 <= final["started_at"] <= int(time.time()) + 1, final)
        self.assertIn("made-in-sandbox", Path(final["log"]).read_text())
        # The output stays readable in the sandbox, which cannot change it.
        log = f"/run/goblins/host-jobs/{record['id']}/output.log"
        output, code = self.run_in(terminal, f"cat {log}")
        self.assertEqual(code, 0, output)
        self.assertIn("made-in-sandbox", output)
        output, code = self.run_in(terminal, f"echo x >> {log}")
        self.assertNotEqual(code, 0, output)
        self.assertIn("Read-only file system", output)

    def test_names_are_required_restricted_and_default_for_older_clients(self):
        session, terminal = self.launch()
        output, code = self.run_in(terminal, "goblin host run --reason r -- true")
        self.assertNotEqual(code, 0, output)
        self.assertIn("--name", output)
        for bad in ["'a/b'", "' x'", "''", "'" + "a" * 33 + "'", "'caf\u00e9'"]:
            output, code = self.run_in(terminal, f"goblin host run --name {bad} --reason r -- true")
            self.assertNotEqual(code, 0, output)
            self.assertIn("1-32 characters", output)
        self.assertFalse(self.d.permissions(session=session, state="pending"))
        # An in-sandbox client without a name still works: the label is the
        # program's base name.
        peer = RPC(self.d.state / session / "resources/request.sock")
        peer.call("permissions.request", {"kind": "host-command", "reason": "old", "host": {
            "argv": ["/bin/true"], "cwd": str(self.project), "detach": True}})
        record = self.pending(session)
        self.assertEqual(record["name"], "true")
        self.assertEqual(record["preview"]["description"].splitlines()[1], "Name: true")
        self.d.decide(record, False)
        peer.close()

    def test_denied_withdrawn_and_unshared_commands_do_not_run(self):
        session, terminal = self.launch()
        marker = self.send(terminal, "goblin host run --name job --reason denied -- touch ran")
        self.d.decide(self.pending(session), False)
        output, code = self.done(terminal, marker)
        self.assertEqual(code, 125, output)
        self.assertIn("denied", output)
        self.assertFalse((self.project / "ran").exists())
        # Interrupting the client withdraws the request.
        terminal.send("goblin host run --name job --reason withdrawn -- touch ran\n")
        record = self.pending(session)
        terminal.send("\x03")
        terminal.expect(PROMPT)
        self.d.wait(lambda: self.record(record["id"])["state"] == "withdrawn")
        self.assertFalse((self.project / "ran").exists())
        # The sandbox's own home and /tmp are not the host's.
        for cwd in ["$HOME", "/tmp", "/run/goblins"]:
            output, code = self.run_in(terminal, f"goblin host run --name job --reason unshared --cwd {cwd} -- true")
            self.assertEqual(code, 125, output)
            self.assertIn("not shared with the host", output)
        self.assertFalse(self.d.permissions(session=session, state="pending"))

    def test_detached_jobs_status_logs_wait_and_cancel(self):
        session, terminal = self.launch()
        output, code = self.run_in(terminal, "goblin host run --name job --detach --reason long -- bash -c 'echo started; exec sleep 300'")
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
        self.assertEqual([j["request"] for j in listed], [request])
        self.run_in(terminal, f"goblin host cancel {request}")
        output, code = self.run_in(terminal, f"goblin host wait {request} --timeout 30")
        self.assertEqual(code, 128 + signal.SIGTERM, output)
        self.assertEqual(self.record(request)["state"], "cancelled")
        # Requested, decided and completed, in the communications log.
        events = self.d.wait(lambda: (e if len(e := [x for x in self.d.rpc.call("communications.list", {})["events"]
                                                    if request in json.dumps(x)]) >= 3 else None))
        self.assertEqual([e["kind"] for e in events][-3:],
                         ["operation.requested", "operation.decided", "operation.completed"])

    def test_the_host_lists_and_cancels_running_commands(self):
        session, terminal = self.launch()
        self.run_in(terminal, "goblin host run --name sleeper --detach --reason host-cancel -- sleep 300")
        record = self.pending(session)
        self.d.decide(record, True)
        self.d.wait(lambda: self.record(record["id"])["state"] == "running")
        listed = json.loads(self.d.cli("host", "list").stdout)["jobs"]
        self.assertEqual([(j["request"], j["name"], j["status"]) for j in listed],
                         [(record["id"], "sleeper", "running")])
        self.assertIsInstance(listed[0]["started_at"], int)
        # The sandbox sees the same name and start.
        sandbox = json.loads(re.search(r"\{.*\}", self.run_in(terminal, f"goblin host status {record['id']}")[0])[0])
        self.assertEqual((sandbox["name"], sandbox["started_at"]), ("sleeper", listed[0]["started_at"]))
        self.d.cli("host", "cancel", record["id"])
        self.d.wait(lambda: self.record(record["id"])["state"] == "cancelled")
        self.assertIn("cancelled by the host", self.record(record["id"])["message"])
        refused = self.d.cli("host", "cancel", record["id"], check=False)
        self.assertNotEqual(refused.returncode, 0)

    def test_processes_left_behind_are_stopped_whatever_the_ending(self):
        session, terminal = self.launch()
        leave = "setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > {name}.pid; exit {code}"
        for name, code in [("success", 0), ("failure", 4)]:
            marker = self.send(terminal, f"goblin host run --name job --reason cleanup -- bash -c '{leave.format(name=name, code=code)}'")
            self.d.decide(self.pending(session), True)
            output, status = self.done(terminal, marker)
            self.assertEqual(status, code, output)
            pid = int((self.project / f"{name}.pid").read_text())
            self.assertFalse(alive(pid), f"{name}: background process {pid} survived")
        # Orphans that exit are reaped while the job runs, as init would:
        # an exiting PID namespace waits for its processes' reaping.
        marker = self.send(terminal, "goblin host run --name job --reason reap -- bash -c 'echo $PPID > supervisor.pid; (sleep 0.1 &); sleep 2'")
        self.d.decide(self.pending(session), True)
        self.d.wait(lambda: (self.project / "supervisor.pid").exists())
        supervisor = int((self.project / "supervisor.pid").read_text())
        time.sleep(1)
        children = Path(f"/proc/{supervisor}/task/{supervisor}/children").read_text().split()
        zombies = [c for c in children if Path(f"/proc/{c}/stat").read_text().rsplit(")", 1)[1].split()[0] == "Z"]
        self.assertEqual(zombies, [], "the supervisor left exited orphans unreaped")
        self.assertEqual(self.done(terminal, marker)[1], 0)
        # Stopping the goblin stops its job and what it started.
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

    def test_host_commands_name_their_own_server(self):
        session, terminal = self.launch()
        cli = f"{self.d.binary} --runtime {self.d.manifest}"
        marker = self.send(terminal, f"goblin host run --name job --reason guard -- {cli} server status")
        self.d.decide(self.pending(session), True)
        output, code = self.done(terminal, marker)
        self.assertNotEqual(code, 0, output)
        self.assertIn("pass --state-dir", output)
        # A test daemon with its own state directory works.
        script = (f"state=$(mktemp -d); {cli} --state-dir $state server start >/dev/null && "
                  f"{cli} --state-dir $state server status && {cli} --state-dir $state server stop; "
                  "code=$?; rm -rf $state; exit $code")
        marker = self.send(terminal, f"goblin host run --name job --reason nested -- bash -c '{script}'")
        self.d.decide(self.pending(session), True)
        output, code = self.done(terminal, marker, timeout=120)
        self.assertEqual(code, 0, output)
        self.assertEqual(self.d.get(session)["state"], "running")

    def test_requests_are_shown_whole_or_refused_and_views_stay_healthy(self):
        owner, _ = self.launch()
        sessions = [owner]
        for i in range(12):
            # Raid members the owner runs, each asking on its own socket.
            sessions.append(self.d.member(owner, "host", agent_name=f"kid{i}", key=f"c{i}", wait=False)["session"])
        self.d.wait(lambda: all(self.d.get(s)["state"] == "running" for s in sessions), timeout=120)

        def request(session, quotes):
            client = RPC(self.d.state / session / "resources/request.sock")
            try:
                return client.call("permissions.request", {"kind": "host-command", "reason": "size", "host": {
                    "argv": ["true", "'" * quotes], "cwd": str(self.project), "detach": True}})
            except ValueError as error:
                return error.args[0]
            finally:
                client.close()
        # Too long to show whole: refused, never cut.
        self.assertIn("too long to show", str(request(sessions[0], 12000)))
        # Each fits on its own; together they would overflow approval views.
        results = [request(s, 6000) for s in sessions]
        accepted = [r for r in results if "request" in r]
        refused = [r for r in results if "request" not in r]
        self.assertTrue(accepted and refused, results)
        self.assertTrue(all("too many large requests" in str(r) for r in refused), refused)
        # Approval views keep working, and every accepted request is complete.
        snapshot = self.d.host().call("state.subscribe", {})["snapshot"]
        records = {r["id"]: r for r in snapshot["permissions"]}
        # Shown whole: each apostrophe quoted as '\'' by the display quoting.
        whole = "true '" + "'\\''" * 6000 + "'"
        for r in accepted:
            self.assertEqual(records[r["request"]]["preview"]["command"], whole)
        self.assertLess(len(json.dumps(snapshot)), 1024 * 1024)

    def test_a_goblin_without_host_commands_cannot_ask(self):
        session, terminal = self.launch("plain")
        output, code = self.run_in(terminal, "goblin host run --name job --reason no -- true")
        self.assertEqual(code, 125, output)
        self.assertIn("may not run host commands", output)
        self.assertIn("Host commands: none", self.run_in(terminal, "goblin status")[0])
        self.assertFalse(self.d.permissions(session=session))


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
