"""Packaged server lifecycle, help and real shell completion scripts."""
from pathlib import Path
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import time
import unittest
from daemon_support import RPC, app


class ServerTests(unittest.TestCase):
    def setUp(self):
        self.app = app()
        self.temp = tempfile.TemporaryDirectory(prefix="gv-")
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name) / "state"
        self.addCleanup(self.stop_server)

    def cli(self, *args, code=0):
        result = subprocess.run([str(self.app), "--state-dir", str(self.state), *args],
                                text=True, capture_output=True, timeout=25)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        return result.stdout

    def stop_server(self):
        subprocess.run([str(self.app), "--state-dir", str(self.state), "server", "stop"],
                       capture_output=True, timeout=25)

    def test_background_start_status_logs_stop_and_restart(self):
        self.assertIn("Server stopped", self.cli("server", "status", code=1))
        self.assertFalse(self.state.exists())
        self.assertIn("No server output", self.cli("server", "logs"))
        self.assertIn("already stopped", self.cli("server", "stop"))
        self.assertIn("Server started", self.cli("server", "start"))
        client = RPC(self.state / "host.sock"); self.addCleanup(client.close)
        instance = client.init["instance"]
        self.assertIn(instance, self.cli("server", "status"))
        self.assertIn("already running", self.cli("server", "start"))
        _, manifest = re.search(r'exec (\S+) --runtime (\S+)', self.app.read_text()).groups()
        launches = [client.call("sessions.start", dict(key=str(i), name="shell", agent_name=name,
                    configuration=manifest, rows=24, cols=100)) for i, name in enumerate(("snikk", "grib"))]
        deadline = time.monotonic() + 40
        while True:
            records = client.call("sessions.list", {})
            if all(r["state"] == "running" for r in records):
                break
            self.assertLess(time.monotonic(), deadline, records)
            time.sleep(.02)
        pids = [r["identity"]["pid"] for r in records]
        self.assertIn("2 live agents", self.cli("server", "status"))
        self.assertIn("daemon ready", self.cli("server", "logs"))
        self.assertEqual((self.state / "server.log").stat().st_mode & 0o777, 0o600)
        self.cli("server", "stop")
        self.assertFalse((self.state / "host.sock").exists())
        self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in pids))
        self.assertTrue(all(not (self.state / s["session"]).exists() for s in launches))
        self.assertIn("Server stopped", self.cli("server", "status", code=1))
        # The log keeps captured output across starts; no sessions are recovered.
        self.assertIn("Goblins server stopped", self.cli("server", "logs"))
        self.cli("server", "start")
        second = RPC(self.state / "host.sock"); self.addCleanup(second.close)
        self.assertNotEqual(second.init["instance"], instance)
        self.assertEqual(second.call("sessions.list", {}), [])
        self.assertEqual(self.cli("server", "logs").count("daemon ready"), 2)
        print("EVIDENCE background server survives launching CLI, stops both agents, retains output and restarts with fresh identity", flush=True)

    def test_help_removed_aliases_and_completion_installation(self):
        for args in [(), ("--help",), ("help",)]:
            text = self.cli(*args)
            for command in ("server", "tui", "run", "stop", "completions"):
                self.assertIn(command, text)
            self.assertNotRegex(text, r"(?m)^  (shell|daemon|serve)\s")
        self.assertIn("--name", self.cli("help", "run"))
        self.assertIn("--foreground", self.cli("server", "start", "--help"))
        self.assertIn("logs", self.cli("server", "--help"))
        self.cli("shell", code=2)
        self.cli("daemon", code=2)
        self.cli("serve", code=2)
        self.assertIn("--plain", self.cli("tui", "--help"))
        root = self.app.resolve().parent.parent
        paths = {"bash": root / "share/bash-completion/completions/goblins",
                 "fish": root / "share/fish/vendor_completions.d/goblins.fish",
                 "zsh": root / "share/zsh/site-functions/_goblins"}
        for shell, path in paths.items():
            self.assertTrue(path.is_file(), path)
            script = self.cli("completions", shell)
            self.assertEqual(script, path.read_text())
            self.assertIn("server", script)
            self.assertRegex(script, r"\btui\b")
            self.assertNotRegex(script, r"\bserve\b")
            self.assertIn("shell", script)  # The run configuration, not an alias.
            self.assertIn("-l name" if shell == "fish" else "--name", script)
        subprocess.run(["zsh", "-n", str(paths["zsh"])], check=True)
        fish = shutil.which("fish")
        self.assertIsNotNone(fish, "Fish is required to exercise packaged completions")
        script = "source " + shlex.quote(str(paths["fish"]))
        for line, expected in [("goblins ", "server"), ("goblins server ", "logs"),
                               ("goblins run ", "shell"), ("goblins run --name scout ", "shell"),
                               ("goblins completions ", "fish"), ("goblins run shell --", "--name")]:
            completed = subprocess.check_output([fish, "--no-config", "-c",
                        script + "; complete -C " + shlex.quote(line)], text=True)
            self.assertIn(expected, completed)
            if line == "goblins ":
                candidates = [row.split("\t")[0] for row in completed.splitlines()]
                self.assertIn("tui", candidates)
                self.assertNotIn("serve", candidates)
                self.assertNotIn("shell", candidates)
        bash = shutil.which("bash")
        completed = subprocess.check_output([bash, "--noprofile", "--norc", "-c",
            "source " + shlex.quote(str(paths["bash"])) +
            "; COMP_WORDS=(goblins run ''); COMP_CWORD=2; _goblins goblins '' run; printf '%s\\n' \"${COMPREPLY[@]}\""], text=True)
        self.assertIn("shell", completed)
        self.assertFalse(self.state.exists(), "help/completions must not start a server")

    def test_start_refuses_symlinked_log_without_modifying_target(self):
        self.state.mkdir(mode=0o700)
        target = Path(self.temp.name) / "private"
        target.write_text("unchanged")
        (self.state / "server.log").symlink_to(target)
        self.cli("server", "start", code=1)
        self.assertEqual(target.read_text(), "unchanged")
        self.assertFalse((self.state / "host.sock").exists())

    def test_live_names_in_bash_fish_and_zsh_completions(self):
        from terminal_support import Terminal
        self.state = Path(self.temp.name) / "goblins"
        root = self.app.resolve().parent.parent
        scripts = {"bash": root / "share/bash-completion/completions/goblins",
                   "fish": root / "share/fish/vendor_completions.d/goblins.fish",
                   "zsh": root / "share/zsh/site-functions/_goblins"}
        env = {**os.environ, "PATH": str(self.app.parent) + ":" + os.environ["PATH"],
               "XDG_RUNTIME_DIR": self.temp.name}

        def complete(shell, words):
            source = "source " + shlex.quote(str(scripts[shell])) + "; "
            if shell == "fish":
                line = shlex.join(words[:-1]) + " " + words[-1]
                code = source + "complete -C " + shlex.quote(line)
                args = ["fish", "--no-config", "-c", code]
            else:
                code = (source + "COMP_WORDS=(" + shlex.join(words) + "); " +
                        f"COMP_CWORD={len(words)-1}; _goblins goblins " +
                        shlex.quote(words[-1]) + " " + shlex.quote(words[-2]) +
                        "; printf '%s\\n' \"${COMPREPLY[@]}\"")
                args = ["bash", "--noprofile", "--norc", "-c", code]
            result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            return [line.split("\t")[0] for line in result.stdout.splitlines() if line]

        for shell in ("bash", "fish"):
            self.assertEqual(complete(shell, ["goblins", "attach", ""]), [])
        self.assertFalse(self.state.exists())
        self.cli("server", "start")
        client = RPC(self.state / "host.sock")
        self.addCleanup(client.close)
        _, manifest = re.search(r'exec (\S+) --runtime (\S+)', self.app.read_text()).groups()
        launches = [client.call("sessions.start", dict(key=name, name="shell", agent_name=name,
                    configuration=manifest, rows=24, cols=100)) for name in ("snikk", "scout")]
        for shell in ("bash", "fish"):
            for command in ("attach", "detatch", "detach"):
                self.assertEqual(set(complete(shell, ["goblins", command, ""])), {"snikk", "scout"})
                self.assertEqual(complete(shell, ["goblins", command, "sn"]), ["snikk"])
                self.assertEqual(set(complete(shell, ["goblins", "--state-dir", str(self.state), command, ""])), {"snikk", "scout"})
                self.assertEqual(set(complete(shell, ["goblins", command, "--state-dir=" + str(self.state), ""])), {"snikk", "scout"})
                self.assertEqual(complete(shell, ["goblins", command, "--state-dir", str(Path(self.temp.name) / "absent state"), ""]), [])
                self.assertNotIn("snikk", complete(shell, ["goblins", command, "snikk", ""]))
            self.assertNotIn("snikk", complete(shell, ["goblins", "attach", "--instance", "exact", ""]))

        # Exercise real Zsh completion through ZLE, including alias registration.
        zsh = Terminal([shutil.which("zsh"), "-f"], env=env)
        def close_zsh():
            if not zsh.reaped:
                import contextlib
                import signal
                with contextlib.suppress(ProcessLookupError):
                    os.kill(zsh.pid, signal.SIGKILL)
            zsh.close()
        self.addCleanup(close_zsh)
        zsh.send("autoload -Uz compinit; compinit -D; source " + shlex.quote(str(scripts["zsh"])) +
                 "; PS1='completion> '; bindkey '^U' kill-whole-line; print COMPLETION_READY\n")
        zsh.expect(r"(?:^|\n)COMPLETION_READY\n")
        for command in ("attach", "detatch", "detach"):
            zsh.send(f"goblins --state-dir {shlex.quote(str(self.state))} {command} s\t\t")
            zsh.expect("scout")
            zsh.expect("snikk")
            # Clear through ZLE: test runners can inherit ignored SIGINT, so
            # Ctrl-C is not a reliable way to cancel an interactive input line.
            zsh.send("\x15print COMPLETION_RESET\n")
            zsh.expect(r"(?:^|\n)COMPLETION_RESET\n")
        zsh.send("exit\n")
        self.assertEqual(zsh.wait(), 0)

        client.call("sessions.stop", {"session": launches[1]["session"]})
        deadline = time.monotonic() + 30
        while client.call("sessions.get", {"session": launches[1]["session"]})["state"] not in ("stopped", "failed"):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        for shell in ("bash", "fish"):
            self.assertEqual(complete(shell, ["goblins", "attach", ""]), ["snikk"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
