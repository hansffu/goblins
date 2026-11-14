"""User workflows when the daemon or one goblin's supervisor dies."""
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import unittest
import uuid

from build_support import fixture_app
from daemon_support import RPC
from support import step
from terminal_support import Terminal
from test_host_commands import HostCommandFixture, alive, PROMPT


class CrashWorkflows(HostCommandFixture):
    def supervisor(self, session):
        lease = str(self.d.state / session / "resources/.gc-lease")
        def find():
            children = Path(f"/proc/{self.d.process.pid}/task/{self.d.process.pid}/children").read_text().split()
            for child in children:
                try:
                    if any(str(fd.readlink()) == lease for fd in Path(f"/proc/{child}/fd").iterdir()):
                        return int(child)
                except (FileNotFoundError, ProcessLookupError):
                    pass
        return self.d.wait(find)

    def job(self, session, terminal):
        self.run_in(terminal, "goblin host run --name job --detach --reason crash -- bash -c "
                    "'setsid sleep 300 </dev/null >/dev/null 2>&1 & echo $! > orphan.pid; exec sleep 300'")
        request = self.pending(session)
        self.d.decide(request, True)
        self.d.wait(lambda: (self.project / "orphan.pid").exists())
        return request, int((self.project / "orphan.pid").read_text())

    def test_the_daemon_dies(self):
        d = self.d
        with step("a goblin runs a host job, alongside a networked goblin and a chain of helpers"):
            session, terminal = self.launch()
            _, orphan = self.job(session, terminal)
            network = fixture_app("network-goblins")
            _, manifest = re.search(r'exec (\S+) --runtime (\S+)', network.read_text()).groups()
            online = d.start("online", manifest=manifest)["session"]
            owner = d.start("host", agent_name="a")["session"]
            helper = d.member(owner, "host", agent_name="b")["session"]
            d.cli("stop", owner)
            d.wait(lambda: d.get(owner)["state"] == "stopped")
            d.cli("raid", "set-owner", d.get(helper)["raid"]["name"], "b")
            last = d.member(helper, "host", agent_name="c")["session"]
            directories = [d.state / s / "resources" for s in (owner, helper, last)]
            self.assertTrue(all(path.is_dir() for path in directories))
            payloads = [d.get(s)["identity"]["pid"] for s in (session, online, helper, last)]
            # Track the complete process tree, including pasta and namespace helpers.
            descendants, pending = set(), [str(d.process.pid)]
            while pending:
                parent = pending.pop()
                try:
                    for task in Path(f"/proc/{parent}/task").iterdir():
                        for child in (task / "children").read_text().split():
                            if child not in descendants:
                                descendants.add(child)
                                pending.append(child)
                except (FileNotFoundError, ProcessLookupError):
                    pass  # A stopped owner's terminal supervisor may already be reaped.
            identities, names = {}, {}
            for pid in descendants:
                try:
                    start = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
                    name = Path(f"/proc/{pid}/comm").read_text().strip()
                except (FileNotFoundError, ProcessLookupError):
                    continue
                identities[pid], names[pid] = start, name
            self.assertEqual(sum(n.startswith(("pasta", "passt")) for n in names.values()), len(payloads), names)
            self.assertTrue(any(n.startswith("goblins-mount") for n in names.values()), names)
            audit = next(d.state.glob("communications-*.jsonl"))

        with step("killing the daemon stops every payload, network helper and host job"):
            d.process.kill()
            d.process.wait(timeout=10)
            def stopped():
                for pid, start in identities.items():
                    try:
                        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                        if fields[0] != "Z" and fields[19] == start:
                            return False
                    except (FileNotFoundError, ProcessLookupError):
                        pass
                return True
            d.wait(stopped, timeout=5)
            d.wait(lambda: not alive(orphan), timeout=30)
            d.wait(lambda: all(not Path(f"/proc/{pid}").exists() for pid in payloads))
            self.assertTrue(all(path.exists() for path in directories))
            def leases_free():
                for path in directories:
                    with (path / ".gc-lease").open("rb") as lease:
                        try:
                            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        except BlockingIOError:
                            return False
                return True
            d.wait(leases_free, timeout=30)

        with step("a dry run keeps the leftovers; gc collects them and keeps the records and audit log"):
            audit_before = audit.read_bytes()
            history = {path: path.read_bytes() for path in d.history.glob("*.json")}
            self.assertTrue(history)
            self.assertIn("Would remove", d.cli("gc", "--dry-run").stdout)
            self.assertTrue(all(path.exists() for path in directories))
            d.cli("gc")
            self.assertFalse(any(path.exists() for path in directories))
            self.assertEqual(audit.read_bytes(), audit_before)
            for path, contents in history.items():
                self.assertEqual(path.read_bytes(), contents)

        with step("the same server can start again"):
            d.cli("server", "start")
            self.addCleanup(d.cli, "server", "stop")
            with_connection = RPC(d.state / "host.sock")
            try:
                self.assertEqual(with_connection.call("sessions.list", {}), [])
            finally:
                with_connection.close()

    def test_one_goblins_supervisor_dies(self):
        d = self.d
        with step("two goblins run; one has an attached client and a host job"):
            config = json.loads(Path(d.manifest).read_text())
            config["goblins"]["host"]["args"] = [
                "--noprofile", "--norc", "-c", "echo SERVER_UP; exec sleep 600"]
            config["goblins"]["host"]["on_attach"] = {
                "command": ["bash", "--noprofile", "--norc", "-c",
                            "echo CLIENT_UP; export PS1='host-test> '; exec bash --noprofile --norc -i"],
                "timeout": 10, "wait_for": []}
            config["goblins"]["host"]["on_detach"] = [
                "bash", "--noprofile", "--norc", "-c", "echo unexpected > /workspace/on-detach"]
            manifest = self.project.parent / "attached.json"
            manifest.write_text(json.dumps(config))
            victim = d.rpc.call("sessions.start", {
                "key": uuid.uuid4().hex, "name": "host", "configuration": str(manifest),
                "agent_name": "victim", "cwd": str(self.project), "rows": 24, "cols": 100})["session"]
            d.wait(lambda: d.get(victim)["state"] == "running", timeout=60)
            terminal = Terminal([str(self.app), "--state-dir", str(d.state), "attach", victim])
            self.addCleanup(terminal.close)
            terminal.expect("CLIENT_UP")
            terminal.expect(PROMPT)
            request, orphan = self.job(victim, terminal)
            healthy, healthy_terminal = self.launch()
            supervisor = self.supervisor(victim)
            payload = d.get(victim)["identity"]["pid"]

        with step("the supervisor dies: its client is interrupted and its job is cancelled"):
            os.kill(supervisor, signal.SIGKILL)
            failed = d.wait(lambda: (r if (r := d.get(victim))["state"] == "failed" else None))
            self.assertIn("supervisor", failed["detail"])
            self.assertTrue(failed["terminal_interrupted"])
            self.assertFalse(failed["terminal_attached"])
            self.assertFalse(failed["terminal_detached"])
            self.assertNotEqual(terminal.wait(), 0)
            d.wait(lambda: not alive(orphan), timeout=30)
            d.wait(lambda: not Path(f"/proc/{supervisor}").exists())
            d.wait(lambda: not Path(f"/proc/{payload}").exists())
            d.wait(lambda: self.record(request["id"])["state"] == "cancelled")
            self.assertFalse((self.project / "on-detach").exists())
            self.assertIsNone(d.process.poll())

        with step("the other goblin keeps working and the failed name can be reused"):
            self.assertEqual(d.get(healthy)["state"], "running")
            self.assertEqual(self.run_in(healthy_terminal, "true")[1], 0)
            # A crash leaves files for GC; every live holder must release its lease.
            lease = d.state / victim / "resources/.gc-lease"
            def released():
                try:
                    with lease.open("rb") as lock:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            return True
                        except BlockingIOError:
                            return False
                except FileNotFoundError:
                    return True
            d.wait(released, timeout=20)
            replacement = d.start("host", agent_name="victim")["session"]
            self.assertNotEqual(victim, replacement)


if __name__ == "__main__":
    unittest.main()
