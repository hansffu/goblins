"""Private scope startup and descriptor lifetime, without daemon/CLI startup."""
import array
import json
import os
from pathlib import Path
import select
import signal
import socket
import time
import tempfile
import unittest

from build_support import fixture_path
from support import FLAKE


class ScopeProcessTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.executable = os.environ.get("GOBLINS_LOCAL_SCOPE_RUNTIME") or str(
            fixture_path(FLAKE + "#checks.x86_64-linux.scope-goblins.supervisor") /
            "bin/goblins-supervisor-bwrap")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-scope-process-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def spawn(self, stream, extra=(), wrong_parent=False):
        pid = os.fork()
        if pid == 0:
            try:
                if wrong_parent:
                    # The socketpair's peer credentials still name the test
                    # process, but the executable's parent is this intermediary.
                    child = os.fork()
                    if child:
                        _, status = os.waitpid(child, 0)
                        os._exit(os.waitstatus_to_exitcode(status))
                os.dup2(stream.fileno(), 3, inheritable=True)
                for fd in extra:
                    # Protect it from fd 3's replacement above.
                    os.dup2(fd, 100 + fd, inheritable=True)
                env = {**os.environ, "XDG_CACHE_HOME": str(self.root / "cache")}
                os.execve(self.executable, [self.executable], env)
            finally:
                os._exit(127)
        self.addCleanup(self.reap, pid, True)
        return pid

    def reap(self, pid, kill=False):
        try:
            if kill:
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    return os.waitstatus_to_exitcode(status)
                os.kill(pid, signal.SIGKILL)
                _, status = os.waitpid(pid, 0)
                return os.waitstatus_to_exitcode(status)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    return os.waitstatus_to_exitcode(status)
                time.sleep(.01)
            self.fail("scope process did not exit")
        except ChildProcessError:
            return None

    @staticmethod
    def send(stream, method, params, fds=()):
        body = json.dumps({"jsonrpc": "2.0", "method": method, "params": params}).encode()
        header = f"Content-Length: {len(body)}\r\nDescriptor-Count: {len(fds)}\r\n\r\n".encode()
        ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds))] if fds else []
        # These small fixture messages fit in one write; production backpressure
        # and fragmentation are exercised by runtime's channel tests.
        stream.sendmsg([header + body], ancillary)

    def receive(self, stream):
        data, fds = b"", []
        deadline = time.monotonic() + 20
        try:
            while time.monotonic() < deadline:
                if not select.select([stream], [], [], .1)[0]:
                    continue
                chunk, ancillary, flags, _ = stream.recvmsg(65536, socket.CMSG_SPACE(240 * array.array("i").itemsize))
                self.assertFalse(flags & socket.MSG_CTRUNC)
                for level, kind, value in ancillary:
                    self.assertEqual((level, kind), (socket.SOL_SOCKET, socket.SCM_RIGHTS))
                    received = array.array("i")
                    received.frombytes(value)
                    fds.extend(received)
                self.assertTrue(chunk, "scope channel closed before reply")
                data += chunk
                if b"\r\n\r\n" in data:
                    header, body = data.split(b"\r\n\r\n", 1)
                    fields = dict(line.split(b":", 1) for line in header.split(b"\r\n"))
                    length = int(fields[b"Content-Length"])
                    if len(body) == length:
                        self.assertEqual(len(fds), int(fields[b"Descriptor-Count"]))
                        return json.loads(body)
            self.fail("scope reply timed out")
        finally:
            for fd in fds:
                os.close(fd)

    def pair(self):
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        return left, right

    def test_startup_closes_unrelated_descriptors_before_configuration(self):
        control, child = self.pair()
        read, write = os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close, read)
        pid = self.spawn(child, extra=(write,))
        child.close()
        os.close(write)
        self.assertTrue(select.select([read], [], [], 10)[0], "scope retained an unrelated pipe")
        self.assertEqual(os.read(read, 1), b"")
        self.assertEqual(os.waitpid(pid, os.WNOHANG), (0, 0), "scope should await its first message")
        control.close()
        self.assertEqual(self.reap(pid), 0)

    def test_parent_death_kills_even_a_stopped_scope_process(self):
        report_read, report_write = os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close, report_read)
        parent = os.fork()
        if parent == 0:
            try:
                os.close(report_read)
                control, child = socket.socketpair()
                ready_read, ready_write = os.pipe2(os.O_CLOEXEC)
                scope_pid = os.fork()
                if scope_pid == 0:
                    os.dup2(ready_write, 100, inheritable=True)
                    os.dup2(child.fileno(), 3, inheritable=True)
                    os.execve(self.executable, [self.executable], os.environ)
                child.close()
                os.close(ready_write)
                # EOF proves the scope has installed PDEATHSIG and applied
                # its fd allowlist, before the test stops its event loop.
                os.read(ready_read, 1)
                os.write(report_write, str(scope_pid).encode())
                while True:
                    signal.pause()
            finally:
                os._exit(127)
        os.close(report_write)
        self.addCleanup(self.reap, parent, True)
        self.assertTrue(select.select([report_read], [], [], 10)[0])
        scope_pid = int(os.read(report_read, 64))
        pidfd = os.pidfd_open(scope_pid)
        try:
            signal.pidfd_send_signal(pidfd, signal.SIGSTOP)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                status = Path(f"/proc/{scope_pid}/status").read_text()
                if any(line.startswith("State:") and "T" in line for line in status.splitlines()):
                    break
                time.sleep(.01)
            else:
                self.fail("scope was not stopped")
            os.kill(parent, signal.SIGKILL)
            self.assertEqual(self.reap(parent), -signal.SIGKILL)
            # It cannot process control EOF while stopped; readiness proves
            # that the kernel parent-death signal is the independent backstop.
            self.assertTrue(select.select([pidfd], [], [], 10)[0], "stopped scope survived daemon death")
        finally:
            try:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(pidfd)

    def test_socket_peer_credentials_reject_an_unexpected_parent(self):
        control, child = self.pair()
        pid = self.spawn(child, wrong_parent=True)
        child.close()
        self.assertNotEqual(self.reap(pid), 0)

    def test_membership_eof_before_join_reports_idle_and_waits_for_stop(self):
        control, child = self.pair()
        pid = self.spawn(child)
        child.close()
        directory = self.root / "state/scopes/probe"
        self.send(control, "Setup", {"api": 1, "name": "probe", "directory": str(directory)})
        runtime, member = self.pair()
        self.send(control, "AddMember", {"fd": {"$fd": 0}}, [member.fileno()])
        member.close()
        runtime.close()
        self.assertEqual(self.receive(control), {"jsonrpc": "2.0", "method": "Idle", "params": {"added": 1}})
        self.assertEqual(os.waitpid(pid, os.WNOHANG), (0, 0), "only daemon Stop may end an idle scope")
        self.send(control, "Stop", {})
        self.assertEqual(self.receive(control)["method"], "Stopping")
        self.assertEqual(self.reap(pid), 0)
        self.assertTrue((directory / "lock").is_file())

    def test_mismatched_private_protocol_fails_before_creating_runtime_state(self):
        control, child = self.pair()
        pid = self.spawn(child)
        child.close()
        directory = self.root / "state/scopes/probe"
        self.send(control, "Setup", {"api": 999, "name": "probe", "directory": str(directory)})
        self.assertNotEqual(self.reap(pid), 0)
        self.assertFalse(directory.exists())
