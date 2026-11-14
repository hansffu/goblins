"""Raids and messaging as people and agents use them: a crew of host goblins,
an owner running helpers, and a crew that carries on when its owner leaves.

Goblins talk through the in-sandbox `goblin` CLI, the host through `goblins`.
No coding agents, model calls, or subscription credentials are used here.
"""
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest
import uuid

from daemon_support import Daemon, RPC, frame, receive
from support import step


class RaidWorkflow(unittest.TestCase):
    workspace = None

    def setUp(self):
        self.d = Daemon(workspace=self.workspace)
        self.addCleanup(self.d.close)

    def cli(self, *args, check=True, **kwargs):
        """The host CLI."""
        result = subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), *args],
                                env=self.d.env, capture_output=True, text=True, timeout=60, **kwargs)
        if check:
            self.assertEqual(result.returncode, 0, f"goblins {args}: {result.stdout}\n{result.stderr}")
        return result

    def goblin(self, session, script, check=True):
        """A command line in a goblin, as its agent would run it."""
        result = subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), "exec", session, "--",
                                 "sh", "-c", script], env=self.d.env, capture_output=True, text=True, timeout=60)
        if check:
            self.assertEqual(result.returncode, 0, f"{script}: {result.stdout}\n{result.stderr}")
        return result

    def json(self, session, script):
        return json.loads(self.goblin(session, script).stdout)

    def running(self, session):
        record = self.d.wait(lambda: (r if (r := self.d.get(session))["state"] != "starting" else None), timeout=60)
        self.assertEqual(record["state"], "running", record)
        return record

    def stopped(self, session):
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")

    def next_message(self, session, key=None):
        return self.json(session, f"goblin inbox next --key {key or uuid.uuid4().hex}")["message"]

    def lease_free(self, directory):
        """Whether nobody holds a lease on `directory` any more (probed, then let go)."""
        with open(directory / ".gc-lease", "rb") as lease:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(lease, fcntl.LOCK_UN)
            return True

    def log_file(self):
        return next(self.d.state.glob("communications-*.jsonl"))

    def logged(self):
        return [json.loads(line) for line in self.log_file().read_text().splitlines()]

    def filtered(self, session):
        result = self.cli("communications-log", "--session", session, "--json")
        return [json.loads(line) for line in result.stdout.splitlines()]


