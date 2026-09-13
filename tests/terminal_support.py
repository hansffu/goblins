"""Real two-terminal test: host tui, attached Fish, sandbox request CLI."""
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
import socket
import struct
import tempfile
import termios
import time
import unittest

from support import command

ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")


class Terminal:
    def __init__(self, argv, env=None, output=None):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            if output:
                os.dup2(os.open(output, os.O_WRONLY), 1)
            os.execve(argv[0], argv, env or os.environ)
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
        if self.fd is None:
            return
        if not self.reaped:
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.pid, signal.SIGTERM)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(self.pid, 0)
            self.reaped = True
        os.close(self.fd)
        self.fd = None


def screen_wait(ui, predicate, timeout=30):
    import pyte
    if not hasattr(ui, "screen"):
        ui.screen = pyte.Screen(100, 24)
        ui.parser = pyte.ByteStream(ui.screen)
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if select.select([ui.fd], [], [], .02)[0]:
            ui.parser.feed(os.read(ui.fd, 65536))
        text = "\n".join(ui.screen.display)
        if predicate(text):
            return text
    raise AssertionError("screen condition timed out:\n" + text)
