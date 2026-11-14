"""Scopes, as a team of goblins uses them: shared network, processes and storage.

Run on the host as a regular user with newuidmap/newgidmap and subuid/subgid.
The opt-in slow tier (GOBLINS_TEST_SLOW=1, `just test-feature slow`) runs
real Gradle daemons in a shared home.
"""
import os
from pathlib import Path
import select
import shutil
import subprocess
import tempfile
import time
import unittest
import uuid
import zipfile

from build_support import fixture_app
from daemon_support import Daemon, ROOT, supervisor_children
from support import step
from terminal_support import Terminal

# What Gradle relies on in a shared home, without Gradle: the lock owner
# records its PID and a loopback port beside the lock, and releases the lock
# when another member asks over that port.
LOCK_OWNER = """
import fcntl, os, socket
home = os.path.expanduser("~/.gradle")
lock = open(os.path.join(home, "probe.lock"), "a+")
fcntl.flock(lock, fcntl.LOCK_EX)
server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
server.bind(("127.0.0.1", 0))
owner = os.path.join(home, "probe.owner")
with open(owner + ".tmp", "w") as f:
    f.write(f"{os.getpid()} {server.getsockname()[1]}\\n")
os.rename(owner + ".tmp", owner)
request, peer = server.recvfrom(64)
fcntl.flock(lock, fcntl.LOCK_UN)
server.sendto(b"released-for-" + request, peer)
"""
LOCK_CONTENDER = """
import fcntl, os, socket
home = os.path.expanduser("~/.gradle")
pid, port = open(os.path.join(home, "probe.owner")).read().split()
print("OWNER", open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\\0", b" ").decode())
lock = open(os.path.join(home, "probe.lock"), "a+")
try:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    print("NOT-HELD")
except BlockingIOError:
    print("HELD")
client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
client.settimeout(10)
client.sendto(b"contender", ("127.0.0.1", int(port)))
print(client.recv(64).decode())
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
print("ACQUIRED")
"""


class ScopeWorkflow(unittest.TestCase):
    """Helpers for driving goblins of the scope-goblins fixture through their terminals."""

    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("scope-goblins")

    def setUp(self):
        cache = tempfile.TemporaryDirectory(prefix="goblins-scope-cache-", dir=Path.home() / ".cache")
        self.addCleanup(cache.cleanup)
        self.cache = Path(cache.name)
        root = tempfile.TemporaryDirectory(prefix="goblins-scope-test-")
        self.addCleanup(root.cleanup)
        self.root = Path(root.name)
        (self.root / "projects").mkdir()
        (self.root / "repository").mkdir()
        self.d = Daemon(self.app, env=os.environ | {"XDG_CACHE_HOME": str(self.cache),
                                                     "GOBLINS_TEST_ROOT": str(self.root)})
        self.addCleanup(self.d.close)
        self.counter = 0

    def start(self, name="member", scope=None, cwd=None):
        params = {"key": uuid.uuid4().hex, "name": name, "configuration": self.d.manifest,
                  "rows": 24, "cols": 100}
        if cwd:
            params["cwd"] = str(cwd)
        if scope:
            params["scope"] = scope
        session = self.d.rpc.call("sessions.start", params)["session"]
        record = self.d.wait(lambda: (r if (r := self.d.get(session))["state"] != "starting" else None),
                             timeout=60)
        self.assertEqual(record["state"], "running", record)
        return session

    def terminal(self, session):
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect("scope-test>")
        return terminal

    def run_in(self, terminal, script, timeout=30):
        """Run a shell command and return its output and exit code."""
        self.counter += 1
        marker = f"END{self.counter}"
        if script.rstrip().endswith("&"):
            script += " true"
        # The terminal echoes the command line; the printf formats keep these
        # markers out of the echo, so only the command's own output is captured.
        terminal.send(f"printf 'BEGIN%s\\n' {self.counter}; {script}; printf '\\n{marker}=%s\\n' \"$?\"\n")
        match = terminal.expect(rf"BEGIN{self.counter}\n([\s\S]*?)\n{marker}=(\d+)\n", timeout=timeout)
        terminal.expect("scope-test>")
        return match.group(1).decode(), int(match.group(2))

    def ok(self, terminal, script, timeout=30):
        output, code = self.run_in(terminal, script, timeout)
        self.assertEqual(code, 0, f"{script}\n{output}")
        return output

    def stop(self, session):
        self.d.rpc.call("sessions.stop", {"session": session})
        self.d.wait(lambda: self.d.get(session)["state"] not in ("starting", "running", "stopping"))

    # Find processes by command line through this member's /proc.
    FIND = ("python3 -c 'import os,sys; n=\"\\0\".join(sys.argv[1:]).encode(); "
            "print(*[p for p in os.listdir(\"/proc\") if p.isdigit() and p != str(os.getpid()) "
            "and n in open(f\"/proc/{p}/cmdline\",\"rb\").read()])' ")

    def fetch(self, port):
        return (f"for i in $(seq 50); do python3 -c 'import urllib.request as u; "
                f"print(u.urlopen(\"http://127.0.0.1:{port}/\").status)' 2>/dev/null && break; sleep .1; done")


