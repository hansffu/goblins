"""Raids through real daemon sockets, sandboxes and CLIs.

A raid only lets members find and message each other; it grants nothing else.
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

    def child(self, parent, name, **extra):
        launch = self.sandbox(parent, "sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "agent_name": name, "detached": True, **extra,
        })
        record = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                             timeout=60)
        self.assertEqual(record["state"], "running", record)
        return launch["session"]

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
        self.assertEqual([m["path"] for m in record["members"]], ["alpha", "beta"])
        members = self.cli("raid", "members", "crew")
        self.assertEqual(members.returncode, 0, members.stderr)
        lines = members.stdout.splitlines()
        self.assertEqual(lines[1].split()[:3], ["NAME", "ROLE", "PATH"])
        self.assertEqual(lines[2].split()[:3], ["alpha", "-", "alpha"])
        self.assertIn("owner", lines[2].split())
        listed = {r["id"]: r for r in json.loads(self.cli("list").stdout)}
        self.assertEqual(listed[beta]["raid"]["name"], "crew")
        self.assertEqual(listed[beta]["raid"]["owner"], alpha)
        # Both branches can now address each other, by name, path or ID.
        sent = self.send(alpha, "beta")
        self.assertEqual(sent["recipient"], beta)
        self.assertEqual(self.send(beta, alpha)["recipient"], alpha)
        claim = self.sandbox(beta, "inbox.next", {"key": "fetch"})
        self.assertEqual(claim["message"]["id"], sent["id"])
        self.sandbox(beta, "messages.reply", {"key": "reply", "message": sent["id"],
                                              "claim_generation": claim["claim_generation"], "body": "ok"})
        # Membership grants no control over, or view into, the other branch.
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
        terminal.send("goblins status; goblins raid members; goblins send beta --message from-cli --key cli-send; "
                      "printf 'SEND=%s\\n' $status\n")
        output = terminal.expect(r"(?s)(.*?)SEND=(\d+)\n")
        text = output.group(1).decode(errors="replace")
        self.assertEqual(output.group(2), b"0", text)
        self.assertIn("Raid: crew (role: none, owner: alpha)", text)
        self.assertRegex(text, r"beta\s+-\s+beta\s+shell\s+running")
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
                                  ("host", "tree", None)})
        # An invited member's filtered log shows the host invitation.
        joined = [e for e in self.filtered("beta") if e["kind"] == "raid.joined"]
        self.assertEqual([(e["actor"], e["sessions"], e["data"]["session"]) for e in joined],
                         [("host", [beta], beta)])

    def test_members_recruit_direct_children_only_and_never_take_ownership(self):
        root = self.d.start(agent_name="root")["session"]
        other = self.d.start(agent_name="other")["session"]
        created = self.sandbox(root, "raids.create", {"raid": "crew"})
        self.assertEqual(created["owner"], root)
        self.assertEqual(self.refused(root, "raids.create", {"raid": "second"})["code"], -32009)
        self.assertEqual(self.refused(other, "raids.create", {"raid": "crew"})["message"],
                         "raid 'crew' already exists")
        kid = self.child(root, "kid")
        sibling = self.child(root, "sib")
        grandkid = self.child(kid, "grand")
        self.assertEqual(len(self.sandbox(root, "raids.invite", {"sessions": ["kid"]})["members"]), 2)
        # Grandchildren and siblings cannot be recruited.
        self.assertEqual(self.refused(root, "raids.invite", {"sessions": ["kid/grand"]})["message"],
                         "only direct children can be invited")
        self.assertEqual(self.refused(kid, "raids.invite", {"sessions": [sibling]})["code"], -32004)
        # Any member may invite its own child, or launch one into the raid.
        self.sandbox(kid, "raids.invite", {"sessions": ["grand"]})
        helper = self.child(root, "helper", inherit_raid=True)
        record = self.sandbox(helper, "raids.members", {})
        self.assertEqual(record["owner"], root)
        self.assertEqual({m["path"] for m in record["members"]},
                         {"root", "root/kid", "root/kid/grand", "root/helper"})
        # --inherit-raid needs a parent in a raid.
        refused = self.refused(sibling, "sessions.start", {"key": "k", "name": "shell", "inherit_raid": True})
        self.assertEqual(refused["message"], "parent is not in a raid")
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
        record = json.loads(self.cli("raid", "set-role", "crew", "root/kid/grand", "reviewer").stdout)
        self.assertEqual({m["path"]: m["role"] for m in record["members"]}["root/kid/grand"], "reviewer")
        # Raid siblings message each other directly.
        self.assertEqual(self.send(grandkid, "helper")["recipient"], helper)

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
        self.assertEqual(self.sandbox(y, "raids.leave", {}), {"accepted": True, "dissolved": False})
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
        # The owner exiting dissolves the raid and stops nobody else.
        launch = dict(owner, terminal=self.d.get(owner["session"])["terminal"])
        peer = self.d.terminal(launch)
        self.addCleanup(peer.close)
        peer.sendall(b"exit\n")
        self.d.wait(lambda: self.d.get(owner["session"])["state"] == "stopped")
        self.assertEqual(self.host_refused("raids.members", {"raid": "crew"})["code"], -32004)
        for session in (x, z):
            record = self.d.get(session)
            self.assertEqual(record["state"], "running")
            self.assertIsNone(record["raid"])
        self.assertEqual(self.refused(x, "messages.send", {"key": "k2", "to": "z", "body": "x"})["code"], -32004)
        self.assertEqual(self.refused(x, "raids.members", {})["code"], -32009)
        # Each former member's filtered log shows the dissolution.
        for session in ("x", "z", owner["session"]):
            dissolved = [e for e in self.filtered(session) if e["kind"] == "raid.dissolved"]
            self.assertEqual([e["data"]["reason"] for e in dissolved], ["owner-left"], session)
        left = {e["data"]["session"]: e["data"]["reason"] for e in self.logged() if e["kind"] == "raid.left"}
        self.assertEqual(left, {y: "left", owner["session"]: "stopped"})

    def test_destroy_reuse_and_flushing_without_other_traffic(self):
        a = self.d.start(agent_name="a")["session"]
        b = self.d.start(agent_name="b")["session"]
        first = self.d.rpc.call("raids.invite", {"raid": "crew", "sessions": [a, b]})
        self.assertEqual(self.d.rpc.call("raids.destroy", {"raid": "crew"}), {"accepted": True})
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
