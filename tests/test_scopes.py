"""Scopes: shared network/PID namespaces and managed storage between root goblins.

Run on the host as a regular user with newuidmap/newgidmap and subuid/subgid.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest
import uuid
import zipfile

from build_support import fixture_app
from daemon_support import Daemon, ROOT
from terminal_support import Terminal


class ScopeTests(unittest.TestCase):
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
        self.d = Daemon(self.app, env=os.environ | {"XDG_CACHE_HOME": str(self.cache),
                                                     "GOBLINS_TEST_ROOT": str(self.root)})
        self.addCleanup(self.d.close)
        self.counter = 0

    def start(self, name="member", scope=None, wait=True, cwd=None):
        params = {"key": uuid.uuid4().hex, "name": name, "configuration": self.d.manifest,
                  "rows": 24, "cols": 100}
        if cwd:
            params["cwd"] = str(cwd)
        if scope:
            params["scope"] = scope
        session = self.d.rpc.call("sessions.start", params)["session"]
        if wait:
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
        """Run a shell command and return its output."""
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

    def stop(self, session):
        self.d.rpc.call("sessions.stop", {"session": session})
        self.d.wait(lambda: self.d.get(session)["state"] not in ("starting", "running", "stopping"))

    # Find processes by command line through this member's /proc.
    FIND = ("python3 -c 'import os,sys; n=\"\\0\".join(sys.argv[1:]).encode(); "
            "print(*[p for p in os.listdir(\"/proc\") if p.isdigit() and p != str(os.getpid()) "
            "and n in open(f\"/proc/{p}/cmdline\",\"rb\").read()])' ")

    def test_members_share_network_processes_and_storage(self):
        a, b = self.start(), self.start()
        ta, tb = self.terminal(a), self.terminal(b)
        output, code = self.run_in(ta, "echo $SCOPE_MARKER $OVERRIDDEN")
        self.assertEqual(output.split()[-2:], ["from-scope", "from-goblin"])
        self.run_in(ta, "python3 -m http.server 18555 --bind 127.0.0.1 >/dev/null 2>&1 &")
        self.run_in(ta, "sleep 1000 &")
        self.run_in(ta, "echo shared > /var/cache/probe/file")
        output, code = self.run_in(tb, "for i in $(seq 50); do python3 -c 'import urllib.request as u; "
                                       "print(u.urlopen(\"http://127.0.0.1:18555/\").status)' 2>/dev/null && break; "
                                       "sleep .1; done")
        self.assertIn("200", output)
        output, _ = self.run_in(tb, self.FIND + "sleep 1000")
        self.assertRegex(output, r"\d+")
        output, _ = self.run_in(tb, "cat /var/cache/probe/file")
        self.assertIn("shared", output)
        self.assertEqual((self.cache / "goblins/scopes/scope-work/cache/file").read_text(), "shared\n")

    def test_departed_member_daemons_are_stopped(self):
        a, b = self.start(), self.start()
        ta, tb = self.terminal(a), self.terminal(b)
        # A detached daemon in its own session survives its shell, not its goblin.
        self.run_in(ta, "setsid -f sleep 1001 </dev/null >/dev/null 2>&1")
        output, _ = self.run_in(tb, self.FIND + "sleep 1001")
        self.assertRegex(output, r"\d+")
        self.stop(a)
        output, _ = self.run_in(tb, "for i in $(seq 50); do out=$(" + self.FIND + "sleep 1001); "
                                    "[ -z \"$out\" ] && echo GONE && break; sleep .1; done")
        self.assertIn("GONE", output)

    def test_exec_teardown_and_isolation_in_a_shared_pid_namespace(self):
        a, b = self.start(), self.start()
        ta = self.terminal(a)
        exec_ = [str(self.app), "--state-dir", str(self.d.state), "exec"]
        # goblins-init is an ordinary member PID here, a subreaper rather
        # than the namespace's init. Its execs, even one that left the exec's
        # process group, still end with its goblin.
        background = subprocess.Popen(
            [*exec_, a, "--", "sh", "-c", "trap '' HUP TERM; setsid sleep 1002 & sleep 1003; wait"],
            stderr=subprocess.DEVNULL)
        self.addCleanup(background.wait)
        find = [*exec_, b, "--", "sh", "-c", self.FIND + "sleep 100"]
        self.d.wait(lambda: len(subprocess.run(find, capture_output=True, text=True).stdout.split()) == 2)
        # Another member sees the processes but neither init's descriptors nor
        # any socket in the scope.
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
        ta.send("exit 6\n")
        self.assertEqual(ta.wait(), 6)
        self.d.wait(lambda: subprocess.run(find, capture_output=True, text=True).stdout.split() == [])
        self.assertEqual(self.d.get(a)["exit_code"], 6)
        self.assertEqual(self.d.get(b)["state"], "running")
        self.assertNotEqual(background.wait(timeout=10), 0)

    def test_temporary_scope_storage_is_deleted_and_persistent_kept(self):
        scratch = self.start(scope="scratch")
        self.run_in(self.terminal(scratch), "echo temporary > /var/cache/probe/file")
        self.assertEqual(len(list((self.cache / "goblins/scopes").glob("temporary-*"))), 1)
        self.stop(scratch)
        self.d.wait(lambda: not list((self.cache / "goblins/scopes").glob("temporary-*")))
        again = self.start(scope="scratch")
        output, code = self.run_in(self.terminal(again), "cat /var/cache/probe/file")
        self.assertNotEqual(code, 0)

        work = self.start()
        self.run_in(self.terminal(work), "echo kept > /var/cache/probe/file")
        self.stop(work)
        work = self.start()
        output, _ = self.run_in(self.terminal(work), "cat /var/cache/probe/file")
        self.assertIn("kept", output)

    def test_unshared_pid_namespace_hides_processes_but_shares_network(self):
        a, b = self.start(scope="private"), self.start(scope="private")
        ta, tb = self.terminal(a), self.terminal(b)
        self.run_in(ta, "sleep 1002 &")
        self.run_in(ta, "python3 -m http.server 18556 --bind 127.0.0.1 >/dev/null 2>&1 &")
        output, _ = self.run_in(tb, self.FIND + "sleep 1002")
        self.assertNotRegex(output, r"\d")
        output, _ = self.run_in(tb, "for i in $(seq 50); do python3 -c 'import urllib.request as u; "
                                    "print(u.urlopen(\"http://127.0.0.1:18556/\").status)' 2>/dev/null && break; "
                                    "sleep .1; done")
        self.assertIn("200", output)
        output, code = self.run_in(tb, "ls /var/cache/probe")
        self.assertNotEqual(code, 0)

    def test_scope_selection_rules(self):
        # No default scope: an unscoped goblin has its own network namespace.
        loner = self.start("loner")
        output, _ = self.run_in(self.terminal(loner), "echo ${SCOPE_MARKER:-none}")
        self.assertIn("none", output)
        failed = self.start("loner", scope="scratch", wait=False)
        record = self.d.wait(lambda: (r if (r := self.d.get(failed))["state"] not in ("starting", "running") else None),
                             timeout=60)
        self.assertIn("not allowed", record["detail"] or "")
        # A goblin a raid owner runs joins the owner's scope, so an unscoped
        # owner cannot run a configuration that requires one.
        failed = self.d.member(loner, "member", wait=False)["session"]
        record = self.d.wait(lambda: (r if (r := self.d.get(failed))["state"] not in ("starting", "running") else None),
                             timeout=60)
        self.assertIn("its owner has no scope", record["detail"] or "")
        owner = self.start(scope="scratch")
        member = self.d.member(owner, "member")["session"]
        self.assertEqual(self.d.get(member)["scope"], "scratch")
        self.run_in(self.terminal(owner), "echo inherited > /var/cache/probe/file")
        output, _ = self.run_in(self.terminal(member), "cat /var/cache/probe/file")
        self.assertIn("inherited", output)

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
    unittest.main()