class CrewWorkflow(RaidWorkflow):
    """The host gathers goblins it started into a crew and talks with them."""

    def test_a_crew_of_host_goblins(self):
        d = self.d
        with step("the host gathers two goblins into a crew"):
            alpha = d.start(agent_name="alpha")["session"]
            beta = d.start(agent_name="beta")["session"]
            record = json.loads(self.cli("raid", "invite", "crew", "alpha", "beta").stdout)
            self.assertEqual(record["owner"], alpha)
            self.assertEqual([m["agent_name"] for m in record["members"]], ["alpha", "beta"])
            lines = self.cli("raid", "members", "crew").stdout.splitlines()
            self.assertEqual(lines[1].split()[:3], ["NAME", "ROLE", "CONFIGURATION"])
            self.assertEqual(lines[2].split()[:3], ["alpha", "-", "shell"])
            self.assertIn("owner", lines[2].split())
            listed = {r["id"]: r for r in json.loads(self.cli("list").stdout)}
            self.assertEqual((listed[beta]["raid"]["name"], listed[beta]["raid"]["owner"]), ("crew", alpha))

        with step("the host messages a goblin, a retry is the same message, and the sidebar counts it"):
            text = 'my message\n literal $(false) "quoted" 🐟'
            sent = self.cli("send", "alpha", "--message", text)
            host_message = json.loads(sent.stdout)
            self.assertEqual(host_message["sender"], "host")
            self.assertIn(host_message["key"], sent.stderr)
            retry = self.cli("send", "alpha", "--message", text, "--key", host_message["key"])
            self.assertEqual(json.loads(retry.stdout), host_message)
            d.wait(lambda: d.get(alpha)["inbox"] == 1)

        with step("crew members find and message each other by name"):
            output = self.goblin(alpha, "goblin status; goblin raid members; "
                                        "goblin send beta --message from-cli --key cli-send").stdout
            self.assertIn("Raid: crew (role: none, owner: alpha)", output)
            self.assertRegex(output, r"beta\s+-\s+shell\s+running")
            question = self.next_message(beta)
            self.assertEqual((question["sender"], question["body"]), (alpha, "from-cli"))
            self.goblin(beta, f"goblin reply {question['id']} --claim-generation {question['claim_generation']} "
                              "--message ok")

        with step("a goblin answers the host, which reads the answer in its inbox"):
            claim = self.next_message(alpha)
            self.assertEqual((claim["id"], claim["body"]), (host_message["id"], text))
            d.wait(lambda: d.get(alpha)["inbox"] == 1)  # beta's answer is still queued
            reply = self.json(alpha, f"goblin reply {claim['id']} --claim-generation {claim['claim_generation']} "
                                     "--message done")
            incoming = json.loads(self.cli("inbox", "next", "--key", "host-fetch").stdout)["message"]
            self.assertEqual((incoming["id"], incoming["body"]), (reply["reply"]["id"], "done"))
            self.cli("inbox", "complete", incoming["id"], "--claim-generation", str(incoming["claim_generation"]))
            answer = self.next_message(alpha)
            self.assertEqual((answer["sender"], answer["body"]), (beta, "ok"))
            self.goblin(alpha, f"goblin inbox complete {answer['id']} --claim-generation {answer['claim_generation']}")

        with step("membership grants no control over another member"):
            refused = self.goblin(alpha, "goblin stop beta", check=False)
            self.assertNotEqual(refused.returncode, 0, refused.stdout)
            self.assertEqual(d.get(beta)["state"], "running")

        with step("the communications log shows the crew's messages, and each goblin its invitation"):
            routes = {(e["actor"], e["data"]["change"]["route"], e["data"]["change"]["raid"])
                      for e in self.logged() if e["kind"] == "message.accepted"}
            self.assertEqual(routes, {(alpha, "raid", record["id"]), ("host", "host", None)})
            replied = {e["actor"] for e in self.logged() if e["kind"] == "message.replied"}
            self.assertEqual(replied, {alpha, beta})
            events = self.filtered("alpha")
            self.assertEqual([r["sequence"] for r in events], sorted(r["sequence"] for r in events))
            accepted = [e for e in events if e["kind"] == "message.accepted" and e["actor"] == "host"]
            self.assertEqual(accepted[0]["data"]["change"]["message"]["body"], text)
            joined = [e for e in self.filtered("beta") if e["kind"] == "raid.joined"]
            self.assertEqual([(e["actor"], e["sessions"], e["data"]["session"]) for e in joined],
                             [("host", [beta], beta)])

        with step("the host kicks a member, and kicking the owner pauses the crew"):
            self.assertEqual(d.rpc.call("raids.kick", {"raid": "crew", "session": "beta"}),
                             {"accepted": True, "dissolved": False, "paused": False})
            listed = {r["id"]: r for r in json.loads(self.cli("list").stdout)}
            self.assertIsNone(listed[beta]["raid"])
            self.assertEqual(listed[alpha]["raid"]["name"], "crew")
            self.cli("raid", "invite", "crew", "beta")
            self.assertEqual(d.rpc.call("raids.kick", {"raid": "crew", "session": "alpha"}),
                             {"accepted": True, "dissolved": False, "paused": True})
            paused = d.rpc.call("raids.members", {"raid": "crew"})
            self.assertEqual((paused["paused"], paused["owner"], paused["former_owner"]), (True, None, alpha))

        with step("the host dissolves the crew, and its log is written without other traffic"):
            self.assertEqual(d.rpc.call("raids.destroy", {"raid": "crew"}), {"accepted": True, "stopped": []})
            # Only the file is read: inbox, send and log calls flush the outbox themselves.
            dissolved = d.wait(lambda: next((e for e in self.logged() if e["kind"] == "raid.dissolved"), None))
            self.assertEqual((dissolved["actor"], dissolved["data"]["reason"], sorted(dissolved["sessions"])),
                             ("host", "destroyed", [beta]))
            for session in (alpha, beta):
                self.assertEqual(d.get(session)["state"], "running")
                self.assertIsNone(d.get(session)["raid"])

        with step("a dissolved crew's name makes a new raid without its former members"):
            second = d.rpc.call("raids.invite", {"raid": "crew", "sessions": [beta]})
            self.assertNotEqual(second["id"], record["id"])
            self.assertEqual(([m["id"] for m in second["members"]], second["owner"]), ([beta], beta))

        with step("stopping the server records what nobody received, and the offline log reads it"):
            first = json.loads(self.cli("send", "alpha", "--message", "first").stdout)
            claim = self.next_message(alpha)
            self.assertEqual(claim["id"], first["id"])
            unread = self.json(alpha, f"goblin reply {first['id']} --claim-generation {claim['claim_generation']} "
                                      "--message unread")
            pending = json.loads(self.cli("send", "alpha", "--message", "second").stdout)
            d.rpc.call("raids.invite", {"raid": "crew", "sessions": [alpha]})
            # A raid launch and the stop on separate connections, sent together
            # so they are dispatched in the same tick.
            launcher, stopper = RPC(d.state / "host.sock"), RPC(d.state / "host.sock")
            self.addCleanup(launcher.close)
            self.addCleanup(stopper.close)
            launcher.peer.sendall(frame({"jsonrpc": "2.0", "id": 1, "method": "sessions.start", "params": {
                "key": uuid.uuid4().hex, "name": "shell", "agent_name": "late", "raid": "late-crew",
                "configuration": str(d.manifest), "rows": 24, "cols": 80}}))
            stopper.peer.sendall(frame({"jsonrpc": "2.0", "id": 1, "method": "server.stop", "params": {}}))
            late = receive(launcher.peer)["result"]["session"]
            self.assertEqual(receive(stopper.peer)["result"], {"accepted": True})
            self.assertEqual(d.process.wait(timeout=30), 0)
            records = self.logged()
            failures = {r["data"]["recipient"]: r["data"]["messages"]
                        for r in records if r["kind"] == "recipient.stopped"}
            self.assertEqual((failures[alpha], failures["host"]), ([pending["id"]], [unread["reply"]["id"]]))
            # Events queued just before the stop reach the file too.
            joined = [e["data"]["session"] for e in records
                      if e["kind"] == "raid.joined" and e["data"]["raid"] == second["id"]]
            self.assertEqual(joined, [alpha])
            offline = self.cli("communications-log", "--log", str(self.log_file()), "--json")
            self.assertEqual([json.loads(line) for line in offline.stdout.splitlines()], records)
            # Offline selection by a goblin needs its session.started record.
            scoped = self.cli("communications-log", "--log", str(self.log_file()), "--session", late, "--json")
            self.assertIn("raid.created", [json.loads(line)["kind"] for line in scoped.stdout.splitlines()])
            self.assertEqual(self.log_file().stat().st_mode & 0o777, 0o600)


