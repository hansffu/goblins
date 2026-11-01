"""Private scope startup and descriptor lifetime, without daemon/CLI startup."""
import array
import fcntl
import json
import os
import resource
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
        cls.executable = os.environ.get("GOBLINS_SUPERVISOR_RUNTIME") or str(
            fixture_path(FLAKE + "#checks.x86_64-linux.scope-goblins.supervisor") /
            "bin/goblins-supervisor-bwrap")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="goblins-scope-process-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def spawn(self, stream, extra=(), wrong_parent=False, nofile=None):
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
                if nofile is not None:
                    resource.setrlimit(resource.RLIMIT_NOFILE, nofile)
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

    def supervisor_start(self, **changes):
        params = {"supervisor_api": 1, "source": {
            "kind": "Host", "configuration": str(self.root / "absent.json"),
            "name": "probe", "scope": None, "dev_shell": None,
            "workspace": None, "cwd": None, "state": str(self.root),
            "history": None, "recreate": None, "sequence": 1},
            "directory": str(self.root / "runtime"), "dimensions": [24, 80],
            "detached": True, "payload_nofile": None, "handoff": None}
        params.update(changes)
        return params

    def test_supervisor_validates_complete_start_before_creating_runtime(self):
        invalid = [{"supervisor_api": 999}, {"dimensions": [0, 80]},
                   {"directory": "relative"}, {"unexpected": True},
                   {"source": {"kind": "Member", "scope": None,
                               "owner_scope": None, "manifest": str(self.root / "manifest"),
                               "configuration": "probe", "auto": {}, "state": str(self.root),
                               "history": None}}]
        for changes in invalid:
            with self.subTest(changes=changes):
                control, child = self.pair()
                pid = self.spawn(child)
                child.close()
                self.send(control, "SupervisorStart", self.supervisor_start(**changes))
                self.assertNotEqual(self.reap(pid), 0)
                self.assertFalse((self.root / "terminal.sock").exists())
                self.assertFalse((self.root / "runtime").exists())

    def test_supervisor_rejects_physical_payload_limits_before_engine_or_adoption(self):
        parent_limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        for limit in (257, 513, 128):
            with self.subTest(limit=limit):
                control, child = self.pair()
                pid = self.spawn(child, nofile=(256, 512))
                child.close()
                scope, scope_child = self.pair()
                resources = self.root / f"owned-{limit}"
                resources.mkdir(mode=0o700)
                (resources / "held").write_text("sender-owned")
                with (resources / ".gc-lease").open("w+b") as owner, \
                     (resources / ".gc-lease").open("r+b") as transfer:
                    # Independent shared OFDs retain sender ownership until
                    # confirmed child death; the receiver must close its copy.
                    fcntl.flock(owner, fcntl.LOCK_SH | fcntl.LOCK_NB)
                    fcntl.flock(transfer, fcntl.LOCK_SH | fcntl.LOCK_NB)
                    workspace = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
                    try:
                        params = self.supervisor_start(payload_nofile=limit,
                            source=dict(kind="Member", scope={"$fd": 0}, owner_scope=None,
                                        manifest=str(self.root / "absent.json"), configuration="probe",
                                        auto={}, state=str(self.root), history=None),
                            handoff=dict(transfer=99, count=1, workspace={"$fd": 1},
                                         working_directory=None, dev_shell="null", docker=False,
                                         leases=[dict(directory=str(resources), lock={"$fd": 2})],
                                         more=False))
                        self.send(control, "SupervisorStart", params,
                                  [scope_child.fileno(), workspace, transfer.fileno()])
                    finally:
                        os.close(workspace)
                    scope_child.close()
                    transfer.close()
                    if limit == 128:
                        messages = self.supervisor_messages(control)
                        self.assertEqual(self.reap(pid), 0)
                        self.assertEqual([m["params"] for m in messages if m["method"] == "Adopted"],
                                         [dict(transfer=99)])
                        failure = [m["params"]["error"] for m in messages if m["method"] == "Event.Failed"]
                        self.assertEqual(failure, [
                            f"cannot open configuration {self.root / 'absent.json'}: No such file or directory (os error 2)"])
                        self.assertEqual([m["method"] for m in messages].count("Released"), 1)
                    else:
                        self.assertEqual(self.reap(pid), 1)
                        control.settimeout(3)
                        self.assertEqual(control.recv(1), b"", "invalid startup emitted an ACK/event")
                    scope.settimeout(3)
                    self.assertEqual(scope.recv(1), b"", "receiver retained the scope capability")
                    self.assertEqual((resources / "held").read_text(), "sender-owned")
                    # This upgrade fails while an independent receiver lease
                    # survives; it succeeds only after all transferred copies close.
                    fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
                if limit != 128:
                    self.assertFalse((self.root / "runtime").exists())
                    self.assertFalse((self.root / "terminal.sock").exists())
                    self.assertFalse((self.root / "cache").exists())
                self.assertEqual(resource.getrlimit(resource.RLIMIT_NOFILE), parent_limits)

    def test_supervisor_reports_worker_failure_and_releases_before_exit(self):
        parent_limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        for limit in (None, 128):
            with self.subTest(limit=limit):
                self.worker_failure_control(limit, parent_limits)

    def worker_failure_control(self, limit, parent_limits):
        control, child = self.pair()
        pid = self.spawn(child, nofile=(256, 512))
        child.close()
        self.send(control, "SupervisorStart", self.supervisor_start(payload_nofile=limit))
        messages = self.supervisor_messages(control)
        self.assertEqual(self.reap(pid), 0)
        methods = [message["method"] for message in messages]
        self.assertEqual(methods.count("Event.Failed"), 1)
        self.assertEqual(methods.count("Event.Stopped"), 1)
        self.assertEqual(methods.count("Released"), 1)
        self.assertLess(methods.index("Event.Failed"), methods.index("Event.Stopped"))
        self.assertLess(methods.index("Event.Stopped"), methods.index("Released"))
        failure = next(message["params"] for message in messages if message["method"] == "Event.Failed")
        self.assertEqual(failure["error"],
                         f"cannot open configuration {self.root / 'absent.json'}: No such file or directory (os error 2)")
        self.assertEqual(resource.getrlimit(resource.RLIMIT_NOFILE), parent_limits)

    def supervisor_messages(self, control):
        control.settimeout(20)
        data = bytearray()
        while chunk := control.recv(65536):
            data.extend(chunk)
        messages = []
        while data:
            header, body = bytes(data).split(b"\r\n\r\n", 1)
            fields = dict(line.split(b":", 1) for line in header.split(b"\r\n"))
            length = int(fields[b"Content-Length"])
            self.assertEqual(int(fields[b"Descriptor-Count"]), 0)
            self.assertGreaterEqual(len(body), length)
            messages.append(json.loads(body[:length]))
            data = bytearray(body[length:])
        return messages

    def test_wrong_first_mode_message_disposes_its_descriptors(self):
        control, child = self.pair()
        read, write = os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close, read)
        pid = self.spawn(child)
        child.close()
        self.send(control, "Event.Mode", {"server": False, "invalid": {"$fd": 0}}, [write])
        os.close(write)
        self.assertNotEqual(self.reap(pid), 0)
        self.assertTrue(select.select([read], [], [], 10)[0])
        self.assertEqual(os.read(read, 1), b"")
        self.assertFalse((self.root / "terminal.sock").exists())

    def preflight(self, path, sequence=0, **overrides):
        control, child = self.pair()
        pid = self.spawn(child)
        child.close()
        params = dict(supervisor_api=1, path=str(path),
                      directory=str(self.root / "manifests"), sequence=sequence)
        params.update(overrides)
        self.send(control, "Preflight", params)
        return control, pid

    def test_preflight_adopts_an_unrooted_manifest_then_exits_without_an_engine(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps(dict(api=2, helper_api=6, goblins={})))
        # Relative paths retain the inherited working directory.
        control, pid = self.preflight(os.path.relpath(path), sequence=17)
        reply = self.receive(control)
        self.assertEqual(reply["method"], "Event.Manifest")
        params = reply["params"]
        self.assertEqual(params["sequence"], 17)
        self.assertEqual(params["path"], os.path.relpath(path))
        self.assertIsNone(params["root"])
        self.assertIsNone(params["owner"])
        self.assertGreater(params["transfer"], 0)
        self.send(control, "Adopted", dict(transfer=params["transfer"]))
        self.assertEqual(self.reap(pid), 0)
        self.assertEqual(control.recv(1), b"")
        self.assertFalse((self.root / "manifests").exists())
        self.assertFalse((self.root / "terminal.sock").exists())

    def test_preflight_validation_error_is_one_normal_result_before_successful_exit(self):
        path = self.root / "invalid.json"
        path.write_text(json.dumps(dict(api=1, helper_api=6, goblins={})))
        control, pid = self.preflight(path)
        self.assertEqual(self.receive(control), dict(
            jsonrpc="2.0", method="PreflightFailed",
            params=dict(error="incompatible manifest/helper API")))
        self.assertEqual(self.reap(pid), 0)
        self.assertEqual(control.recv(1), b"")
        self.assertFalse((self.root / "manifests").exists())

    def test_preflight_rejects_incompatible_api_and_relative_root_directory(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps(dict(api=2, helper_api=6, goblins={})))
        for overrides in [dict(supervisor_api=2), dict(directory="relative")]:
            with self.subTest(overrides=overrides):
                control, pid = self.preflight(path, **overrides)
                self.assertNotEqual(self.reap(pid), 0)
                self.assertEqual(control.recv(1), b"")
                self.assertFalse((self.root / "manifests").exists())

    def test_preflight_rejects_wrong_adoption_and_already_queued_extra_input(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps(dict(api=2, helper_api=6, goblins={})))
        for mode in ["wrong", "duplicate", "partial"]:
            with self.subTest(mode=mode):
                control, pid = self.preflight(path)
                params = self.receive(control)["params"]
                transfer = params["transfer"]
                body = json.dumps(dict(jsonrpc="2.0", method="Adopted",
                                       params=dict(transfer=transfer + (mode == "wrong")))).encode()
                frame = (f"Content-Length: {len(body)}\r\nDescriptor-Count: 0\r\n\r\n".encode() + body)
                # One send queues the terminal ACK and suffix together.
                suffix = frame if mode == "duplicate" else b"C" if mode == "partial" else b""
                control.sendall(frame + suffix)
                self.assertNotEqual(self.reap(pid), 0)
                try:
                    self.assertEqual(control.recv(1), b"")
                except ConnectionResetError:
                    pass  # Closing unread trailing input is an intentional failure.

    def test_preflight_rejects_a_multipart_handoff_instead_of_adoption(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps(dict(api=2, helper_api=6, goblins={})))
        control, pid = self.preflight(path)
        self.assertEqual(self.receive(control)["method"], "Event.Manifest")
        resources = self.root / "unexpected-resources"
        resources.mkdir()
        lock = os.open(resources / ".gc-lease", os.O_CREAT | os.O_RDWR | os.O_CLOEXEC, 0o600)
        self.addCleanup(os.close, lock)
        fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
        read, write = os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close, read)
        handoff = dict(transfer=99, count=2, workspace={"$fd": 0}, working_directory=None,
                       dev_shell="null", docker=False,
                       leases=[dict(directory=str(resources), lock={"$fd": 1})], more=True)
        self.send(control, "Command.Start", dict(handoff=handoff), [write, lock])
        os.close(write)
        control.settimeout(3)
        self.assertEqual(control.recv(1), b"")
        # The rejected raw descriptors close before the control EOF is visible.
        self.assertTrue(select.select([read], [], [], 0)[0])
        self.assertEqual(os.read(read, 1), b"")
        self.assertNotEqual(self.reap(pid), 0)

    def maintenance(self, operation, output, error, overrides=None, suffix=b""):
        control, child = self.pair()
        pid = self.spawn(child)
        child.close()
        params = dict(supervisor_api=1, operation=operation,
                      stdout={"$fd": 0}, stderr={"$fd": 1})
        params.update(overrides or {})
        body = json.dumps(dict(jsonrpc="2.0", method="Maintenance", params=params)).encode()
        frame = f"Content-Length: {len(body)}\r\nDescriptor-Count: 2\r\n\r\n".encode() + body
        control.sendmsg([frame + suffix],
                        [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [output, error]))])
        return control, pid

    def test_maintenance_scope_dry_run_writes_explicit_output_without_deleting(self):
        selected = self.root / "cache/goblins/scopes/scope-work"
        selected.mkdir(mode=0o700, parents=True)
        for path in (selected.parent, selected.parent.parent):
            path.chmod(0o700)
        (selected / "payload").write_text("keep")
        with (self.root / "stdout").open("wb") as output, (self.root / "stderr").open("wb") as error:
            flags = fcntl.fcntl(output, fcntl.F_GETFL)
            control, pid = self.maintenance(
                dict(kind="ClearScope", runtime=None, name="work", dry_run=True),
                output.fileno(), error.fileno())
            self.assertEqual(self.receive(control),
                             dict(jsonrpc="2.0", method="MaintenanceFinished", params=dict(result={"Ok": None})))
            self.assertEqual(self.reap(pid), 0)
            self.assertEqual(fcntl.fcntl(output, fcntl.F_GETFL), flags)
        self.assertEqual((self.root / "stdout").read_text(), f"Would remove {selected}\n")
        self.assertEqual((self.root / "stderr").read_bytes(), b"")
        self.assertEqual((selected / "payload").read_text(), "keep")
        self.assertFalse((self.root / "terminal.sock").exists())

    def test_maintenance_operation_error_has_normal_exit_and_original_diagnostic(self):
        with (self.root / "stdout").open("wb") as output, (self.root / "stderr").open("wb") as error:
            control, pid = self.maintenance(
                dict(kind="ClearScope", runtime=None, name="../invalid", dry_run=False),
                output.fileno(), error.fileno())
            self.assertEqual(self.receive(control),
                             dict(jsonrpc="2.0", method="MaintenanceFinished",
                                  params=dict(result={"Err": "invalid scope name"})))
            self.assertEqual(self.reap(pid), 0)
        self.assertFalse((self.root / "cache").exists())

    def test_maintenance_rejects_invalid_api_mode_fields_and_readonly_output_before_effects(self):
        for mode in ["api", "operation", "extra", "readonly"]:
            with self.subTest(mode=mode):
                with (self.root / "stdout").open("w+b") as output, (self.root / "stderr").open("wb") as error:
                    readonly = os.open(self.root / "stdout", os.O_RDONLY | os.O_CLOEXEC)
                    try:
                        overrides = (dict(supervisor_api=2) if mode == "api" else
                                     dict(operation=dict(kind="Unknown")) if mode == "operation" else
                                     dict(extra=True) if mode == "extra" else {})
                        control, pid = self.maintenance(
                            dict(kind="Gc", runtime=None, state=str(self.root / "state"), dry_run=True),
                            readonly if mode == "readonly" else output.fileno(),
                            error.fileno(), overrides)
                        self.assertNotEqual(self.reap(pid), 0)
                    finally:
                        os.close(readonly)
                self.assertFalse((self.root / "cache").exists())
                self.assertFalse((self.root / "state").exists())

    def test_maintenance_queued_cancellation_precedes_cache_and_lock_mutations(self):
        with (self.root / "stdout").open("wb") as output, (self.root / "stderr").open("wb") as error:
            control, pid = self.maintenance(
                dict(kind="ClearScope", runtime=None, name=None, dry_run=True),
                output.fileno(), error.fileno(), suffix=b"C")
            self.assertNotEqual(self.reap(pid), 0)
        self.assertFalse((self.root / "cache").exists())
        self.assertEqual((self.root / "stdout").read_bytes(), b"")

    def test_maintenance_broken_output_is_an_operation_error_with_normal_protocol_exit(self):
        read, write = os.pipe2(os.O_CLOEXEC)
        os.close(read)
        try:
            with (self.root / "stderr").open("wb") as error:
                control, pid = self.maintenance(
                    dict(kind="ClearScope", runtime=None, name=None, dry_run=True),
                    write, error.fileno())
                reply = self.receive(control)
                self.assertEqual(reply["method"], "MaintenanceFinished")
                self.assertIn("Broken pipe", reply["params"]["result"]["Err"])
                self.assertEqual(self.reap(pid), 0)
        finally:
            os.close(write)
