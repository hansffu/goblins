"""Real two-terminal test: host serve, attached Fish, sandbox request CLI."""
import contextlib
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import tempfile
import termios
import time
import unittest

from runtime import command

ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")


class Terminal:
    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execv(argv[0], argv)
        self.buffer = b""
        self.reaped = False
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))

    def send(self, text):
        os.write(self.fd, text.encode())

    def expect(self, pattern, timeout=30):
        deadline = time.monotonic() + timeout
        regex = re.compile(pattern.encode())
        while True:
            plain = ANSI.sub(b"", self.buffer).replace(b"\r", b"")
            match = regex.search(plain)
            if match:
                self.buffer = plain[match.end():]
                return match
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.fd], [], [], remaining)[0]:
                raise AssertionError(f"timeout waiting for {pattern!r}: {plain[-6000:]!r}")
            try:
                chunk = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b""
            if not chunk:
                raise AssertionError(f"terminal exited waiting for {pattern!r}: {plain[-6000:]!r}")
            self.buffer += chunk

    def wait(self):
        for _ in range(100):
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.reaped = True
                return os.waitstatus_to_exitcode(status)
            time.sleep(.03)
        raise AssertionError("terminal process did not exit")

    def close(self):
        if not self.reaped:
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.pid, signal.SIGTERM)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(self.pid, 0)
            self.reaped = True
        os.close(self.fd)