class OwnerWorkflow(RaidWorkflow):
    """An agent creates a raid and runs helper goblins in its project."""

    def test_an_owner_runs_helpers(self):
        d = self.d
        temp = tempfile.TemporaryDirectory(prefix="goblin-raid-work-")
        self.addCleanup(temp.cleanup)
        original = Path(temp.name) / "project"
        original.mkdir()
        (original / "input").write_text("owner\n")

        with step("a goblin outside a raid cannot run helpers"):
            result = self.cli("run", "shell", "--name", "owner", "--detached", cwd=original)
            owner = json.loads(result.stdout)["session"]
            self.running(owner)
            refused = self.goblin(owner, "goblin run shell --name early", check=False)
            self.assertEqual(refused.returncode, 2, refused.stderr)
            self.assertIn("only a raid owner can run goblins", refused.stderr)

        with step("it creates a raid and runs a helper, which works in the directory it was started in"):
            # The host replaces the project directory: the goblins keep theirs.
            moved = Path(temp.name) / "moved"
            original.rename(moved)
            original.mkdir()
            (original / "input").write_text("replacement-host-path\n")
            launch = self.json(owner, "goblin raid create crew >&2; goblin run shell --name helper")
            self.assertEqual(set(launch), {"session", "agent_name", "state"})
            helper = launch["session"]
            record = self.running(helper)
            self.assertEqual(record["created_by"], {"id": owner, "agent_name": "owner"})
            self.assertEqual(Path(record["cwd"]), original)
            self.assertEqual(self.goblin(helper, "cat input; echo helper-write > shared").stdout, "owner\n")
            self.assertEqual((moved / "shared").read_text(), "helper-write\n")
            self.assertFalse((original / "shared").exists())

        with step("status and completions show the raid and what the owner may run and stop"):
            self.assertIn("Raid: crew (role: none, owner: owner)\n", self.goblin(owner, "goblin status").stdout)
            self.assertEqual(self.goblin(owner, "goblin __complete-names -- goblin run").stdout.split(), ["shell"])
            self.assertEqual(self.goblin(owner, "goblin __complete-names -- goblin stop").stdout.split(),
                             ["helper"])

        with step("the owner sees and stops only the helpers it runs"):
            other = d.start(agent_name="other")["session"]
            self.cli("raid", "invite", "crew", "other")
            self.assertEqual([r["id"] for r in self.json(owner, "goblin list")], [helper])
            refused = self.goblin(owner, "goblin stop other", check=False)
            self.assertNotEqual(refused.returncode, 0)
            self.assertEqual(d.get(other)["state"], "running")

        with step("a helper runs nobody and cannot leave"):
            refused = self.goblin(helper, "goblin run shell --name nested", check=False)
            self.assertIn("only a raid owner can run goblins", refused.stderr)
            refused = self.goblin(helper, "goblin raid leave", check=False)
            self.assertIn("stay until stopped", refused.stderr)

        with step("a file the owner sends crosses its private /tmp by its contents"):
            with tempfile.NamedTemporaryFile(prefix="goblins-message-", dir="/tmp", delete=False) as host_file:
                host_file.write(b"host-only contents")
                path = Path(host_file.name)
            self.addCleanup(lambda: path.unlink(missing_ok=True))
            # The same pathname is a different file in the goblin.
            self.goblin(owner, f"printf 'sandbox-only contents' > {path}; goblin send helper --file {path}; rm {path}")
            self.assertEqual(path.read_text(), "host-only contents")
            message = self.next_message(helper)
            self.assertEqual((message["body"], message["sender"]), ("sandbox-only contents", owner))

        with step("an approval reaches the helpers run later, not those run before"):
            request, permission = d.pending(owner)
            self.addCleanup(request.close)
            d.decide(permission, True)
            request.peer.settimeout(120)
            self.assertEqual(receive(request.peer)["result"]["status"], "ready")
            later = self.json(owner, "goblin run shell --name later")["session"]
            self.assertEqual(self.running(later)["auto_approve"], ["hello"])
            output = self.goblin(later, "goblin request-package hello --reason copied; command -v hello").stdout
            self.assertIn("/run/goblins/packages/current/bin/hello", output)
            self.assertEqual(d.get(later)["packages"], ["hello"])
            self.assertEqual(d.permissions(session=later, state="pending"), [])
            # The helper run before still asks the host.
            request, permission = d.pending(helper)
            self.addCleanup(request.close)
            self.assertIsNone(permission["approved"])
            d.decide(permission, False)
            self.assertEqual(receive(request.peer)["result"]["status"], "denied")

        with step("roles describe members and never move ownership"):
            self.goblin(helper, "goblin raid set-role owner")
            self.cli("raid", "set-role", "crew", "later", "tester")
            members = self.goblin(owner, "goblin raid members").stdout
            self.assertRegex(members, r"\nhelper +owner +shell ")
            self.assertRegex(members, r"\nlater +tester +shell ")
            self.assertEqual(d.rpc.call("raids.members", {"raid": "crew"})["owner"], owner)

        with step("the owner stops a helper, and the record says who did"):
            self.goblin(owner, "goblin stop helper")
            self.stopped(helper)
            self.assertEqual(d.get(helper)["stop_reason"], f"stopped by raid owner owner ({owner})")

        with step("helpers outlive the owner"):
            d.rpc.call("sessions.stop", {"session": owner})
            self.stopped(owner)
            self.assertEqual(d.get(later)["state"], "running")


