"""One-shot CLI maintenance: direct output, cancellation and cleanup authority."""
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest

from support import cargo_artifacts


class MaintenanceCliTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.controller = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if not cls.controller:
            cls.controller = next(a["executable"] for a in cargo_artifacts(
                "build", "--locked", "--workspace") if a.get("executable")
                and a["target"]["name"] == "goblins")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-maintenance-cli-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.peer = self.root / "peer"
        self.peer.write_text(f"""#!{sys.executable}
import array, ctypes, json, os, socket, struct, sys, time
from pathlib import Path
root = Path(os.environ["MAINTENANCE_FIXTURE_ROOT"])
s = socket.socket(fileno=3)
owner = struct.unpack("3i", s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[0]
if ctypes.CDLL(None).prctl(1, 9, 0, 0, 0) < 0 or os.getppid() != owner: os._exit(1)
data, fds = b"", []
while True:
    chunk, ancillary, flags, _ = s.recvmsg(65536, socket.CMSG_SPACE(2 * array.array("i").itemsize))
    if not chunk: raise RuntimeError("missing request")
    data += chunk
    for level, kind, value in ancillary:
        a = array.array("i"); a.frombytes(value); fds.extend(a)
    if b"\\r\\n\\r\\n" in data:
        header, body = data.split(b"\\r\\n\\r\\n", 1)
        length = int(dict(line.split(b":", 1) for line in header.split(b"\\r\\n"))[b"Content-Length"])
        if len(body) == length: break
request = json.loads(body)
assert request["method"] == "Maintenance" and len(fds) == 2
(root / "peer.tmp").write_text(json.dumps(dict(pid=os.getpid(), pgid=os.getpgrp(), parent=os.getppid(), sid=os.getsid(0))))
(root / "peer.tmp").replace(root / "peer.json")
if os.environ.get("MAINTENANCE_FIXTURE_DESCENDANT"):
    import subprocess
    code = "import ctypes,os; owner=" + str(os.getpid()) + "; libc=ctypes.CDLL(None); libc.prctl(1,9,0,0,0); os._exit(1) if os.getppid()!=owner else None; os.write(2,bytes([254])+b'helper'+bytes([10]))"
    subprocess.run([sys.executable, "-c", code], stderr=fds[1], process_group=0, check=True)
os.write(fds[1], b"\\xffstderr\\n")
result = {{"Ok": None}}
try:
    block = bytes(range(256)) * 16
    for _ in range(1024): os.write(fds[0], block)
except BrokenPipeError:
    result = {{"Err": "output consumer closed"}}
for fd in fds: os.close(fd)
body = json.dumps(dict(jsonrpc="2.0", method="MaintenanceFinished", params=dict(result=result))).encode()
s.sendall(f"Content-Length: {{len(body)}}\\r\\nDescriptor-Count: 0\\r\\n\\r\\n".encode() + body)
s.close()
time.sleep(.1)
""")
        self.peer.chmod(0o700)
        self.env = dict(os.environ, GOBLINS_SUPERVISOR_RUNTIME=str(self.peer),
                        MAINTENANCE_FIXTURE_ROOT=str(self.root),
                        XDG_CACHE_HOME=str(self.root / "cache"),
                        XDG_STATE_HOME=str(self.root / "history"))

    def launch(self):
        p = subprocess.Popen([self.controller, "--state-dir", str(self.root / "state"),
                              "scope", "clear", "--all", "--dry-run"],
                             env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
        self.addCleanup(self.finish, p)
        return p

    def finish(self, p):
        descriptor = getattr(self, "peer_fd", None)
        if descriptor is not None:
            try:
                if not select.select([descriptor], [], [], 0)[0]:
                    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            finally:
                os.close(descriptor)
                self.peer_fd = None
        if p.poll() is None:
            os.killpg(p.pid, signal.SIGKILL)
        p.communicate(timeout=10)

    def ready(self, p):
        path = self.root / "peer.json"
        deadline = time.monotonic() + 10
        while not path.exists():
            self.assertIsNone(p.poll(), "CLI exited before its supervisor")
            self.assertLess(time.monotonic(), deadline, "no maintenance supervisor")
            time.sleep(.005)
        peer = json.loads(path.read_text())
        try:
            self.peer_fd = os.pidfd_open(peer["pid"])
        except ProcessLookupError:
            # A broken consumer can finish before this cleanup handle opens.
            self.peer_fd = None
        return peer

    def test_real_cli_group_signal_cancels_a_separately_grouped_output_blocked_supervisor(self):
        p = self.launch()
        peer = self.ready(p)
        self.assertEqual(os.getpgid(p.pid), p.pid)
        self.assertEqual(peer["pgid"], peer["pid"])
        self.assertNotEqual(peer["pgid"], p.pid)
        descriptor = os.pidfd_open(peer["pid"])
        self.addCleanup(os.close, descriptor)
        started = time.monotonic()
        os.killpg(p.pid, signal.SIGINT)
        # Stdout remains undrained, so this observes control cancellation while
        # the peer is blocked writing. Its stderr diagnostic sink is writable.
        self.assertEqual(p.wait(timeout=5), 1)
        self.assertLess(time.monotonic() - started, 3.5)
        self.assertTrue(select.select([descriptor], [], [], 0)[0], "supervisor still executing")
        output, error = p.communicate(timeout=10)
        self.assertGreater(len(output), 0)
        self.assertLess(len(output), 4 * 1024 * 1024)
        self.assertEqual(error, b"\xffstderr\ngoblins: session cancelled\n")
        self.assertFalse((self.root / "state").exists())
        self.assertFalse((self.root / "cache").exists())

    def test_real_cli_streams_slow_consumer_output_and_binary_stderr_without_operation_timeout(self):
        p = self.launch()
        self.ready(p)
        time.sleep(2.1)
        self.assertIsNone(p.poll(), "output work was given a result deadline")
        output, error = p.communicate(timeout=15)
        self.assertEqual(p.returncode, 0, error)
        self.assertEqual(output, bytes(range(256)) * (4 * 1024 * 1024 // 256))
        self.assertEqual(error, b"\xffstderr\n")

    def test_real_cli_reports_output_failure_after_forwarded_stderr_with_exit_one(self):
        p = self.launch()
        p.stdout.close()
        p.stdout = None
        self.ready(p)
        _, error = p.communicate(timeout=10)
        self.assertEqual(p.returncode, 1)
        self.assertEqual(error, b"\xffstderr\ngoblins: output consumer closed\n")

    def test_controlling_tty_tostop_allows_separate_group_and_inherited_stderr_writers(self):
        import fcntl
        import termios
        master, slave = os.openpty()
        process = None
        try:
            attributes = termios.tcgetattr(slave)
            attributes[1] &= ~termios.OPOST
            attributes[3] |= termios.TOSTOP
            termios.tcsetattr(slave, termios.TCSANOW, attributes)
            def controlling_tty():
                signal.signal(signal.SIGTTOU, signal.SIG_DFL)
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            process = subprocess.Popen(
                [self.controller, "--state-dir", str(self.root / "state"),
                 "scope", "clear", "--all", "--dry-run"],
                env=dict(self.env, MAINTENANCE_FIXTURE_DESCENDANT="1"),
                stdin=slave, stdout=slave, stderr=slave,
                start_new_session=True, preexec_fn=controlling_tty)
            # Keep a private slave handle so terminal settings remain
            # inspectable even after the foreground command exits.
            self.addCleanup(self.finish, process)
            peer = self.ready(process)
            self.assertEqual(os.tcgetpgrp(master), process.pid)
            self.assertEqual(peer["sid"], process.pid)
            self.assertNotEqual(peer["pgid"], process.pid)
            expected = b"\xfehelper\n\xffstderr\n" + bytes(range(256)) * (4 * 1024 * 1024 // 256)
            output = bytearray()
            deadline = time.monotonic() + 15
            while len(output) < len(expected):
                self.assertLess(time.monotonic(), deadline, "TOSTOP stopped a maintenance writer")
                if select.select([master], [], [], .1)[0]:
                    output.extend(os.read(master, min(65536, len(expected) - len(output))))
            self.assertEqual(bytes(output), expected)
            self.assertTrue(termios.tcgetattr(master)[3] & termios.TOSTOP)
            self.assertEqual(process.wait(timeout=5), 0)
            self.assertIsNotNone(self.peer_fd)
            self.assertTrue(select.select([self.peer_fd], [], [], 0)[0], "supervisor not reaped/exited")
        finally:
            if slave is not None:
                os.close(slave)
            if process is not None:
                self.finish(process)
            os.close(master)

    def test_name_validation_and_process_free_help_preserve_precedence_without_a_pin(self):
        for pin in [None, "relative-supervisor"]:
            env = dict(self.env)
            if pin is None:
                env.pop("GOBLINS_SUPERVISOR_RUNTIME", None)
            else:
                env["GOBLINS_SUPERVISOR_RUNTIME"] = pin
            result = subprocess.run(
                [self.controller, "--state-dir", str(self.root / "state"),
                 "scope", "clear", "../invalid"],
                env=env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stderr, b"goblins: invalid scope name\n")
            self.assertNotIn(b"supervisor executable", result.stderr)
            for args in [["--help"], ["completions", "bash"]]:
                result = subprocess.run([self.controller, *args], env=env,
                                        capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "state").exists())
        self.assertFalse((self.root / "cache").exists())
        self.assertFalse((self.root / "peer.json").exists())


class MaintenanceHelperTests(unittest.TestCase):
    """Real helper backstops; requires the separately approved host context."""
    @classmethod
    def setUpClass(cls):
        from build_support import fixture_path
        from support import FLAKE
        cls.controller = os.environ.get("GOBLINS_LOCAL_CONTROLLER")
        if not cls.controller:
            cls.controller = next(a["executable"] for a in cargo_artifacts(
                "build", "--locked", "--workspace") if a.get("executable")
                and a["target"]["name"] == "goblins")
        cls.supervisor = os.environ.get("GOBLINS_SUPERVISOR_RUNTIME") or str(
            fixture_path(FLAKE + "#checks.x86_64-linux.exec-goblins.supervisor") /
            "bin/goblins-supervisor-bwrap")
        cls.original = json.loads(fixture_path(
            FLAKE + "#checks.x86_64-linux.exec-goblins.config").read_text())
        cls.real_helper = next(iter(cls.original["goblins"].values()))["helper"]

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-maintenance-helper-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.selected = self.root / "cache/goblins/scopes/scope-work"
        self.selected.mkdir(parents=True, mode=0o700)
        for path in [self.selected.parent, self.selected.parent.parent]:
            path.chmod(0o700)
        (self.selected / "payload").write_text("selected fixture retained\n")
        self.outside = self.root / "outside"
        self.outside.mkdir(mode=0o700)
        (self.outside / "payload").write_text("outside fixture retained\n")
        (self.selected / "link").symlink_to(self.outside, target_is_directory=True)
        self.saved = self.selected.with_name("saved-work")
        self.release = self.root / "release"
        self.supervisor_wrapper = self.root / "supervisor"
        self.supervisor_wrapper.write_text(f"""#!{sys.executable}
import json, os
from pathlib import Path
root = Path({str(self.root)!r})
(root / "supervisor.tmp").write_text(json.dumps(dict(pid=os.getpid(), pgid=os.getpgrp())))
(root / "supervisor.tmp").replace(root / "supervisor.json")
os.execv({self.supervisor!r}, [{self.supervisor!r}])
""")
        self.supervisor_wrapper.chmod(0o700)
        self.helper_wrapper = self.root / "helper"
        self.helper_wrapper.write_text(f"""#!{sys.executable}
import json, os, sys, time
from pathlib import Path
root = Path({str(self.root)!r})
(root / "helper.tmp").write_text(json.dumps(dict(pid=os.getpid(), parent=os.getppid())))
(root / "helper.tmp").replace(root / "helper.json")
deadline = time.monotonic() + 8
while not (root / "release").exists():
    if time.monotonic() >= deadline: os._exit(91)
    time.sleep(.002)
os.execv({self.real_helper!r}, [{self.real_helper!r}, *sys.argv[1:]])
""")
        self.helper_wrapper.chmod(0o700)
        manifest = json.loads(json.dumps(self.original))
        for config in manifest["goblins"].values():
            config["helper"] = str(self.helper_wrapper)
        self.manifest = self.root / "manifest.json"
        self.manifest.write_text(json.dumps(manifest))
        self.env = dict(os.environ, LC_ALL="C",
                        GOBLINS_SUPERVISOR_RUNTIME=str(self.supervisor_wrapper),
                        XDG_CACHE_HOME=str(self.root / "cache"),
                        XDG_STATE_HOME=str(self.root / "history"))

    def marker(self, name, process):
        path = self.root / (name + ".json")
        deadline = time.monotonic() + 7
        while not path.exists():
            self.assertIsNone(process.poll(), f"CLI exited before {name}")
            self.assertLess(time.monotonic(), deadline, f"missing {name} marker")
            time.sleep(.002)
        return json.loads(path.read_text())

    @staticmethod
    def text_at(directory, name):
        with os.fdopen(os.open(name, os.O_RDONLY | os.O_CLOEXEC,
                               dir_fd=directory), "rb") as file:
            value = file.read(4097)
        if len(value) > 4096:
            raise AssertionError("owned helper proc record exceeds observation bound")
        return value.decode()

    @staticmethod
    def pending_write(directory, words):
        import ctypes
        class Iovec(ctypes.Structure):
            _fields_ = [("base", ctypes.c_void_p), ("length", ctypes.c_size_t)]
        ids = {"x86_64": (1, 20), "aarch64": (64, 66)}
        write, writev = ids[os.uname().machine]
        number = int(words[0])
        if number not in (write, writev) or int(words[1], 0) != 2:
            return b""
        with os.fdopen(os.open("mem", os.O_RDONLY | os.O_CLOEXEC,
                               dir_fd=directory), "rb", buffering=0) as memory:
            def read(address, size):
                if not 0 <= size <= 4096:
                    raise AssertionError("owned helper syscall buffer exceeds read bound")
                memory.seek(address)
                return memory.read(size)
            address, count = int(words[2], 0), int(words[3], 0)
            if number == write:
                return read(address, min(count, 4096))
            if not 0 < count <= 16:
                raise AssertionError("owned helper iovec count exceeds observation bound")
            array = read(address, count * ctypes.sizeof(Iovec))
            result = bytearray()
            for index in range(count):
                vector = Iovec.from_buffer_copy(array, index * ctypes.sizeof(Iovec))
                # Total inspected payload, not only each iovec, is bounded.
                result.extend(read(vector.base, min(vector.length, 4096 - len(result))))
                if len(result) == 4096:
                    break
            return bytes(result)

    @staticmethod
    def subordinate(kind, identity):
        import pwd
        uid = os.getuid()
        names = {str(uid), pwd.getpwuid(uid).pw_name}
        for line in Path("/etc/sub" + kind).read_text().splitlines():
            fields = line.split(":")
            if len(fields) == 3 and fields[0] in names:
                start, count = int(fields[1]), int(fields[2])
                if count >= 65536 and not start <= identity < start + 65536:
                    return start
        raise AssertionError(f"no fixture-capable subordinate {kind} mapping")

    def observe_helper(self, directory, helper_fd, postmapping):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            self.assertFalse(select.select([helper_fd], [], [], 0)[0],
                             "real helper exited before required phase observation")
            executable = os.readlink("exe", dir_fd=directory)
            if executable != self.real_helper:
                time.sleep(.002)
                continue
            words = self.text_at(directory, "syscall").split()
            if words[0] == "running":
                time.sleep(.002)
                continue
            if postmapping:
                uid_map = [tuple(map(int, line.split())) for line in
                           self.text_at(directory, "uid_map").splitlines()]
                gid_map = [tuple(map(int, line.split())) for line in
                           self.text_at(directory, "gid_map").splitlines()]
                expected_uid = [(0, os.getuid(), 1),
                                (1, self.subordinate("uid", os.getuid()), 65536)]
                expected_gid = [(0, os.getgid(), 1),
                                (1, self.subordinate("gid", os.getgid()), 65536)]
                if uid_map == expected_uid and gid_map == expected_gid:
                    pending = self.pending_write(directory, words)
                    if b"No such file or directory" in pending:
                        self.assertFalse(select.select([helper_fd], [], [], 0)[0])
                        return
            else:
                reads = {"x86_64": 0, "aarch64": 63}
                if int(words[0]) == reads[os.uname().machine] and int(words[1], 0) == 0:
                    # The genuine helper reads stdin only after READY. Its
                    # supervisor is stopped, so no mapping/release has occurred.
                    self.assertEqual(self.text_at(directory, "uid_map").strip(), "")
                    return
            time.sleep(.002)
        self.fail("required owned-helper phase was not observed; no backstop coverage claimed")

    def probe(self, postmapping):
        import fcntl
        import threading
        error_read, error_write = os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close, error_read)
        if postmapping:
            capacity = fcntl.fcntl(error_write, fcntl.F_GETPIPE_SZ)
            fill = b"F" * (capacity - len(b"goblins helper: "))
            offset = 0
            while offset < len(fill):
                offset += os.write(error_write, fill[offset:])
        process = subprocess.Popen(
            [self.controller, "--runtime", str(self.manifest),
             "--state-dir", str(self.root / "state"), "scope", "clear", "work"],
            env=self.env, stdout=subprocess.PIPE, stderr=error_write,
            start_new_session=True)
        os.close(error_write)
        supervisor_fd = helper_fd = directory = None
        collected = []
        drain = None
        def drain_errors():
            while True:
                data = os.read(error_read, 8192)
                if not data:
                    break
                collected.append(data)
        try:
            supervisor = self.marker("supervisor", process)
            helper = self.marker("helper", process)
            self.assertEqual(helper["parent"], supervisor["pid"])
            self.assertEqual(supervisor["pgid"], supervisor["pid"])
            self.assertNotEqual(supervisor["pgid"], process.pid)
            supervisor_fd = os.pidfd_open(supervisor["pid"])
            helper_fd = os.pidfd_open(helper["pid"])
            directory = os.open(f"/proc/{helper['pid']}", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
            locks = [self.root / "cache/goblins/locks/storage.lock",
                     self.root / "cache/goblins/scopes/locks/work"]
            inodes = [path.stat().st_ino for path in locks]
            for path in locks:
                with path.open("r+b") as lock:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if postmapping:
                self.selected.rename(self.saved)
                self.assertFalse(self.selected.exists())
            else:
                signal.pidfd_send_signal(supervisor_fd, signal.SIGSTOP)
            self.release.touch()
            self.observe_helper(directory, helper_fd, postmapping)
            if postmapping:
                signal.pidfd_send_signal(supervisor_fd, signal.SIGSTOP)
            os.killpg(process.pid, signal.SIGINT)
            self.assertTrue(select.select([supervisor_fd], [], [], 4)[0],
                            "fixed supervisor cleanup bound did not terminate stopped child")
            self.assertTrue(select.select([helper_fd], [], [], 2)[0],
                            "real helper survived its supervisor parent")
            # Supervisor reap never substitutes for this independent helper
            # exit observation and inherited authority-release observation.
            for path, inode in zip(locks, inodes):
                self.assertEqual(path.stat().st_ino, inode)
                with path.open("r+b") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            retained = self.saved if postmapping else self.selected
            self.assertEqual((retained / "payload").read_text(), "selected fixture retained\n")
            self.assertEqual((self.outside / "payload").read_text(), "outside fixture retained\n")
            # Keep a full stderr sink untouched until helper death and lock
            # release are proven; only now unblock the CLI's final diagnostic.
            drain = threading.Thread(target=drain_errors)
            drain.start()
            output, _ = process.communicate(timeout=10)
            drain.join(timeout=5)
            self.assertFalse(drain.is_alive())
            self.assertEqual(process.returncode, 1)
            self.assertIn(b"goblins: session cancelled\n", b"".join(collected))
            self.assertNotIn(b"Removed ", output)
        finally:
            # Cleanup acts only on pidfds of these owned fixture processes.
            if helper_fd is not None and not select.select([helper_fd], [], [], 0)[0]:
                signal.pidfd_send_signal(helper_fd, signal.SIGKILL)
            self.release.touch(exist_ok=True)
            if supervisor_fd is not None and not select.select([supervisor_fd], [], [], 0)[0]:
                signal.pidfd_send_signal(supervisor_fd, signal.SIGCONT)
            if drain is None:
                drain = threading.Thread(target=drain_errors)
                drain.start()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)
            if supervisor_fd is not None and not select.select([supervisor_fd], [], [], 0)[0]:
                signal.pidfd_send_signal(supervisor_fd, signal.SIGKILL)
            drain.join(timeout=5)
            self.assertFalse(drain.is_alive(), "owned diagnostic pipe did not close")
            for descriptor in (directory, helper_fd, supervisor_fd):
                if descriptor is not None:
                    os.close(descriptor)

    def test_real_ready_helper_exits_and_releases_inherited_locks_after_forced_supervisor_death(self):
        self.probe(False)

    def test_real_postmapping_stderr_blocked_helper_exits_and_releases_locks_after_forced_parent_death(self):
        self.probe(True)
