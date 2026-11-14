"""Working on a branch in a Git worktree, as a goblin does: its folder, the goblin worktree command, helpers, a launch in a worktree, resume, and dev shells."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC
from support import command, lock_flake, pinned_nixpkgs, step
from test_devshell import TEMPLATE


class WorktreeWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.application = fixture_app("worktree-goblins")

    def setUp(self):
        # HOME holds the checkouts and the worktree directory, apart from the
        # daemon's state, which is never inside a checkout.
        self.home = tempfile.TemporaryDirectory(prefix="wt-")
        self.addCleanup(self.home.cleanup)
        self.home_path = Path(os.path.realpath(self.home.name))
        self.d = Daemon(self.application, env={**os.environ, "HOME": str(self.home_path)})
        self.addCleanup(self.d.close)
        self.worktrees = self.home_path / "worktrees"

    def git(self, directory, *args):
        return command(["git", "-C", str(directory), "-c", "user.name=t", "-c", "user.email=t@example.com", *args])

    def repository(self, *parts):
        checkout = self.home_path.joinpath("code", *(parts or ("api",)))
        checkout.mkdir(parents=True)
        self.git(checkout, "init", "-q", "-b", "main")
        (checkout / "file").write_text("main\n")
        self.git(checkout, "add", "file")
        self.git(checkout, "commit", "-qm", "first")
        return checkout

    def launch(self, cwd, name="shell", manifest=None):
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": name, "configuration": str(manifest or self.d.manifest),
            "cwd": str(cwd), "rows": 24, "cols": 100})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"] != "starting" else None),
                             timeout=60)
        self.assertEqual(record["state"], "running", record)
        return record

    def run_in(self, session, script):
        result = subprocess.run([str(self.d.app), "--state-dir", str(self.d.state), "exec", session, "--",
                                 "sh", "-c", script], env=self.d.env, text=True, capture_output=True, timeout=60)
        return result.returncode, result.stdout.strip(), result.stderr

    def sandbox(self, session, method, params):
        peer = RPC(self.d.state / session / "resources/request.sock")
        try:
            return peer.call(method, params)
        finally:
            peer.close()

    def member(self, owner, name="shell", **extra):
        if self.sandbox(owner, "sessions.status", {})["raid"] is None:
            self.sandbox(owner, "raids.create", {"raid": f"crew-{uuid.uuid4().hex[:8]}"})
        launch = self.sandbox(owner, "sessions.start", {"key": uuid.uuid4().hex, "name": name,
                                                        "detached": True, **extra})
        return self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                           timeout=60)

    def link(self, checkout, name):
        """A worktree of `checkout` in the folder, made by Git on the host."""
        path = self.worktrees / checkout.name / name
        self.git(checkout, "worktree", "add", "-q", "-b", name, str(path))
        return path

    def resume(self, session):
        """Stop a goblin and resume it from its record, as `goblins resume` does."""
        recorded = self.d.wait(lambda: self.d.record(session))
        self.d.rpc.call("sessions.stop", {"session": session})
        self.d.wait_record_released(recorded["id"])
        launch = json.loads(self.d.cli("resume", "--detached", session).stdout)
        resumed = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                              timeout=60)
        self.assertEqual(resumed["state"], "running", resumed)
        return resumed

    def test_work_on_a_branch_in_a_worktree(self):
        checkout = self.repository()
        folder = self.worktrees / "api"

        with step("a goblin launched at a checkout's root gets its worktree folder"):
            record = self.launch(checkout)
            owner = record["id"]
            self.assertTrue(folder.is_dir())
            self.assertEqual((record["worktree"], record["checkout"]), (None, str(checkout)))
            self.assertFalse(record.get("detail"))
            code, out, err = self.run_in(owner, 'echo "$GOBLINS_WORKTREES"; echo "$GOBLINS_CHECKOUT"; '
                                                'touch "$GOBLINS_WORKTREES/made"')
            self.assertEqual((code, out.split("\n")), (0, [str(folder), str(checkout)]), err)
            self.assertTrue((folder / "made").exists(), "the folder is writable and shared with the host")
            self.assertEqual(self.d.rpc.call("server.status", {})["worktrees_directory"], str(self.worktrees))

        with step("goblin worktree makes a branch the goblin works and commits in"):
            code, out, err = self.run_in(owner, "goblin worktree fix")
            self.assertEqual((code, out), (0, str(folder / "fix")), err)
            self.assertIn("fix", self.git(checkout, "branch", "--list", "fix"), "the branch is visible on the host")
            self.assertEqual(self.d.get(owner)["worktree"], {"name": "fix", "path": str(folder / "fix")})
            self.assertEqual(self.sandbox(owner, "sessions.status", {})["worktree"]["name"], "fix")
            code, out, err = self.run_in(owner, f'cd "{folder / "fix"}" && echo x > f && git add f && '
                                                'git -c user.name=t -c user.email=t@example.com commit -qm x')
            self.assertEqual(code, 0, err)
            self.assertIn(" x", self.git(folder / "fix", "log", "--oneline"))

        with step("again nothing is made; an existing branch is checked out, a new one starts at --base"):
            self.assertEqual(self.run_in(owner, "goblin worktree fix")[:2], (0, str(folder / "fix")))
            self.git(checkout, "branch", "old")
            self.assertEqual(self.run_in(owner, "goblin worktree old")[:2], (0, str(folder / "old")))
            code, out, err = self.run_in(owner, "goblin worktree two --base main")
            self.assertEqual((code, out), (0, str(folder / "two")), err)
            self.assertEqual(self.git(folder / "two", "log", "--oneline").count("\n"), 0,
                             "two starts at main, without fix's commit")
            self.assertEqual(self.d.get(owner)["worktree"]["name"], "two")
            self.git(checkout, "worktree", "remove", str(folder / "two"))

        with step("--main goes back to the main checkout, and the record follows"):
            code, out, err = self.run_in(owner, "goblin worktree --main")
            self.assertEqual((code, out), (0, str(checkout)), err)
            self.assertIsNone(self.d.get(owner)["worktree"])
            record = self.d.wait(lambda: (r if (r := self.d.record(owner)) and r.get("version") == 3 else None))
            self.assertIsNone(record["worktree"])

        with step("a helper inherits the folder, or starts in a worktree by name"):
            inherited = self.member(owner)
            self.assertEqual(inherited["state"], "running", inherited)
            self.assertEqual(self.run_in(inherited["id"], 'echo "$GOBLINS_WORKTREES"')[1], str(folder))
            member = self.member(owner, worktree="fix")
            self.assertEqual(member["state"], "running", member)
            self.assertEqual((member["cwd"], member["worktree"]),
                             (str(folder / "fix"), {"name": "fix", "path": str(folder / "fix")}))
            code, out, err = self.run_in(
                member["id"], f'pwd; echo "$GOBLINS_WORKTREES"; git -C "{folder / "fix"}" status --short --branch')
            self.assertEqual((code, out.split("\n")), (0, [str(folder / "fix"), str(folder), "## fix"]), err)
            refused = self.member(owner, name="plain", worktree="fix")
            self.assertEqual(refused["state"], "failed")
            self.assertIn("worktrees are not enabled", refused["detail"])

        with step("a goblin launched inside a worktree gets the checkout and the folder"):
            worktree = self.link(checkout, "hostfix")
            (worktree / "src").mkdir()
            record = self.launch(worktree / "src")
            self.assertFalse(record.get("detail"), record)
            self.assertEqual((record["worktree"], record["checkout"]),
                             ({"name": "hostfix", "path": str(worktree)}, str(checkout)))
            code, out, err = self.run_in(
                record["id"], f'pwd; cat "{checkout}/file"; echo "$GOBLINS_WORKTREES"; echo "$GOBLINS_CHECKOUT"; '
                              f'git -C "{worktree}" status --short --branch')
            self.assertEqual((code, out.split("\n")),
                             (0, [str(worktree / "src"), "main", str(folder), str(checkout), "## hostfix"]), err)
            code, out, err = self.run_in(record["id"], "goblin worktree --main")
            self.assertEqual((code, out), (0, str(checkout)), err)
            self.assertEqual((self.d.get(record["id"])["worktree"], self.d.get(record["id"])["checkout"]),
                             (None, str(checkout)))

        with step("a resumed goblin works in its worktree again"):
            self.assertEqual(self.run_in(owner, "goblin worktree fix")[0], 0)
            self.d.wait(lambda: (r := self.d.record(owner)) and r.get("worktree"))
            resumed = self.resume(owner)
            self.assertEqual((resumed["worktree"], resumed["skipped"]),
                             ({"name": "fix", "path": str(folder / "fix")}, []))

        with step("resumed after its worktree was removed, it works in the main checkout and says why"):
            for session in (member["id"], inherited["id"]):
                self.d.rpc.call("sessions.stop", {"session": session})
            self.d.wait(lambda: all(self.d.get(s)["state"] == "stopped" for s in (member["id"], inherited["id"])))
            self.git(checkout, "worktree", "remove", str(folder / "fix"))
            again = self.resume(resumed["id"])
            self.assertIsNone(again["worktree"])
            self.assertEqual([s["name"] for s in again["skipped"]], ["worktree"], again)
            self.assertIn("no longer exists", again["skipped"][0]["reason"])
            # The record follows what the goblin works in now.
            record = self.d.wait(lambda: (r if (r := self.d.record(again["id"])) and again["id"] in r["sessions"]
                                          else None))
            self.assertIsNone(record["worktree"])


class DevShellWorktreeWorkflow(WorktreeWorkflow):
    """A dev shell whose flake is in a worktree."""

    # The inherited workflow ran in the class above.
    test_work_on_a_branch_in_a_worktree = None

    def flake(self, directory, marker):
        (directory / "flake.nix").write_text(
            TEMPLATE.replace("NIXPKGS", pinned_nixpkgs()[0]).replace("@MARKER@", marker))

    def test_the_initial_launch_and_a_refresh_evaluate_a_worktree_flake(self):
        checkout = self.home_path / "code/api"
        checkout.mkdir(parents=True)
        self.git(checkout, "init", "-q", "-b", "main")
        self.flake(checkout, "original")
        lock_flake(checkout)
        self.git(checkout, "add", "flake.nix", "flake.lock")
        self.git(checkout, "commit", "-qm", "flake")
        worktree = self.link(checkout, "fix")
        self.assertTrue((worktree / ".git").is_file())
        result = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "shell", "configuration": str(self.d.manifest),
            "dev_shell": str(worktree), "cwd": str(worktree), "rows": 24, "cols": 80})
        record = self.d.wait(lambda: (r if (r := self.d.get(result["session"]))["state"] != "starting" else None),
                             timeout=180)
        self.assertEqual(record["state"], "running", record)
        self.assertEqual(record["dev_shell"], str(worktree))
        # The goblin's Git also works in the worktree: its common directory is mounted.
        code, out, err = self.run_in(record["id"], f'git -C "{worktree}" rev-parse --absolute-git-dir')
        self.assertEqual((code, out), (0, f"{checkout}/.git/worktrees/fix"), err)
        # A refresh evaluates the edited flake with the same visibility.
        self.flake(worktree, "edited")
        self.git(worktree, "add", "flake.nix")
        refresh = subprocess.Popen(
            [str(self.d.app), "--state-dir", str(self.d.state), "exec", record["id"], "--", "goblin", "devshell",
             "refresh", "--reason", "edited flake"], env=self.d.env, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(refresh.kill)

        def pending():
            rpc = self.d.host()
            try:
                return next((r for r in rpc.call("permissions.list", {"session": record["id"]})
                             if r["kind"] == "devshell" and r["state"] == "pending" and r.get("preview")), None)
            finally:
                rpc.close()
        approval = self.d.wait(pending, timeout=180)
        self.d.decide(approval, True)
        stdout, stderr = refresh.communicate(timeout=180)
        self.assertEqual(refresh.returncode, 0, stdout + stderr)
        self.assertIn("generation 2 is active", stdout)


if __name__ == "__main__":
    unittest.main()