class CrewCarriesOnWorkflow(RaidWorkflow):
    """The owner leaves; the host picks a new one and the crew keeps working
    in the first owner's snapshot (a --workspace server)."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblin-raid-chain-")
        self.addCleanup(temp.cleanup)
        self.source = Path(temp.name)
        (self.source / "input").write_text("host\n")
        self.workspace = self.source
        super().setUp()

    def resources(self, session):
        return self.d.state / session / "resources"

    def test_the_owner_leaves_and_the_crew_carries_on(self):
        d = self.d
        a = d.start(agent_name="a")["session"]
        snapshot = self.resources(a) / "workspace"

        with step("helpers work in the owner's snapshot, not in the host's files"):
            b = d.member(a, agent_name="b")["session"]
            c = d.member(a, agent_name="c")["session"]
            self.assertEqual(self.goblin(b, "cat input; echo b-write > input").stdout, "host\n")
            self.assertEqual((snapshot / "input").read_text(), "b-write\n")
            self.assertEqual((self.source / "input").read_text(), "host\n")
            raid = d.get(b)["raid"]["name"]

        with step("a goblin the host invited can leave; its routes go"):
            y = d.start(agent_name="y")["session"]
            self.cli("raid", "invite", raid, "y")
            self.goblin(y, "goblin send c --message question")
            question = self.next_message(c)
            self.assertEqual(question["body"], "question")
            self.goblin(y, "goblin raid leave")
            self.assertEqual(len(d.rpc.call("raids.members", {"raid": raid})["members"]), 3)
            refused = self.goblin(y, "goblin send c --message again", check=False)
            self.assertNotEqual(refused.returncode, 0)
            # Its message stays with the goblin that took it.
            self.goblin(c, f"goblin inbox complete {question['id']} "
                           f"--claim-generation {question['claim_generation']}")

        with step("the owner exits: the crew pauses and keeps running"):
            peer = d.terminal(dict(session=a, terminal=d.get(a)["terminal"]))
            self.addCleanup(peer.close)
            peer.sendall(b"exit\n")
            self.stopped(a)
            self.assertIsNone(d.get(a)["stop_reason"])
            for session in (b, c):
                record = d.get(session)
                self.assertEqual(record["state"], "running")
                self.assertEqual((record["raid"]["paused"], record["raid"]["owner"]), (True, None))
            refused = self.goblin(c, "goblin send b --message hello", check=False)
            self.assertIn(f"raid '{raid}' is paused until the host selects a new owner; "
                          "you will get a notice when it resumes", refused.stderr)
            # Nobody owns a paused raid, so nobody runs or stops its members.
            refused = self.goblin(b, "goblin run shell --name early", check=False)
            self.assertNotEqual(refused.returncode, 0)
            prompts = [r for r in d.permissions() if r["kind"] == "raid-owner"]
            self.assertEqual([(r["state"], r["session"], r["package"]) for r in prompts], [("pending", a, raid)])
            self.assertEqual({m["id"] for m in prompts[0]["preview"]["members"]}, {b, c})
            self.assertIn(f"Paused: the owner {a} left", self.cli("raid", "members", raid).stdout)

        with step("gc keeps the stopped owner's snapshot while its helpers run"):
            self.assertIn("Keeping resources of the running server", self.cli("gc").stdout)
            self.assertTrue((snapshot / "input").exists())

        with step("the host picks a new owner, and the warned member gets a notice"):
            resumed = json.loads(self.cli("raid", "set-owner", raid, "b").stdout)
            self.assertEqual(resumed["owner"], b)
            notice = self.next_message(c)
            self.assertEqual((notice["sender"], notice["raid"]["event"], notice["raid"]["owner"]),
                             ("goblins", "resumed", b))
            self.assertIsNone(self.next_message(b))
            self.assertEqual([r["state"] for r in d.permissions() if r["id"] == prompts[0]["id"]], ["approved"])
            self.goblin(c, "goblin send b --message again")
            for session in ("b", "c"):
                kinds = [e["kind"] for e in self.filtered(session) if e["kind"] in ("raid.paused", "raid.owner")]
                self.assertEqual(kinds, ["raid.paused", "raid.owner"], session)
            left = {e["data"]["session"]: e["data"]["reason"] for e in self.logged() if e["kind"] == "raid.left"}
            self.assertEqual(left, {y: "left", a: "stopped"})

        with step("the new owner's helper works in the first owner's snapshot"):
            e = self.json(b, "goblin run shell --name e")["session"]
            self.running(e)
            self.assertEqual(self.goblin(e, "cat input; echo e-write > input").stdout, "b-write\n")
            self.assertEqual((snapshot / "input").read_text(), "e-write\n")
            # The new owner controls every member created in the raid.
            self.assertEqual({r["id"] for r in self.json(b, "goblin list")}, {c, e})

        with step("the new owner stops, and its helper keeps every snapshot up its chain through gc"):
            d.rpc.call("sessions.stop", {"session": b})
            self.stopped(b)
            self.assertIn("Keeping resources of the running server", self.cli("gc").stdout)
            self.assertEqual(self.goblin(e, "cat input; echo e-again > input").stdout, "e-write\n")
            self.assertEqual((snapshot / "input").read_text(), "e-again\n")
            for session in (a, b, e):
                self.assertFalse(self.lease_free(self.resources(session)), session)

        with step("dissolving with --stop-members stops the crew, and the last holder removes the snapshots"):
            destroyed = json.loads(self.cli("raid", "destroy", raid, "--stop-members").stdout)
            self.assertEqual(sorted(destroyed["stopped"]), sorted([c, e]))
            for session in (c, e):
                self.stopped(session)
                self.assertIn("closed by the host", d.get(session)["stop_reason"])
            for session in (a, b, c, e):
                d.wait(lambda: not self.resources(session).exists(), timeout=15)
            self.assertEqual((self.source / "input").read_text(), "host\n")


class CrashedChainWorkflow(RaidWorkflow):
    """A chain of owners and helpers the server dies under is left to gc."""

    def test_gc_collects_a_chain_once_every_holder_died(self):
        d = self.d
        # A runs B; A stops and B takes the raid over and runs C.
        a = d.start(agent_name="a")["session"]
        b = d.member(a, agent_name="b")["session"]
        d.rpc.call("sessions.stop", {"session": a})
        self.stopped(a)
        self.cli("raid", "set-owner", d.get(b)["raid"]["name"], "b")
        c = d.member(b, agent_name="c")["session"]
        directories = [d.state / session / "resources" for session in (a, b, c)]
        self.assertTrue(all(directory.is_dir() for directory in directories))
        os.kill(d.process.pid, signal.SIGKILL)
        d.process.wait()
        # Every sandbox, and with it every copy of a lease, goes with the
        # daemon: observe that, rather than collecting until it has.
        d.wait(lambda: all(self.lease_free(directory) for directory in directories),
               timeout=30)
        collected = self.cli("gc")
        self.assertFalse(any(directory.exists() for directory in directories), collected.stdout)


if __name__ == "__main__":
    unittest.main()
