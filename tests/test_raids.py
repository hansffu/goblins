"""Raids through real daemon sockets, sandboxes and CLIs.

A raid lets members find and message each other, and its owner run and stop
the members it creates; it grants nothing else.
No coding agents, model calls, or subscription credentials are used here.
"""
import json
import subprocess
import unittest
import uuid

from daemon_support import Daemon, RPC, frame, receive
from terminal_support import Terminal


class RaidTests(unittest.TestCase):
    def setUp(self):
        self.d = Daemon()
        self.addCleanup(self.d.close)

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def refused(self, session, method, params):
        with self.assertRaises(ValueError) as error:
            self.sandbox(session, method, params)
        return error.exception.args[0]

    def host_refused(self, method, params):
        with self.assertRaises(ValueError) as error:
            self.d.rpc.call(method, params)
        return error.exception.args[0]

    def cli(self, *args):
        return subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), *args],
                              capture_output=True, text=True, timeout=20)

    def member(self, owner, name):
        return self.d.member(owner, agent_name=name)["session"]

    def send(self, sender, to, body="hello"):
        return self.sandbox(sender, "messages.send", {"key": uuid.uuid4().hex, "to": to, "body": body})

    def log_file(self):
        return next(self.d.state.glob("communications-*.jsonl"))

    def logged(self):
        return [json.loads(line) for line in self.log_file().read_text().splitlines()]

    def filtered(self, session):
        result = self.cli("communications-log", "--session", session, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        return [json.loads(line) for line in result.stdout.splitlines()]

    def test_host_invited_roots_find_and_message_each_other(self):
        alpha = self.d.start(agent_name="alpha")["session"]
        beta = self.d.start(agent_name="beta")["session"]
        invited = self.cli("raid", "invite", "crew", "alpha", "beta")
        self.assertEqual(invited.returncode, 0, invited.stderr)
        record = json.loads(invited.stdout)
        self.assertEqual(record["owner"], alpha)
        self.assertEqual([m["agent_name"] for m in record["members"]], ["alpha", "beta"])
        members = self.cli("raid", "members", "crew")
        self.assertEqual(members.returncode, 0, members.stderr)
        lines = members.stdout.splitlines()
        self.assertEqual(lines[1].split()[:3], ["NAME", "ROLE", "CONFIGURATION"])
        self.assertEqual(lines[2].split()[:3], ["alpha", "-", "shell"])
        self.assertIn("owner", lines[2].split())
        listed = {r["id"]: r for r in json.loads(self.cli("list").stdout)}
        self.assertEqual(listed[beta]["raid"]["name"], "crew")
        self.assertEqual(listed[beta]["raid"]["owner"], alpha)
        # Both can now address each other, by name or ID.
        sent = self.send(alpha, "beta")
        self.assertEqual(sent["recipient"], beta)
        self.assertEqual(self.send(beta, alpha)["recipient"], alpha)
        claim = self.sandbox(beta, "inbox.next", {"key": "fetch"})
        self.assertEqual(claim["message"]["id"], sent["id"])
        self.sandbox(beta, "messages.reply", {"key": "reply", "message": sent["id"],
                                              "claim_generation": claim["claim_generation"], "body": "ok"})
        # Membership grants no control over, or view into, another member.
        for method, params in [("sessions.get", {"session": beta}),
                               ("sessions.stop", {"session": beta})]:
            self.assertEqual(self.refused(alpha, method, params)["code"], -32004)
        private = json.loads(self.cli("send", "beta", "--message", "for beta only").stdout)
        self.assertEqual(self.refused(alpha, "messages.get", {"message": private["id"]})["code"], -32004)
        # Each member reads only its own inbox.
        own = self.sandbox(alpha, "inbox.next", {"key": "alpha-fetch"})["message"]
        self.assertEqual((own["sender"], own["recipient"]), (beta, alpha))
        # The in-sandbox CLI: status, members and send by name.
        terminal = Terminal([str(self.d.app), "--state-dir", str(self.d.state), "attach", "alpha"])
        self.addCleanup(terminal.close)
        terminal.send("goblin status; goblin raid members; goblin send beta --message from-cli --key cli-send; "
                      "printf 'SEND=%s\\n' $status\n")
        output = terminal.expect(r"(?s)(.*?)SEND=(\d+)\n")
        text = output.group(1).decode(errors="replace")
        self.assertEqual(output.group(2), b"0", text)
        self.assertIn("Raid: crew (role: none, owner: alpha)", text)
        self.assertRegex(text, r"beta\s+-\s+shell\s+running")
        bodies = []
        for n in range(2):
            item = self.sandbox(beta, "inbox.next", {"key": f"fetch-{n}"})["message"]
            bodies.append(item["body"])
            self.sandbox(beta, "inbox.complete", {"key": f"complete-{n}", "message": item["id"],
                                                  "claim_generation": item["claim_generation"]})
        self.assertEqual(bodies, ["for beta only", "from-cli"])
        events = self.logged()
        routes = {(e["actor"], e["data"]["change"]["route"], e["data"]["change"]["raid"])
                  for e in events if e["kind"] == "message.accepted"}
        self.assertEqual(routes, {(alpha, "raid", record["id"]), (beta, "raid", record["id"]),
                                  ("host", "host", None)})
        # An invited member's filtered log shows the host invitation.
        joined = [e for e in self.filtered("beta") if e["kind"] == "raid.joined"]
        self.assertEqual([(e["actor"], e["sessions"], e["data"]["session"]) for e in joined],
                         [("host", [beta], beta)])

    def test_the_host_kicks_members_and_kicking_the_owner_pauses_the_raid(self):
        alpha = self.d.start(agent_name="alpha")["session"]
        beta = self.d.start(agent_name="beta")["session"]
        self.cli("raid", "invite", "crew", "alpha", "beta")
        self.assertEqual(self.host_refused("raids.kick", {"raid": "crew", "session": "nobody"})["code"], -32004)
        self.assertEqual(self.host_refused("raids.kick", {"raid": "absent", "session": beta})["code"], -32004)
        kicked = self.d.rpc.call("raids.kick", {"raid": "crew", "session": beta})
        self.assertEqual(kicked, {"accepted": True, "dissolved": False, "paused": False})
        listed = {r["id"]: r for r in json.loads(self.cli("list").stdout)}
        self.assertIsNone(listed[beta]["raid"])
        self.assertEqual(listed[alpha]["raid"]["name"], "crew")
        self.assertEqual(self.host_refused("raids.kick", {"raid": "crew", "session": beta})["code"], -32009)
        # Kicking the only member dissolves the raid.
        kicked = self.d.rpc.call("raids.kick", {"raid": "crew", "session": alpha})
        self.assertEqual(kicked, {"accepted": True, "dissolved": True, "paused": False})
        self.assertEqual(self.host_refused("raids.members", {"raid": "crew"})["code"], -32004)
        # Kicking an owner with members left pauses the raid instead.
        self.cli("raid", "invite", "crew", "alpha", "beta")
        kicked = self.d.rpc.call("raids.kick", {"raid": "crew", "session": alpha})
        self.assertEqual(kicked, {"accepted": True, "dissolved": False, "paused": True})
        record = self.d.rpc.call("raids.members", {"raid": "crew"})
        self.assertEqual((record["paused"], record["owner"], record["former_owner"]), (True, None, alpha))

    def test_owners_run_members_and_roles_never_take_ownership(self):
        root = self.d.start(agent_name="root")["session"]
        other = self.d.start(agent_name="other")["session"]
        created = self.sandbox(root, "raids.create", {"raid": "crew"})
        self.assertEqual(created["owner"], root)
        self.assertEqual(self.refused(root, "raids.create", {"raid": "second"})["code"], -32009)
        self.assertEqual(self.refused(other, "raids.create", {"raid": "crew"})["message"],
                         "raid 'crew' already exists")
        kid = self.member(root, "kid")
        helper = self.member(root, "helper")
        record = self.sandbox(helper, "raids.members", {})
        self.assertEqual(record["owner"], root)
        self.assertEqual({m["agent_name"]: m["created_by"] for m in record["members"]},
                         {"root": None, "kid": root, "helper": root})
        # Sandboxes invite nobody; only the owner runs members.
        self.assertEqual(self.refused(root, "raids.invite", {"sessions": [other]})["code"], -32601)
        refused = self.refused(kid, "sessions.start", {"key": "k", "name": "shell"})
        self.assertEqual(refused["message"], "only a raid owner can run goblins; create one with goblin raid create NAME")
        # A goblin in another raid is never moved.
        self.sandbox(other, "raids.create", {"raid": "elsewhere"})
        moved = self.cli("raid", "invite", "crew", "other")
        self.assertNotEqual(moved.returncode, 0)
        self.assertIn("goblin 'other' is already in raid 'elsewhere'", moved.stderr)
        # A role is a description: it never moves ownership and cannot be cleared.
        record = self.sandbox(kid, "raids.set_role", {"role": "owner"})
        self.assertEqual(record["owner"], root)
        self.assertEqual(self.sandbox(kid, "sessions.status", {})["raid"]["role"], "owner")
        for role in ["", None]:
            self.assertEqual(self.refused(kid, "raids.set_role", {"role": role})["code"], -32602)
        record = json.loads(self.cli("raid", "set-role", "crew", "helper", "reviewer").stdout)
        self.assertEqual({m["agent_name"]: m["role"] for m in record["members"]}["helper"], "reviewer")
        # Members message each other directly.
        self.assertEqual(self.send(kid, "helper")["recipient"], helper)

    def test_leaving_and_owner_exit_remove_raid_routes_but_not_messages(self):
        owner = self.d.start(agent_name="owner")
        x = self.d.start(agent_name="x")["session"]
        y = self.d.start(agent_name="y")["session"]
        z = self.d.start(agent_name="z")["session"]
        self.d.rpc.call("raids.invite", {"raid": "crew", "sessions": ["owner", "x", "y", "z"]})
        question = self.send(y, "x", "question")
        claim = self.sandbox(x, "inbox.next", {"key": "fetch"})
        self.assertEqual(claim["message"]["id"], question["id"])
        # A non-owner leaving leaves the raid in place.
        self.assertEqual(self.sandbox(y, "raids.leave", {}), {"accepted": True, "dissolved": False, "paused": False})
        self.assertEqual(len(self.d.rpc.call("raids.members", {"raid": "crew"})["members"]), 3)
        self.assertEqual(self.refused(y, "messages.send", {"key": "k", "to": "x", "body": "x"})["code"], -32004)
        # A reply is authorized like a send; the claim stays outstanding.
        rejected = self.refused(x, "messages.reply", {"key": "reply", "message": question["id"],
                                                     "claim_generation": claim["claim_generation"], "body": "late"})
        self.assertEqual(rejected, {"code": -32009, "message": "not allowed: 'y' is no longer in this goblin's raid"})
        status = self.sandbox(x, "inbox.status", {})
        self.assertEqual(status["claim"], question["id"])
        self.assertEqual(self.sandbox(x, "messages.get", {"message": question["id"]})["body"], "question")
        self.sandbox(x, "inbox.complete", {"key": "complete", "message": question["id"],
                                           "claim_generation": claim["claim_generation"]})
        self.assertEqual(self.send(x, "z")["recipient"], z)
        # The owner exiting pauses the raid and stops nobody else.
        launch = dict(owner, terminal=self.d.get(owner["session"])["terminal"])
        peer = self.d.terminal(launch)
        self.addCleanup(peer.close)
        peer.sendall(b"exit\n")
        self.d.wait(lambda: self.d.get(owner["session"])["state"] == "stopped")
        record = self.d.rpc.call("raids.members", {"raid": "crew"})
        self.assertEqual((record["paused"], record["owner"]), (True, None))
        for session in (x, z):
            record = self.d.get(session)
            self.assertEqual(record["state"], "running")
            self.assertEqual((record["raid"]["paused"], record["raid"]["owner"]), (True, None))
        # Members are refused with a warning, and the host is asked to resolve it.
        warning = self.refused(x, "messages.send", {"key": "k2", "to": "z", "body": "x"})
        self.assertEqual(warning, {"code": -32009, "message": "raid 'crew' is paused until the host selects a new "
                                   "owner; you will get a notice when it resumes"})
        prompts = [r for r in self.d.rpc.call("permissions.list", {}) if r["kind"] == "raid-owner"]
        self.assertEqual([(r["state"], r["session"], r["package"]) for r in prompts],
                         [("pending", owner["session"], "crew")])
        self.assertEqual({m["id"] for m in prompts[0]["preview"]["members"]}, {x, z})
        members = self.cli("raid", "members", "crew")
        self.assertIn(f"Paused: the owner {owner['session']} left", members.stdout)
        # The host selects z; x, which was warned, gets a notice, z does not.
        resumed = self.cli("raid", "set-owner", "crew", "z")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(json.loads(resumed.stdout)["owner"], z)
        notice = self.d.wait(lambda: self.sandbox(x, "inbox.next", {"key": uuid.uuid4().hex})["message"])
        self.assertEqual((notice["sender"], notice["raid"]["event"], notice["raid"]["owner"]),
                         ("goblins", "resumed", z))
        self.assertIsNone(self.sandbox(z, "inbox.next", {"key": "z-empty"})["message"])
        prompt = [r for r in self.d.rpc.call("permissions.list", {}) if r["id"] == prompts[0]["id"]][0]
        self.assertEqual(prompt["state"], "approved")
        self.assertEqual(self.send(x, "z", "again")["recipient"], z)
        # Each member's filtered log shows the pause and the new owner.
        for session in ("x", "z"):
            kinds = [e["kind"] for e in self.filtered(session) if e["kind"] in ("raid.paused", "raid.owner")]
            self.assertEqual(kinds, ["raid.paused", "raid.owner"], session)
        left = {e["data"]["session"]: e["data"]["reason"] for e in self.logged() if e["kind"] == "raid.left"}
        self.assertEqual(left, {y: "left", owner["session"]: "stopped"})
        # Dissolving with --stop-members stops every running member.
        destroyed = self.cli("raid", "destroy", "crew", "--stop-members")
        self.assertEqual(destroyed.returncode, 0, destroyed.stderr)
        self.assertEqual(sorted(json.loads(destroyed.stdout)["stopped"]), sorted([x, z]))
        for session in (x, z):
            self.d.wait(lambda: self.d.get(session)["state"] == "stopped")
            self.assertIn("closed by the host", self.d.get(session)["stop_reason"])

    def test_destroy_reuse_and_flushing_without_other_traffic(self):
        a = self.d.start(agent_name="a")["session"]
        b = self.d.start(agent_name="b")["session"]
        first = self.d.rpc.call("raids.invite", {"raid": "crew", "sessions": [a, b]})
        self.assertEqual(self.d.rpc.call("raids.destroy", {"raid": "crew"}), {"accepted": True, "stopped": []})
        # No inbox, send or log call: those would flush the outbox themselves.
        def raid_events():
            return [(e["kind"], e["data"].get("raid")) for e in self.logged() if e["kind"].startswith("raid.")]
        events = self.d.wait(lambda: (r if len(r := raid_events()) == 3 else None))
        self.assertEqual(events, [("raid.created", first["id"]), ("raid.joined", first["id"]),
                                  ("raid.dissolved", first["id"])])
        dissolved = [e for e in self.logged() if e["kind"] == "raid.dissolved"][0]
        self.assertEqual((dissolved["actor"], dissolved["data"]["reason"], sorted(dissolved["sessions"])),
                         ("host", "destroyed", sorted([a, b])))
        for session in (a, b):
            self.assertEqual(self.d.get(session)["state"], "running")
            self.assertIsNone(self.d.get(session)["raid"])
        # A reused name is a new raid without its former members.
        second = self.d.rpc.call("raids.invite", {"raid": "crew", "sessions": [b]})
        self.assertNotEqual(second["id"], first["id"])
        self.assertEqual([m["id"] for m in second["members"]], [b])
        self.assertEqual(second["owner"], b)
        # Events queued just before an orderly stop reach the file too.
        self.d.rpc.call("raids.invite", {"raid": "crew", "sessions": [a]})
        stopped = self.cli("server", "stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertEqual(self.d.process.wait(timeout=10), 0)
        joined = [e["data"]["session"] for e in self.logged()
                  if e["kind"] == "raid.joined" and e["data"]["raid"] == second["id"]]
        self.assertEqual(joined, [a])

    def test_owner_launched_just_before_stop_is_in_the_offline_log(self):
        # A --raid launch and server.stop on separate connections, sent
        # together so they are dispatched in the same tick.
        launcher, stopper = RPC(self.d.state / "host.sock"), RPC(self.d.state / "host.sock")
        self.addCleanup(launcher.close)
        self.addCleanup(stopper.close)
        launch = frame({"jsonrpc": "2.0", "id": 1, "method": "sessions.start", "params": {
            "key": uuid.uuid4().hex, "name": "shell", "agent_name": "late", "raid": "crew",
            "configuration": str(self.d.manifest), "rows": 24, "cols": 80}})
        stop = frame({"jsonrpc": "2.0", "id": 1, "method": "server.stop", "params": {}})
        launcher.peer.sendall(launch)
        stopper.peer.sendall(stop)
        owner = receive(launcher.peer)["result"]["session"]
        self.assertEqual(receive(stopper.peer)["result"], {"accepted": True})
        self.assertEqual(self.d.process.wait(timeout=30), 0)
        kinds = [e["kind"] for e in self.logged() if owner in (e["actor"], *e["sessions"])]
        self.assertIn("raid.created", kinds)
        # Offline selection needs the owner's session.started record.
        result = self.cli("communications-log", "--log", str(self.log_file()), "--session", owner, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("raid.created", [json.loads(line)["kind"] for line in result.stdout.splitlines()])


if __name__ == "__main__":
    unittest.main()