class ScopeTeamWorkflow(ScopeWorkflow):
    """Goblins in one scope work together; other scopes, and goblins in none, keep apart."""

    def test_a_team_in_one_scope(self):
        exec_ = [str(self.app), "--state-dir", str(self.d.state), "exec"]

        with step("two goblins in one scope see each other's processes, not their goblins' internals"):
            a, b = self.start(), self.start()
            ta, tb = self.terminal(a), self.terminal(b)
            # A detached daemon in its own session, and an exec job that left
            # the exec's process group and ignores hangups.
            self.ok(ta, "setsid -f sleep 2001 </dev/null >/dev/null 2>&1")
            background = subprocess.Popen(
                [*exec_, a, "--", "sh", "-c", "trap '' HUP TERM; setsid sleep 2002 & sleep 2003; wait"],
                stderr=subprocess.DEVNULL)
            # Bounded, so a failure before A exits cannot wait for the sleeps.
            self.addCleanup(lambda: background.poll() is None and (background.kill(), background.wait(timeout=10)))
            find = [*exec_, b, "--", "sh", "-c", self.FIND + "sleep 200"]
            self.d.wait(lambda: len(subprocess.run(find, capture_output=True, text=True).stdout.split()) == 3)
            # goblins-init is an ordinary member PID here. Another member sees
            # it but neither its descriptors nor any socket in the scope.
            script = (r'for d in /proc/[0-9]*; do '
                      r'[ "$(head -c 29 $d/cmdline 2>/dev/null)" = /run/goblins/bin/goblins-init ] && '
                      r'{ ls $d/fd >/dev/null 2>&1 && echo INIT_FD_READABLE || echo hidden; }; '
                      r'for f in $d/fd/*; do case "$(readlink $f 2>/dev/null)" in socket:*) echo SOCKET $f;; esac; done; '
                      r'done')
            output = subprocess.run([*exec_, b, "--", "sh", "-c", script], capture_output=True,
                                    text=True, timeout=30).stdout
            # Both inits and the keepers of their execs: none is dumpable.
            self.assertGreaterEqual(len(output.split()), 3, output)
            self.assertEqual(set(output.split()), {"hidden"}, output)

        with step("they get the scope's defaults and share a loopback server and storage"):
            output = self.ok(ta, "echo $SCOPE_MARKER $OVERRIDDEN")
            self.assertEqual(output.split()[-2:], ["from-scope", "from-goblin"])
            self.ok(ta, "python3 -m http.server 18555 --bind 127.0.0.1 >/dev/null 2>&1 &")
            self.ok(ta, "echo shared > /var/cache/probe/file")
            self.assertIn("200", self.ok(tb, self.fetch(18555)))
            self.assertIn("shared", self.ok(tb, "cat /var/cache/probe/file"))

        with step("tools in a shared home coordinate through a lock and a loopback socket, as Gradle does"):
            # The 'builds' scope shares only the Gradle home, at its default path.
            for name, text in (("owner.py", LOCK_OWNER), ("contender.py", LOCK_CONTENDER)):
                (self.root / "projects" / name).write_text(text)
            first, second = self.start("builder"), self.start("builder")
            t1, t2 = self.terminal(first), self.terminal(second)
            self.ok(t1, f"python3 {self.root}/projects/owner.py & "
                        "for i in $(seq 100); do test -e ~/.gradle/probe.owner && break; sleep .1; done; "
                        "test -e ~/.gradle/probe.owner")
            output = self.ok(t2, f"python3 {self.root}/projects/contender.py")
            # The owner's PID means the same process to the other member.
            self.assertIn(f"OWNER python3 {self.root}/projects/owner.py", output)
            self.assertIn("HELD", output.split())
            self.assertIn("released-for-contender", output)
            self.assertIn("ACQUIRED", output)
            # Tools' daemon registries in /tmp stay private to each member.
            self.ok(t1, "touch /tmp/registry-first")
            self.ok(t2, "! test -e /tmp/registry-first")

        with step("a goblin that leaves takes its processes with it, and its exit code is recorded"):
            ta.send("exit 6\n")
            self.assertEqual(ta.wait(), 6)
            self.d.wait(lambda: subprocess.run(find, capture_output=True, text=True).stdout.split() == [])
            self.assertEqual(self.d.get(a)["exit_code"], 6)
            self.assertEqual(self.d.get(b)["state"], "running")
            self.assertNotEqual(background.wait(timeout=10), 0)
            self.assertIn("shared", self.ok(tb, "cat /var/cache/probe/file"))

        with step("in a scope without a shared PID namespace goblins share only the network"):
            p, q = self.start(scope="private"), self.start(scope="private")
            tp, tq = self.terminal(p), self.terminal(q)
            self.ok(tp, "sleep 3001 &")
            self.ok(tp, "python3 -m http.server 18556 --bind 127.0.0.1 >/dev/null 2>&1 &")
            self.assertNotRegex(self.ok(tq, self.FIND + "sleep 3001"), r"\d")
            self.assertIn("200", self.ok(tq, self.fetch(18556)))
            _, code = self.run_in(tq, "ls /var/cache/probe")
            self.assertNotEqual(code, 0)

        with step("an unscoped goblin has no scope; a member joins its owner's scope and storage"):
            # No default scope: the loner has nothing of the scope's.
            self.assertIn("none", self.ok(self.terminal(self.start("loner")), "echo ${SCOPE_MARKER:-none}"))
            owner = self.start(scope="scratch")
            member = self.d.member(owner, "member")["session"]
            self.assertEqual(self.d.get(member)["scope"], "scratch")
            self.ok(self.terminal(owner), "echo inherited > /var/cache/probe/file")
            self.assertIn("inherited", self.ok(self.terminal(member), "cat /var/cache/probe/file"))

        with step("when everyone has left, temporary scope data is gone and persistent data kept"):
            for session in (member, owner, b):
                self.stop(session)
            _, code = self.run_in(self.terminal(self.start(scope="scratch")), "cat /var/cache/probe/file")
            self.assertNotEqual(code, 0)
            self.assertIn("shared", self.ok(self.terminal(self.start()), "cat /var/cache/probe/file"))

        with step("stopping the server ends every scope and removes its temporary storage"):
            scopes, members = supervisor_children(self.d.process.pid, self.d.state)
            self.assertEqual(set(scopes), {"work", "scratch", "builds", "private"})
            descriptors = [os.pidfd_open(pid) for pid in set(scopes.values()) | members]
            for fd in descriptors:
                self.addCleanup(os.close, fd)
            self.d.process.terminate()
            self.assertEqual(self.d.process.wait(timeout=10), 0)
            self.assertEqual(set(select.select(descriptors, [], [], 0)[0]), set(descriptors))
            self.assertFalse(list((self.cache / "goblins/scopes").glob("temporary-*")))