class ConnectedTests(unittest.TestCase):
    def test_hello_grant_from_real_fish_without_restart(self):
        with tempfile.TemporaryDirectory(prefix="goblins-connected-test-") as directory:
            root = Path(directory)
            # Exercise the public repository-root flake; other integration
            # scenarios retain the standalone experiment flake.
            flake = "path:" + str(Path(__file__).resolve().parents[2])
            app = Path(command(["nix", "build", "--no-write-lock-file", "--print-out-paths", "--out-link", root / "app", flake + "#goblins"])) / "bin/goblins"
            state = root / "control"
            server = Terminal([str(app), "--state-dir", str(state), "serve"])
            self.addCleanup(server.close)
            server.expect("Goblins serving")
            client = Terminal([str(app), "--state-dir", str(state), "shell"])
            self.addCleanup(client.close)
            server.expect("Fish connected")
            client.expect("workspace[>#]")
            client.send("printf 'BEFORE_PID=%s\\n' $fish_pid; command -q hello; printf 'HELLO_ABSENT=%s\\n' $status\n")
            before = client.expect(r"(?:^|\n)BEFORE_PID=(\d+)\r?\n").group(1)
            client.expect(r"(?:^|\n)HELLO_ABSENT=127\r?\n")
            client.send("set before_start (awk '{print $22}' /proc/$fish_pid/stat); set before_ns (readlink /proc/$fish_pid/ns/mnt)\n")

            # An in-sandbox goblins variant has no host lifecycle commands.
            client.send("goblins serve; printf 'NO_SERVE=%s\\n' $status\n")
            client.expect(r"(?:^|\n)NO_SERVE=2\r?\n")
            client.send(f"test -e {state}/serve.sock; printf 'HOST_SOCKET_HIDDEN=%s\\n' $status\n")
            client.expect(r"(?:^|\n)HOST_SOCKET_HIDDEN=1\r?\n")
            client.send("test -e /nix/var/nix/daemon-socket/socket; printf 'NIX_HIDDEN=%s\\n' $status\n")
            client.expect(r"(?:^|\n)NIX_HIDDEN=1\r?\n")
            client.send("grep '^CapEff:' /proc/self/status\n")
            client.expect(r"(?:^|\n)CapEff:\s+0000000000000000\r?\n")

            client.send('goblins request-package hello --reason "connected test denial"; printf \'DENIED_EXIT=%s\\n\' $status\n')
            server.expect("Approve package\\? Type approve or deny:")
            # Request framing has a timeout; human approval deliberately doesn't.
            time.sleep(3.2)
            server.send("deny\n")
            client.expect(r"(?:^|\n)DENIED_EXIT=1\r?\n")
            client.send("command -q hello; printf 'STILL_ABSENT=%s\\n' $status\n")
            client.expect(r"(?:^|\n)STILL_ABSENT=127\r?\n")

            client.send('goblins request-package hello --reason "connected test approval"; printf \'READY_EXIT=%s\\n\' $status\n')
            server.expect("Approve package\\? Type approve or deny:")
            server.send("approve\n")
            client.expect(r"(?:^|\n)READY_EXIT=0\r?\n", timeout=60)
            client.send("hello; printf 'AFTER_PID=%s\\n' $fish_pid; test $before_start = (awk '{print $22}' /proc/$fish_pid/stat); printf 'SAME_START=%s\\n' $status; test $before_ns = (readlink /proc/$fish_pid/ns/mnt); printf 'SAME_NS=%s\\n' $status\n")
            client.expect(r"(?:^|\n)Hello, world!\r?\n")
            after = client.expect(r"(?:^|\n)AFTER_PID=(\d+)\r?\n").group(1)
            self.assertEqual(before, after)
            client.expect(r"(?:^|\n)SAME_START=0\r?\n")
            client.expect(r"(?:^|\n)SAME_NS=0\r?\n")

            # Regression: a real nixpkgs attribute outside the former five-name
            # allowlist, including its Perl interpreter and cow data at runtime.
            client.send("command -q cowsay; printf 'COWSAY_ABSENT=%s\\n' $status\n")
            client.expect(r"(?:^|\n)COWSAY_ABSENT=127\r?\n")
            client.send("goblins request-package cowsay; printf 'COWSAY_READY=%s\\n' $status\n")
            server.expect("Approve package\\? Type approve or deny:")
            server.send("approve\n")
            client.expect(r"(?:^|\n)COWSAY_READY=0\r?\n", timeout=120)
            client.send("cowsay live-grant-ok; printf 'COWSAY_EXIT=%s\\n' $status; printf 'COWSAY_PID=%s\\n' $fish_pid; test $before_ns = (readlink /proc/$fish_pid/ns/mnt); printf 'COWSAY_NS=%s\\n' $status\n")
            client.expect(r"(?:^|\n)< live-grant-ok >\r?\n")
            client.expect(r"(?:^|\n)COWSAY_EXIT=0\r?\n")
            self.assertEqual(client.expect(r"(?:^|\n)COWSAY_PID=(\d+)\r?\n").group(1), before)
            client.expect(r"(?:^|\n)COWSAY_NS=0\r?\n")

            # Unknown attributes and package sets return correlated errors and
            # leave this same shell usable. Neither is an ambiguous disconnect.
            for name in ("goblinsPackageThatDoesNotExist", "python3Packages"):
                client.send(f"goblins request-package {name}; printf 'LOOKUP_EXIT=%s\\n' $status\n")
                server.expect("Approve package\\? Type approve or deny:")
                server.send("approve\n")
                client.expect("cannot resolve package")
                client.expect(r"(?:^|\n)LOOKUP_EXIT=1\r?\n")
            client.send("goblins request-package 'hello^out'; printf 'INVALID_EXIT=%s\\n' $status\n")
            client.expect("package must be a nixpkgs attribute")
            client.expect(r"(?:^|\n)INVALID_EXIT=1\r?\n")

            # Real terminal features survive the request/approval round trip.
            client.send("sleep 30\n")
            time.sleep(.15)
            client.send("\x03")
            client.send("printf 'AFTER_INTERRUPT=%s\\n' $fish_pid\n")
            self.assertEqual(client.expect(r"(?:^|\n)AFTER_INTERRUPT=(\d+)\r?\n").group(1), before)
            fcntl.ioctl(client.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 102, 0, 0))
            client.send("stty size\n")
            client.expect(r"(?:^|\n)31 102\r?\n")
            client.send("exit\n")
            self.assertEqual(client.wait(), 0)
            server.expect("Shell stopped")

            # A fresh session does not inherit the previous session's grant.
            fresh = Terminal([str(app), "--state-dir", str(state), "shell"])
            self.addCleanup(fresh.close)
            server.expect("Fish connected")
            fresh.expect("workspace[>#]")
            fresh.send("command -q hello; printf 'FRESH_ABSENT=%s\\n' $status\n")
            fresh.expect(r"(?:^|\n)FRESH_ABSENT=127\r?\n")
            server.send("quit\n")
            self.assertEqual(server.wait(), 0)
            self.assertEqual(fresh.wait(), 0)
            self.assertFalse((state / "serve.sock").exists())
            print("EVIDENCE connected hello + cowsay: denial, ready, same Fish PID/start/mount namespace, Perl/data dependencies, correlated lookup errors, Ctrl-C, resize, fresh-session isolation, server shutdown", flush=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
