"""The isolation a user relies on, audited by one probe an agent could run
with `goblins exec`: what the goblin cannot see, reach or change. Kernel
backstops that are not user steps (a bind source swapped after its check, a
trusted placeholder replaced by a link, the mount helper's capabilities) are
Rust host tests."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest

from build_support import fixture_app, fixture_path
from daemon_support import Daemon
from support import FLAKE, command, step

PROBE = r'''
import ctypes, json, os, socket, sys
args = json.loads(sys.argv[1])
libc = ctypes.CDLL(None, use_errno=True)
def refused(call):
    result = call()
    return [result, ctypes.get_errno()]
results = {"hidden": {p: os.path.lexists(p) for p in args["hidden"]}}
results["caps"] = {l.split(":")[0]: int(l.split()[1], 16) for l in open("/proc/self/status") if l.startswith("Cap")}
try:
    os.close(os.open("/dev/tty", os.O_RDWR)); results["terminal"] = True
except OSError:
    results["terminal"] = False
results["mount"] = refused(lambda: libc.mount(b"none", b"/tmp", b"tmpfs", 0, None))
results["remount"] = refused(lambda: libc.mount(None, b"/nix/store", None, 4096 | 32, None))
results["unshare"] = refused(lambda: libc.unshare(0x10000000))
ns = os.open("/proc/self/ns/mnt", os.O_RDONLY)
results["setns"] = refused(lambda: libc.setns(ns, 0x20000))
os.close(ns)
results["writes"] = {}
for path in ["/nix/store/evil", "/run/goblins/packages/evil", "/nix/evil", "/run/goblins/request.sock"]:
    try:
        os.symlink("/tmp", path); results["writes"][path] = True
    except OSError:
        results["writes"][path] = False
results["connects"] = {}
for path in args["sockets"]:
    try:
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(path)
        results["connects"][path] = True
    except OSError:
        results["connects"][path] = False
try:
    with socket.create_connection(("127.0.0.1", args["port"]), timeout=.5):
        results["host_tcp"] = True
except OSError:
    results["host_tcp"] = False
results["fds"], results["init_fds_hidden"] = {}, 0
for pid in filter(str.isdigit, os.listdir("/proc")):
    try:
        if open(f"/proc/{pid}/cmdline", "rb").read().startswith(b"/run/goblins/bin/goblins-init\0"):
            try:
                os.listdir(f"/proc/{pid}/fd"); results["init_fds_hidden"] = -1000
            except PermissionError:
                results["init_fds_hidden"] += 1
            continue
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                results["fds"][f"{pid}/{fd}"] = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                pass
    except OSError:
        pass
results["mounts"] = {f[4]: f[5].split(",") for f in map(str.split, open("/proc/self/mountinfo"))
                     if f[4].startswith("/nix/store/")}
results["run"] = sorted(os.listdir("/run/goblins"))
print(json.dumps(results))
'''


class IsolationAuditWorkflow(unittest.TestCase):
    def test_an_isolation_audit(self):
        d = Daemon(fixture_app("audit-goblins"))
        self.addCleanup(d.close)
        temporary = tempfile.TemporaryDirectory(prefix="goblins-audit-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        # Its startup closure is listed by an output that also references jq,
        # without listing it: GC retention must not widen what is mounted.
        reference = fixture_path(FLAKE + "#checks.x86_64-linux.root-reference-fixture")
        unlisted = (reference / "unlisted").read_text().strip()
        configuration = json.loads(Path(d.manifest).read_text())
        goblin = configuration["goblins"]["shell"]
        spec = json.loads(Path(goblin["build_spec"]).read_text())
        spec["closure_paths_file"] = str(reference / "paths")
        (root / "spec.json").write_text(json.dumps(spec))
        goblin["build_spec"] = str(root / "spec.json")
        (root / "manifest.json").write_text(json.dumps(configuration))
        probe = d.start(manifest=root / "manifest.json", agent_name="probe")["session"]
        other = d.start(agent_name="other")["session"]

        secret = root / "host-secret"
        secret.write_text("not granted")
        for address in (str(root / "host.sock"), "\0goblins-audit-" + str(os.getpid())):
            listener = socket.socket(socket.AF_UNIX)
            self.addCleanup(listener.close)
            listener.bind(address)
            listener.listen()
        tcp = socket.socket()
        self.addCleanup(tcp.close)
        tcp.bind(("127.0.0.1", 0))
        tcp.listen()
        other_socket = str(d.state / other / "resources/request.sock")
        nix_socket = "/nix/var/nix/daemon-socket/socket"
        arguments = {
            "hidden": [str(secret), str(root / "host.sock"), str(d.state / "host.sock"), other_socket, nix_socket,
                       "/etc/shadow", unlisted, str(reference), str(d.state / probe), f"/proc/{d.process.pid}/root"],
            "sockets": [str(root / "host.sock"), str(d.state / "host.sock"), other_socket, nix_socket,
                        "\0goblins-audit-" + str(os.getpid())],
            "port": tcp.getsockname()[1],
        }
        result = subprocess.run([str(d.app), "--state-dir", str(d.state), "exec", "probe", "--",
                                 "python3", "-c", PROBE, json.dumps(arguments)],
                                capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        audit = json.loads(result.stdout)

        with step("host files, sockets and other goblins are hidden"):
            self.assertEqual([p for p, seen in audit["hidden"].items() if seen], [])

        with step("unlisted store references that keep the goblin's closure alive are not mounted"):
            roots = list((d.state / probe / "resources/roots").iterdir())
            self.assertIn(reference, [p.resolve() for p in roots])
            retained = set(command(["nix-store", "--query", "--requisites", *roots]).splitlines())
            self.assertTrue(set((reference / "paths").read_text().split()) <= retained)

        with step("nothing on the host is reachable"):
            self.assertEqual([p for p, reached in audit["connects"].items() if reached], [])
            self.assertFalse(audit["host_tcp"])
            self.assertFalse(audit["terminal"])

        with step("the goblin has no capabilities and cannot mount or change namespaces"):
            self.assertEqual({cap: value for cap, value in audit["caps"].items() if value}, {})
            for operation in ("mount", "remount", "unshare", "setns"):
                self.assertEqual(audit[operation], [-1, 1], operation)  # EPERM

        with step("the store and the goblin's runtime directory cannot be changed"):
            self.assertEqual([p for p, written in audit["writes"].items() if written], [])
            self.assertTrue(audit["mounts"])
            for path, options in audit["mounts"].items():
                self.assertTrue({"ro", "nosuid", "nodev"} <= set(options), (path, options))
            # Only the request socket is a file; init's channel has no name.
            self.assertEqual(audit["run"], ["bin", "packages", "request.sock"])

        with step("no descriptor leads out of the sandbox, and init's are hidden"):
            for fd, target in audit["fds"].items():
                self.assertFalse(target.startswith(("socket:", "mnt:[", "user:[", str(d.state), "/nix/store")),
                                 (fd, target))
            # Init and the keeper of this exec are forks of init: not dumpable.
            self.assertGreaterEqual(audit["init_fds_hidden"], 2)

        with step("host-only commands are not in the goblin's CLI"):
            tui = subprocess.run([str(d.app), "--state-dir", str(d.state), "exec", "probe", "--", "goblin", "tui"],
                                 capture_output=True, text=True, timeout=60)
            self.assertEqual(tui.returncode, 2, tui.stderr)


if __name__ == "__main__":
    unittest.main()
