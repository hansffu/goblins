"""Git worktree support through real daemons and sandboxes: folders, mounts, the goblin worktree command, members, dev shells and resume."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon, RPC
from support import FLAKE, command
from test_devshell import TEMPLATE


class WorktreeTests(unittest.TestCase):
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

    def manifest(self, name, **worktrees):
        """The fixture manifest with other worktree settings."""
        config = json.loads(Path(self.d.manifest).read_text())
        config["worktrees"] = worktrees
        path = Path(self.d.temp.name) / f"{name}.json"
        path.write_text(json.dumps(config))
        return path

    def link(self, checkout, name):
        """A worktree of `checkout` in the folder, made by Git on the host."""
        path = self.worktrees / checkout.name / name
        self.git(checkout, "worktree", "add", "-q", "-b", name, str(path))
        return path

    def test_a_launch_at_a_checkouts_root_gets_its_folder(self):
        checkout = self.repository()
        record = self.launch(checkout)
        folder = self.worktrees / "api"
        self.assertTrue(folder.is_dir())
        self.assertIsNone(record["worktree"])
        self.assertEqual(record["checkout"], str(checkout))
        self.assertFalse(record.get("detail"))
        code, out, err = self.run_in(record["id"], 'echo "$GOBLINS_WORKTREES"; echo "$GOBLINS_CHECKOUT"; touch "$GOBLINS_WORKTREES/made"')
        self.assertEqual((code, out.split("\n")), (0, [str(folder), str(checkout)]), err)
        self.assertTrue((folder / "made").exists(), "the folder is writable and shared with the host")
        self.assertEqual(self.d.rpc.call("server.status", {})["worktrees_directory"], str(self.worktrees))

    def test_nothing_changes_below_the_root_or_without_worktrees(self):
        checkout = self.repository()
        (checkout / "src").mkdir()
        off = self.manifest("off")
        for cwd, name, manifest in ((checkout / "src", "shell", None), (checkout, "plain", None), (checkout, "shell", off)):
            record = self.launch(cwd, name, manifest)
            code, out, err = self.run_in(record["id"], 'echo "[${GOBLINS_WORKTREES-unset}${GOBLINS_CHECKOUT-unset}]"')
            self.assertEqual((code, out), (0, "[unsetunset]"), err)
            self.assertIsNone(record["checkout"])
            self.assertFalse(record.get("detail"))
        self.assertFalse(self.worktrees.exists())
        code, out, err = self.run_in(record["id"], "goblin worktree fix")
        self.assertNotEqual(code, 0)
        self.assertIn("no worktree folder", err)

    def test_a_launch_in_a_worktree_gets_the_checkout_and_the_folder(self):
        checkout = self.repository()
        worktree = self.link(checkout, "fix")
        (worktree / "src").mkdir()
        for cwd in (worktree, worktree / "src"):
            record = self.launch(cwd)
            self.assertFalse(record.get("detail"), record)
            self.assertEqual(record["worktree"], {"name": "fix", "path": str(worktree)})
            self.assertEqual(record["checkout"], str(checkout))
            code, out, err = self.run_in(
                record["id"], f'pwd; cat "{checkout}/file"; echo "$GOBLINS_WORKTREES"; echo "$GOBLINS_CHECKOUT"; '
                              f'git -C "{worktree}" status --short --branch')
            self.assertEqual((code, out.split("\n")),
                             (0, [str(cwd), "main", str(self.worktrees / "api"), str(checkout), "## fix"]), err)
            # Back to the main checkout, which the command prints, from a launch in a worktree.
            code, out, err = self.run_in(record["id"], "goblin worktree --main")
            self.assertEqual((code, out), (0, str(checkout)), err)
            self.assertIsNone(self.d.get(record["id"])["worktree"])
            self.assertEqual(self.d.get(record["id"])["checkout"], str(checkout))

    def test_a_forged_git_file_is_refused(self):
        checkout = self.repository()
        worktree = self.link(checkout, "fix")
        secret = self.repository("secret")
        # Pointing the worktree at another repository fails the back-pointer check.
        (secret / ".git/worktrees/x").mkdir(parents=True)
        (secret / ".git/worktrees/x/gitdir").write_text("/nowhere/.git\n")
        (worktree / ".git").write_text(f"gitdir: {secret}/.git/worktrees/x\n")
        record = self.launch(worktree)
        self.assertIn("does not point back", record["detail"])
        self.assertIsNone(record["worktree"])
        code, out, _ = self.run_in(record["id"], f'echo "[${{GOBLINS_WORKTREES-unset}}]"; test -e "{secret}/file"; echo $?')
        self.assertEqual(out.split("\n"), ["[unset]", "1"], "the other repository is not mounted")

    def test_overlapping_layouts_and_protected_paths_are_refused(self):
        checkout = self.repository()
        # A directory inside the checkout, and a folder that is the checkout itself.
        for name, directory in (("inside", checkout / "trees"), ("same", checkout.parent)):
            record = self.launch(checkout, manifest=self.manifest(name, directory=str(directory)))
            self.assertIn("must not contain each other", record["detail"], name)
            code, out, _ = self.run_in(record["id"], 'echo "[${GOBLINS_WORKTREES-unset}]"')
            self.assertEqual(out, "[unset]")
        # The daemon's own state is protected like any mount.
        record = self.launch(checkout, manifest=self.manifest("state", directory=str(self.d.state / "trees")))
        self.assertTrue(record["detail"], record)
        code, out, _ = self.run_in(record["id"], 'echo "[${GOBLINS_WORKTREES-unset}]"')
        self.assertEqual(out, "[unset]")

    def test_the_command_creates_enters_and_leaves_a_worktree(self):
        checkout = self.repository()
        owner = self.launch(checkout)["id"]
        folder = self.worktrees / "api"
        code, out, err = self.run_in(owner, "goblin worktree fix")
        self.assertEqual((code, out), (0, str(folder / "fix")), err)
        self.assertIn("fix", self.git(checkout, "branch", "--list", "fix"), "the branch is visible on the host")
        self.assertEqual(self.d.get(owner)["worktree"], {"name": "fix", "path": str(folder / "fix")})
        self.assertEqual(self.sandbox(owner, "sessions.status", {})["worktree"]["name"], "fix")
        # A worktree the goblin can commit in, from its sandbox.
        code, out, err = self.run_in(owner, f'cd "{folder / "fix"}" && echo x > f && git add f && '
                                            'git -c user.name=t -c user.email=t@example.com commit -qm x && git log --oneline')
        self.assertEqual(code, 0, err)
        self.assertIn(" x", self.git(folder / "fix", "log", "--oneline"))
        # Again: nothing is created. An existing branch is checked out; a new one starts at --base.
        self.assertEqual(self.run_in(owner, "goblin worktree fix")[:2], (0, str(folder / "fix")))
        self.git(checkout, "branch", "old")
        self.assertEqual(self.run_in(owner, "goblin worktree old")[:2], (0, str(folder / "old")))
        code, out, err = self.run_in(owner, "goblin worktree two --base main")
        self.assertEqual((code, out), (0, str(folder / "two")), err)
        self.assertEqual(self.git(folder / "two", "log", "--oneline").count("\n"), 0, "two starts at main, without fix's commit")
        self.assertEqual(self.d.get(owner)["worktree"]["name"], "two")
        # --main clears it and prints the main checkout, and the record follows.
        code, out, err = self.run_in(owner, "goblin worktree --main")
        self.assertEqual((code, out), (0, str(checkout)), err)
        self.assertIsNone(self.d.get(owner)["worktree"])
        record = self.d.wait(lambda: (r if (r := self.d.record(owner)) and r.get("version") == 3 else None))
        self.assertIsNone(record["worktree"])
        # Bad names and a base for an existing branch are refused; Git does the removal.
        for bad in ("a/b", "../x", ".git", "-x"):
            self.assertNotEqual(self.run_in(owner, f"goblin worktree -- {bad}")[0], 0, bad)
        self.assertEqual(self.run_in(owner, "goblin worktree fix --base main")[0], 0, "an existing worktree needs no base")
        self.git(checkout, "branch", "third")
        code, _, err = self.run_in(owner, "goblin worktree third --base main")
        self.assertNotEqual(code, 0)
        self.assertIn("--base only applies to a new branch", err)
        self.git(checkout, "worktree", "remove", str(folder / "two"))
        self.assertFalse((folder / "two").exists())

    def test_the_daemon_checks_the_name_against_the_callers_folder(self):
        checkout = self.repository()
        owner = self.launch(checkout)["id"]
        plain = self.launch(self.repository("plain-repo"), "plain")["id"]
        folder = self.worktrees / "api"
        self.link(checkout, "fix")
        (folder / "dir").mkdir()
        (folder / "link").symlink_to(folder / "fix")
        for bad in ("dir", "link", "missing"):
            with self.assertRaises(ValueError, msg=bad):
                self.sandbox(owner, "sessions.set-worktree", {"name": bad})
        with self.assertRaises(ValueError):
            self.sandbox(owner, "sessions.set-worktree", {"name": "../api"})
        self.assertEqual(self.sandbox(owner, "sessions.set-worktree", {"name": "fix"})["worktree"]["name"], "fix")
        self.assertIsNone(self.sandbox(owner, "sessions.set-worktree", {"name": None})["worktree"])
        with self.assertRaises(ValueError):
            self.sandbox(plain, "sessions.set-worktree", {"name": "fix"})

    def test_a_member_starts_in_a_worktree_of_the_owners_folder(self):
        checkout = self.repository()
        owner = self.launch(checkout)["id"]
        folder = self.worktrees / "api"
        # Without --worktree a member inherits the owner's directory and gets the folder too.
        inherited = self.member(owner)
        self.assertEqual(inherited["state"], "running", inherited)
        self.assertEqual(self.run_in(inherited["id"], 'echo "$GOBLINS_WORKTREES"')[1], str(folder))
        self.link(checkout, "fix")
        member = self.member(owner, worktree="fix")
        self.assertEqual(member["state"], "running", member)
        self.assertEqual(member["cwd"], str(folder / "fix"))
        self.assertEqual(member["worktree"], {"name": "fix", "path": str(folder / "fix")})
        code, out, err = self.run_in(
            member["id"], f'pwd; echo "$GOBLINS_WORKTREES"; git -C "{folder / "fix"}" status --short --branch')
        self.assertEqual((code, out.split("\n")), (0, [str(folder / "fix"), str(folder), "## fix"]), err)
        # Refused: escapes, a name that is not a worktree, and a configuration without worktrees.
        for bad in ("../api", "a/b", ".git"):
            with self.assertRaises(ValueError, msg=bad):
                self.member(owner, worktree=bad)
        (folder / "plain-dir").mkdir()
        for name in ("missing", "plain-dir"):
            failed = self.member(owner, worktree=name)
            self.assertEqual(failed["state"], "failed", name)
            self.assertIn("is not a worktree", failed["detail"])
        # A symlink in the folder to a real worktree of another repository is not one of this folder.
        elsewhere = self.link(self.repository("other"), "fix")
        (folder / "swap").symlink_to(elsewhere)
        swapped = self.member(owner, worktree="swap")
        self.assertEqual(swapped["state"], "failed", swapped)
        self.assertIn("is not a worktree", swapped["detail"])
        refused = self.member(owner, name="plain", worktree="fix")
        self.assertEqual(refused["state"], "failed")
        self.assertIn("worktrees are not enabled", refused["detail"])
        # An owner without a folder cannot ask for one.
        bare = self.launch(self.repository("bare"), "plain")["id"]
        with self.assertRaises(ValueError):
            self.member(bare, worktree="fix")

    def resume(self, change=None):
        checkout = self.repository()
        first = self.launch(checkout)["id"]
        folder = self.worktrees / "api"
        self.assertEqual(self.run_in(first, "goblin worktree fix")[0], 0)
        recorded = self.d.wait(lambda: (r if (r := self.d.record(first)) and r.get("worktree") else None))
        self.assertEqual(recorded["worktree"], {"name": "fix", "path": str(folder / "fix")})
        self.assertEqual(recorded["version"], 3)
        self.d.rpc.call("sessions.stop", {"session": first})
        self.d.wait_record_released(recorded["id"])
        if change == "removed":
            self.git(checkout, "worktree", "remove", str(folder / "fix"))
        launch = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "configuration": str(self.d.manifest), "recreate": recorded["id"],
            "rows": 24, "cols": 100, "detached": True})
        resumed = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                              timeout=60)
        self.assertEqual(resumed["state"], "running", resumed)
        return resumed, folder

    def test_resume_restores_a_worktree_that_still_exists(self):
        resumed, folder = self.resume()
        self.assertEqual(resumed["worktree"], {"name": "fix", "path": str(folder / "fix")})
        self.assertEqual(resumed["skipped"], [])

    def test_resume_keeps_a_worktree_launch_in_the_main_checkout(self):
        checkout = self.repository()
        worktree = self.link(checkout, "fix")
        first = self.launch(worktree)["id"]
        self.assertEqual(self.run_in(first, "goblin worktree --main")[0], 0)
        recorded = self.d.wait(lambda: (r if (r := self.d.record(first)) and r.get("version") == 3
                                        and r.get("worktree") is None else None))
        self.d.rpc.call("sessions.stop", {"session": first})
        self.d.wait_record_released(recorded["id"])
        launch = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "configuration": str(self.d.manifest), "recreate": recorded["id"],
            "rows": 24, "cols": 100, "detached": True})
        resumed = self.d.wait(lambda: (r if (r := self.d.get(launch["session"]))["state"] != "starting" else None),
                              timeout=60)
        self.assertEqual(resumed["state"], "running", resumed)
        # Relaunched in the worktree, it still works in the main checkout.
        self.assertEqual(resumed["cwd"], str(worktree))
        self.assertIsNone(resumed["worktree"])
        self.assertEqual(resumed["checkout"], str(checkout))
        self.assertEqual(resumed["skipped"], [])

    def test_resume_skips_a_worktree_that_was_removed(self):
        resumed, folder = self.resume("removed")
        self.assertIsNone(resumed["worktree"])
        self.assertEqual([s["name"] for s in resumed["skipped"]], ["worktree"], resumed)
        self.assertIn("no longer exists", resumed["skipped"][0]["reason"])
        # The record follows what the goblin works in now.
        record = self.d.wait(lambda: (r if (r := self.d.record(resumed["id"])) else None))
        self.assertIsNone(record["worktree"])


class DevShellWorktreeTests(WorktreeTests):
    """A dev shell whose flake is in a worktree."""

    # The inherited cases ran in the class above.
    for name in [n for n in dir(WorktreeTests) if n.startswith("test_")]:
        locals()[name] = None
    del name

    def flake(self, directory, marker):
        nixpkgs = command(["nix", "eval", "--impure", "--raw", "--expr",
                           f'(builtins.getFlake "{FLAKE}").inputs.nixpkgs.outPath'])
        (directory / "flake.nix").write_text(TEMPLATE.replace("NIXPKGS", nixpkgs).replace("@MARKER@", marker))

    def test_the_initial_launch_and_a_refresh_evaluate_a_worktree_flake(self):
        checkout = self.home_path / "code/api"
        checkout.mkdir(parents=True)
        self.git(checkout, "init", "-q", "-b", "main")
        self.flake(checkout, "original")
        command(["nix", "flake", "lock", f"path:{checkout}"])
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
