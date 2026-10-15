"""Emacs approvals against the real daemon; never loads the user's init file."""
import os
import pty
import select
import signal
import struct
import fcntl
import termios
import time
import json
import shlex
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

from support import command
from daemon_support import Daemon, ROOT, RPC, app, receive


@unittest.skipUnless(shutil.which("emacs"), "Emacs is not installed")
class EmacsTests(unittest.TestCase):
    def setUp(self):
        available = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (locate-library "magit-section") 0 1))'],
            capture_output=True, timeout=10,
        )
        if available.returncode:
            self.skipTest("magit-section is not on Emacs load-path")

    def test_live_activity_updates_overview_sidebar_and_details(self):
        daemon = Daemon(); self.addCleanup(daemon.close)
        agent = daemon.start(agent_name='activity')['session']
        result = subprocess.run(['emacs', '--batch', '-Q', '--eval', '(setq load-prefer-newer t)',
            '-L', str(ROOT/'emacs'), '-l', 'goblins-tests', '-f', 'goblins-test-activity-live'],
            env={**os.environ, 'GOBLINS_APP':str(daemon.app), 'GOBLINS_TEST_STATE':str(daemon.state),
                 'GOBLINS_TEST_AGENT':agent}, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_live_messages_threads_reader_and_updates(self):
        daemon = Daemon(); self.addCleanup(daemon.close)
        chief = daemon.start(agent_name="chief")["session"]
        sent = subprocess.run([str(daemon.app), "--state-dir", str(daemon.state),
                               "send", "chief", "--message", "First question"],
                              capture_output=True, text=True, timeout=10)
        self.assertEqual(sent.returncode, 0, sent.stderr)
        message = json.loads(sent.stdout)
        def sandbox(method, params):
            # The request socket answers one call per connection.
            peer = RPC(daemon.state / chief / "resources/request.sock")
            try:
                return peer.call(method, params)
            finally:
                peer.close()
        claim = sandbox("inbox.next", {"key": "fetch"})
        sandbox("messages.reply", {"key": "reply", "message": message["id"],
                                   "claim_generation": claim["claim_generation"],
                                   "body": "First answer"})
        ready = Path(daemon.temp.name) / "ready"
        emacs = subprocess.Popen(['emacs', '--batch', '-Q', '--eval', '(setq load-prefer-newer t)',
            '-L', str(ROOT/'emacs'), '-l', 'goblins-tests', '-f', 'goblins-test-messages-live'],
            env={**os.environ, 'GOBLINS_TEST_STATE': str(daemon.state),
                 'GOBLINS_TEST_READY': str(ready)},
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.addCleanup(lambda: emacs.poll() is None and emacs.kill())
        daemon.wait(lambda: ready.exists() or emacs.poll() is not None, timeout=60)
        if ready.exists():
            # Emacs sent the second question; claiming it changes a snapshot,
            # the answer later on doesn't.
            claim = sandbox("inbox.next", {"key": "fetch-second"})
            time.sleep(3)
            sandbox("messages.reply", {"key": "reply-second", "message": claim["message"]["id"],
                                       "claim_generation": claim["claim_generation"],
                                       "body": "Second answer"})
        output, _ = emacs.communicate(timeout=60)
        self.assertEqual(emacs.returncode, 0, output)

    def test_live_ownership_tree_and_stopped_branches(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        parent = daemon.start(agent_name="parent")
        other = daemon.start(agent_name="other")
        def child(owner, name):
            launch = daemon.rpc.call("sessions.start", {
                "key":uuid.uuid4().hex,"name":"shell","agent_name":name,
                "configuration":daemon.manifest,"parent":owner["session"],"rows":24,"cols":100,
            })
            daemon.wait(lambda: daemon.get(launch["session"])["state"] == "running")
            return launch
        kid = child(parent, "kid")
        leaf = child(kid, "leaf")
        child(other, "kid")
        result = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
             "-l", "goblins.el", "-l", "goblins-tests.el", "-f", "goblins-test-tree-live"],
            env={**os.environ,"GOBLINS_TEST_STATE":str(daemon.state),
                 "GOBLINS_TEST_PARENT":parent["session"],"GOBLINS_TEST_CHILD":kid["session"],
                 "GOBLINS_TEST_LEAF":leaf["session"]},
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(daemon.get(other["session"])["state"], "running")

    def test_start_server_from_status(self):
        application = app()
        with tempfile.TemporaryDirectory(prefix="ge-start-") as state:
            try:
                result = subprocess.run(
                    ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
                     "-l", "goblins-tests", "-f", "goblins-test-start-live"],
                    env={**os.environ, "GOBLINS_APP": str(application),
                         "GOBLINS_TEST_STATE": state},
                    capture_output=True, text=True, timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                peer = RPC(os.path.join(state, "host.sock"))
                try:
                    self.assertEqual(peer.call("server.status", {})["state"], "running")
                finally:
                    peer.close()
            finally:
                subprocess.run([str(application), "--state-dir", state, "server", "stop"],
                               capture_output=True, check=True, timeout=20)

    def test_live_status_accept_deny_reconnect_and_close(self):
        daemon = Daemon()
        self.addCleanup(daemon.close)
        agents = [daemon.start(agent_name=name) for name in ("snikk", "zoggit")]
        peers, requests = [], []
        for agent in agents:
            peer = RPC(daemon.state / agent["session"] / "resources/request.sock")
            self.addCleanup(peer.close)
            peer.send("permissions.request", {
                "kind": "package", "package": "hello", "reason": "Try a live grant 🐟",
            })
            requests.append(daemon.wait(lambda: next(iter(daemon.permissions(
                session=agent["session"], state="pending")), None)))
            peers.append(peer)
        result = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
             "-l", "goblins-tests", "-f", "goblins-test-live"],
            env={**os.environ, "GOBLINS_TEST_STATE": str(daemon.state),
                 "GOBLINS_TEST_ACCEPT": requests[0]["id"],
                 "GOBLINS_TEST_DENY": requests[1]["id"]},
            capture_output=True, text=True, timeout=90,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(receive(peers[0].peer)["result"]["status"], "ready")
        self.assertEqual(receive(peers[1].peer)["result"]["status"], "denied")
        for agent in agents:
            self.assertEqual(daemon.get(agent["session"])["state"], "running")

    def test_run_in_ghostel_and_visit_from_status(self):
        available = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (and (locate-library "ghostel") (locate-library "ghostel-module")) 0 1))'],
            capture_output=True, timeout=10,
        )
        if available.returncode:
            self.skipTest("Ghostel and its native module are not installed")
        daemon = Daemon()
        self.addCleanup(daemon.close)
        external = daemon.start()
        # Blank selection exercises the configured default. Keep this fixture
        # shell-only: adding Codex must never turn a default test into model work.
        manifest = json.loads(Path(daemon.manifest).read_text())
        manifest["goblins"] = {"shell": manifest["goblins"]["shell"]}
        manifest_path = Path(daemon.temp.name) / "shell-only.json"
        manifest_path.write_text(json.dumps(manifest))
        launcher = Path(daemon.temp.name) / "goblins-shell-only"
        launcher.write_text(f'#!/bin/sh\nexec {shlex.quote(daemon.binary)} --runtime {shlex.quote(str(manifest_path))} "$@"\n')
        launcher.chmod(0o700)
        with tempfile.TemporaryDirectory(prefix="ge-workspace-") as workspace:
            result = subprocess.run(
                ["emacs", "--batch", "-Q", "--eval", "(setq load-prefer-newer t)", "-L", str(ROOT / "emacs"),
                 "-l", "goblins-tests", "-f", "goblins-test-run-live"],
                env={**os.environ, "GOBLINS_APP": str(launcher),
                     "GOBLINS_TEST_STATE": str(daemon.state),
                     "GOBLINS_TEST_WORKSPACE": workspace,
                     "GOBLINS_TEST_EXTERNAL": external["session"]},
                capture_output=True, text=True, timeout=90,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((Path(workspace) / "emacs-launch.txt").read_text(), "ghostel-run-ok")


    def test_live_workspace_in_a_terminal_emacs(self):
        """Drive the workspace in a real `emacs -nw` against an isolated daemon.

        The daemon runs on a temporary state directory and socket, so the
        user's own daemon is never touched, and Emacs runs in a pseudo
        terminal, so no window appears."""
        daemon = Daemon()
        self.addCleanup(daemon.close)
        projects = tempfile.TemporaryDirectory(prefix="ge-projects-")
        self.addCleanup(projects.cleanup)
        one, two = Path(projects.name) / "one", Path(projects.name) / "two"
        one.mkdir()
        two.mkdir()

        def start(name, cwd):
            launch = daemon.rpc.call("sessions.start", {
                "key": uuid.uuid4().hex, "name": "shell", "agent_name": name,
                "configuration": daemon.manifest, "cwd": str(cwd), "detached": True,
                "rows": 24, "cols": 100})
            daemon.wait(lambda: daemon.get(launch["session"])["state"] == "running", timeout=90)
            return launch["session"]

        ids = {name: start(name, cwd) for name, cwd in
               [("alpha", one), ("beta", one), ("delta", two), ("gamma", two)]}
        peer, request = daemon.pending(ids["gamma"], "hello")
        self.addCleanup(peer.close)
        result = Path(projects.name) / "result"
        argv = ["emacs", "-nw", "-Q", "--eval", "(setq load-prefer-newer t)",
                "-L", str(ROOT / "emacs"), "-l", "goblins-tests",
                "--eval", "(goblins-test-workspace-live)"]
        env = {**os.environ, "TERM": "xterm-256color",
               "GOBLINS_APP": str(daemon.app),
               "GOBLINS_TEST_STATE": str(daemon.state),
               "GOBLINS_TEST_RESULT": str(result),
               "GOBLINS_TEST_PROJECT_ONE": str(one)}
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(argv[0], argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        screen, deadline, status = b"", time.monotonic() + 240, None
        while status is None and time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    screen = (screen + os.read(fd, 65536))[-20000:]
                except OSError:
                    pass
            done, code = os.waitpid(pid, os.WNOHANG)
            if done:
                status = code
        if status is None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)
        self.assertIsNotNone(status, "terminal Emacs timed out: " + screen.decode(errors="replace")[-2000:])
        self.assertTrue(result.exists(), screen.decode(errors="replace")[-2000:])
        ghostel = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (and (locate-library "ghostel") (locate-library "ghostel-module")) 0 1))'],
            capture_output=True, timeout=10).returncode == 0
        # Attaching a terminal is covered whenever Ghostel is installed.
        self.assertEqual(result.read_text(), "ok ghostel" if ghostel else "ok")
        # The daemon saw every action.
        self.assertEqual(daemon.permissions(session=ids["gamma"])[0]["state"], "denied")
        self.assertIn(daemon.get(ids["delta"])["state"], ("stopping", "stopped"))
        self.assertIsNone(daemon.get(ids["alpha"])["raid"])
        self.assertIsNone(daemon.get(ids["beta"])["raid"])

    def test_tramp_shell_and_editor_in_a_terminal_emacs(self):
        """/goblins: TRAMP, goblins-shell and goblins-editor in a real
        `emacs -nw`, against isolated daemons built with editor.emacs.enable."""
        application = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                                    f"git+file://{ROOT}#checks.x86_64-linux.editor-goblins"])) / "bin/goblins"
        temp = tempfile.TemporaryDirectory(prefix="ge-tramp-")
        self.addCleanup(temp.cleanup)
        server_dir = Path(temp.name) / "server"
        server_dir.mkdir(mode=0o700)
        # The daemon runs emacsclient against this test Emacs only.
        daemon = Daemon(application, env={**os.environ,
                                          "EMACS_SOCKET_NAME": str(server_dir / "goblins-test")})
        self.addCleanup(daemon.close)
        other = Daemon(application, env={**os.environ,
                                          "EMACS_SOCKET_NAME": str(server_dir / "goblins-test")})
        self.addCleanup(other.close)
        other.start(name="plain", agent_name="gribble")
        result = Path(temp.name) / "result"
        argv = ["emacs", "-nw", "-Q", "--eval", "(setq load-prefer-newer t)",
                "-L", str(ROOT / "emacs"), "-l", "goblins-tests",
                "--eval", "(goblins-test-tramp-live)"]
        env = {**os.environ, "TERM": "xterm-256color",
               "GOBLINS_APP": str(application),
               "GOBLINS_TEST_STATE": str(daemon.state),
               "GOBLINS_TEST_OTHER_STATE": str(other.state),
               "GOBLINS_TEST_MANIFEST": str(daemon.manifest),
               "GOBLINS_TEST_SERVER_DIR": str(server_dir),
               "GOBLINS_TEST_SERVER_NAME": "goblins-test",
               "GOBLINS_TEST_RESULT": str(result)}
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(argv[0], argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        screen, deadline, status = b"", time.monotonic() + 400, None
        while status is None and time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    screen = (screen + os.read(fd, 65536))[-20000:]
                except OSError:
                    pass
            done, code = os.waitpid(pid, os.WNOHANG)
            if done:
                status = code
        if status is None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)
        self.assertIsNotNone(status, "terminal Emacs timed out: " + screen.decode(errors="replace")[-2000:])
        self.assertTrue(result.exists(), screen.decode(errors="replace")[-2000:])
        ghostel = subprocess.run(
            ["emacs", "--batch", "-Q", "--eval",
             '(kill-emacs (if (and (locate-library "ghostel") (locate-library "ghostel-module")) 0 1))'],
            capture_output=True, timeout=10).returncode == 0
        self.maxDiff = None
        self.assertEqual(result.read_text(), "ok ghostel" if ghostel else "ok")

if __name__ == "__main__":
    unittest.main(verbosity=2)