@unittest.skipUnless(os.environ.get("GOBLINS_TEST_SLOW"),
                     "slow tier: set GOBLINS_TEST_SLOW=1 or run `just test-feature slow`")
class ScopeSlowTier(ScopeWorkflow):
    """Real Gradle daemons. What they rely on is also checked, faster, in
    ScopeTeamWorkflow."""

    def test_gradle_members_share_a_home_without_lock_timeouts(self):
        # Separately sandboxed Gradle daemons coordinate cache locks over the
        # shared loopback. Unshared network namespaces time out after 60s here.
        repo = self.root / "repository/example/sample/1.0"
        repo.mkdir(parents=True)
        (repo / "sample-1.0.pom").write_text(
            "<project><modelVersion>4.0.0</modelVersion><groupId>example</groupId>"
            "<artifactId>sample</artifactId><version>1.0</version></project>")
        with zipfile.ZipFile(repo / "sample-1.0.jar", "w") as jar:
            jar.writestr("marker.txt", "fixture\n")
        projects = {}
        for name in "ab":
            project = projects[name] = self.root / "projects" / name
            project.mkdir(parents=True)
            (project / "settings.gradle").write_text(f"rootProject.name = 'probe-{name}'\n")
            shutil.copyfile(ROOT / "prototypes/shared-namespaces/build.gradle", project / "build.gradle")
        a = self.start("builder", cwd=projects["a"])
        b = self.start("builder", cwd=projects["b"])
        ta, tb = self.terminal(a), self.terminal(b)
        # Each member resolves from its own repository URL, so each first
        # build writes new metadata into the shared dependency cache.
        for port in (18091, 18092):
            self.run_in(ta, f"python3 -m http.server {port} --bind 127.0.0.1 "
                            f"--directory {self.root}/repository >/dev/null 2>&1 &")
        gradle = "gradle --daemon --console=plain -PrepoPort={} resolve 2>&1"

        def build(terminal, port):
            started = time.monotonic()
            output, code = self.run_in(terminal, gradle.format(port), timeout=240)
            self.assertEqual(code, 0, output)
            self.assertNotIn("Timeout waiting to lock", output)
            return time.monotonic() - started, output

        first, _ = build(ta, 18091)
        second, output = build(tb, 18092)
        self.assertLess(second, 45, output)
        pid = next(line.split()[1] for line in output.splitlines() if line.startswith("DAEMON_PID"))
        third, _ = build(ta, 18091)
        self.assertLess(third, 45)
        print(f"EVIDENCE Gradle shared home: A {first:.1f}s, B while A idle {second:.1f}s, "
              f"A while B idle {third:.1f}s", flush=True)
        # Daemons stay per member (private registries), but PIDs are
        # meaningful across the scope: A sees B's daemon.
        output, _ = self.run_in(ta, f"tr '\\0' ' ' < /proc/{pid}/cmdline")
        self.assertIn("GradleDaemon", output)
        # The scope's storage path puts the shared home at Gradle's default.
        self.assertTrue(list((self.cache / "goblins/scopes").glob("temporary-*/gradle/caches")))


if __name__ == "__main__":
    unittest.main(verbosity=2)
