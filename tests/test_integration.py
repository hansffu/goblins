"""Host-run integration suite; every payload and child executes in Bubblewrap."""
import contextlib
import json
import os
from pathlib import Path
import selectors
import shlex
import socket
import subprocess
import tempfile
import time
import unittest
from support import Session, command, receive, request


def line(session, timeout=15):
    with selectors.DefaultSelector() as sel:
        sel.register(session.output, selectors.EVENT_READ)
        if not sel.select(timeout):
            raise AssertionError("sandbox output timed out")
    return session.output.readline().strip()


def probe(session, code):
    session.input.write(str(session.python) + " -c " + shlex.quote(code) + "\n")
    data = line(session)
    try:
        return json.loads(data)
    except ValueError:
        raise AssertionError("unexpected sandbox output: " + data)


def identity(session):
    return probe(session, '''import os,json
p=os.getppid()
print(json.dumps([p,open(f"/proc/{p}/stat").read().split()[21],os.readlink(f"/proc/{p}/ns/mnt")]))''')


class LiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = tempfile.TemporaryDirectory(prefix="goblins-test-")
        cls.paths = {}
        for name in ("runtime", "jq", "script-tool", "data-tool", "collision"):
            cls.paths[name] = Path(command(["nix", "build", "--no-write-lock-file", "--print-out-paths",
                "--out-link", str(Path(cls.fixture.name) / name), "path:" + str(Path(__file__).resolve().parents[1]) + "#" + ("jq^bin" if name == "jq" else name)]))
        cls.config = json.loads(cls.paths["runtime"].read_text())

    @classmethod
    def tearDownClass(cls):
        cls.fixture.cleanup()

    def session(self):
        session = Session(**self.config)
        self.addCleanup(session.close)
        return session.start()

    def test_live_closures_identity_and_isolation(self):
        one, two = self.session(), self.session()
        before = identity(one)
        jq = self.paths["jq"]
        self.assertEqual(probe(one, f'import os,shutil,json; print(json.dumps([shutil.which("jq"),os.path.exists("{jq}/bin/jq")]))'), [None, False])
        missing = one.closure([jq]) - one.mounted
        self.assertGreater(len(missing), 1, "dynamic dependency must be newly exposed")
        self.assertFalse(probe(one, f'import os,json; print(json.dumps(any(os.path.exists(p) for p in {list(map(str,missing))!r})))'))
        for name in ("jq", "script-tool", "data-tool"):
            one.grant(name, self.paths[name])
        one.input.write("/bin/sh -c \"jq -nc '{live:true}'\"; script-tool; data-tool\n")
        self.assertEqual(line(one), '{"live":true}')
        # TextIO buffering can hold subsequent lines; direct readline is safe here.
        self.assertEqual(one.output.readline().strip(), "script-interpreter-ok")
        self.assertEqual(one.output.readline().strip(), "runtime-data-ok")
        self.assertEqual(identity(one), before)
        self.assertFalse(probe(two, f'import os,json; print(json.dumps(os.path.exists("{jq}/bin/jq")))'))
        flags = probe(one, '''import json
print(json.dumps({p[4]:p[5].split(',') for l in open('/proc/self/mountinfo') if (p:=l.split())[4].startswith('/nix/store/')}))''')
        for path in one.mounted:
            self.assertIn("ro", flags[str(path)])
            self.assertIn("nosuid", flags[str(path)])
            self.assertIn("nodev", flags[str(path)])
        self.assertTrue(probe(one, f'import os,json; print(json.dumps(all(os.path.exists(p) for p in {list(map(str,one.mounted))!r})))'))
        print("EVIDENCE same-shell", before, identity(one), "new-jq-paths", len(missing), "namespaces", one.identity, flush=True)

    def test_authority_files_fds_and_sockets(self):
        one, two = self.session(), self.session()
        secret = Path(self.fixture.name) / "host-secret"
        secret.write_text("not granted")
        host_socket = socket.socket(socket.AF_UNIX)
        self.addCleanup(host_socket.close)
        socket_path = Path(self.fixture.name) / "host.sock"
        host_socket.bind(str(socket_path)); host_socket.listen()
        abstract = socket.socket(socket.AF_UNIX)
        self.addCleanup(abstract.close)
        abstract_name = "\0goblins-negative-" + str(os.getpid())
        abstract.bind(abstract_name); abstract.listen()
        tcp = socket.socket()
        self.addCleanup(tcp.close)
        tcp.bind(("127.0.0.1", 0)); tcp.listen()
        tcp_port = tcp.getsockname()[1]
        hidden = [str(secret), str(socket_path), str(two.directory / "request.sock"),
                  "/nix/var/nix/daemon-socket/socket", "/etc/shadow", str(self.paths["collision"]),
                  str(one.directory), f"/proc/{one.proc.pid}/root"]
        result = probe(one, f'''import os,json,socket,ctypes,errno
libc=ctypes.CDLL(None,use_errno=True)
results={{}}
results['hidden']=[not os.path.exists(p) for p in {hidden!r}]
results['caps']=[l.strip() for l in open('/proc/self/status') if l.startswith(('Cap','NoNewPrivs'))]
try:
 t=os.open('/dev/tty',os.O_RDWR); os.close(t); results['no_terminal']=False
except OSError: results['no_terminal']=True
results['mount']=libc.mount(b'none',b'/tmp',b'tmpfs',0,None); results['mount_errno']=ctypes.get_errno()
results['remount']=libc.mount(None,b'/nix/store',None,4096|32,None); results['remount_errno']=ctypes.get_errno()
results['unshare']=libc.unshare(0x10000000); results['unshare_errno']=ctypes.get_errno()
f=os.open('/proc/self/ns/mnt',os.O_RDONLY)
results['setns']=libc.setns(f,0x20000); results['setns_errno']=ctypes.get_errno(); os.close(f)
results['writes']=[]
for p in ['/nix/store/evil','/run/goblins/packages/evil','/nix/evil','/run/goblins/request.sock']:
 try: os.symlink('/tmp',p); results['writes'].append(False)
 except OSError: results['writes'].append(True)
results['sockets']=[]
for p in {[str(socket_path),str(two.directory / 'request.sock'),'/nix/var/nix/daemon-socket/socket',abstract_name]!r}:
 try:
  with socket.socket(socket.AF_UNIX) as s: s.connect(p)
  results['sockets'].append(False)
 except OSError: results['sockets'].append(True)
try:
 with socket.create_connection(('127.0.0.1',{tcp_port}),timeout=.5): pass
 results['host_tcp']=False
except OSError: results['host_tcp']=True
results['fds']={{}}
for proc in os.listdir('/proc'):
 if proc.isdigit():
  try:
   for f in os.listdir('/proc/'+proc+'/fd'):
    try: results['fds'][proc+'/'+f]=os.readlink('/proc/'+proc+'/fd/'+f)
    except OSError: pass
  except OSError: pass
print(json.dumps(results))''')
        self.assertTrue(all(result["hidden"]))
        self.assertTrue(all(result["writes"]))
        self.assertTrue(all(result["sockets"]))
        self.assertTrue(result["host_tcp"])
        self.assertTrue(result["no_terminal"])
        for op in ("mount", "remount", "unshare", "setns"):
            self.assertEqual((result[op], result[op + "_errno"]), (-1, 1))
        for cap in result["caps"]:
            if cap.startswith("Cap"):
                self.assertEqual(int(cap.split()[1], 16), 0)
        for dest in result["fds"].values():
            self.assertFalse(dest.startswith(("mnt:[", "user:[", str(one.directory), "/nix/store")), dest)
        helper_caps = [l.strip() for l in Path(f"/proc/{one.proc.pid}/status").read_text().splitlines() if l.startswith("CapEff")]
        self.assertEqual(int(helper_caps[0].split()[1], 16), (1 << 21) | (1 << 18))
        print("EVIDENCE negative", json.dumps(result), "helper", helper_caps, flush=True)

    def test_socket_cli_denial_approval_malformed_disconnect(self):
        session = self.session()
        for approved in (False, True):
            session.input.write('goblins-request package jq --reason "integration"; echo "CLIENT-EXIT:$?"\n')
            conn, _ = session.socket.accept()
            with conn:
                req = receive(conn)
                reply = session.decide(req, approved)
                conn.sendall((json.dumps(reply) + "\n").encode())
            self.assertEqual(json.loads(line(session))["status"], "ready" if approved else "denied")
            self.assertEqual(session.output.readline().strip(), "CLIENT-EXIT:" + ("0" if approved else "1"))
        # Malformed requests never reach decide. A truncated peer cannot approve.
        session.input.write(str(session.python) + " -c " + shlex.quote("import socket; s=socket.socket(socket.AF_UNIX); s.connect('/run/goblins/request.sock'); s.sendall(b'{bad}\\n'); s.close()") + "\n")
        conn, _ = session.socket.accept()
        with conn, self.assertRaises(ValueError):
            receive(conn)
        session.input.write('goblins-request package data-tool --reason "disconnect"; echo "CLIENT-EXIT:$?"\n')
        conn, _ = session.socket.accept()
        with conn:
            receive(conn)
        self.assertIn("outcome unknown", line(session))
        self.assertEqual(session.output.readline().strip(), "CLIENT-EXIT:2")
        self.assertNotIn("data-tool", session.packages)

    def test_partial_failure_collision_failed_realization_and_cleanup(self):
        session = self.session()
        session.grant("jq", self.paths["jq"])
        current = os.readlink(session.directory / "packages/current")
        with self.assertRaisesRegex(ValueError, "collision"):
            session.grant("script-tool", self.paths["collision"])
        self.assertEqual(os.readlink(session.directory / "packages/current"), current)
        session.flake = "path:/nonexistent-goblins-catalog"
        self.assertEqual(session.decide(request(package="data-tool"), True)["status"], "error")
        self.assertIsNone(session.proc.poll())
        # Fresh session: fail after one actual mount, before publishing profile.
        partial = self.session()
        with self.assertRaisesRegex(RuntimeError, "partial"):
            partial.grant("jq", self.paths["jq"], fail_after=1)
        self.assertIsNotNone(partial.proc.poll())
        self.assertFalse((partial.directory / "packages/current").exists())
        rooted = session.directory / "roots" / self.paths["jq"].name
        self.assertTrue(rooted.is_symlink())
        roots = command(["nix-store", "--query", "--roots", self.paths["jq"]])
        self.assertIn(str(rooted), roots)
        saved = session.directory
        session.close()
        self.assertFalse(saved.exists())
        # Remove cleanup registration's second close by making close idempotent.

    def test_workspace_snapshot_and_destination_race(self):
        source = Path(self.fixture.name) / "workspace-source"
        source.mkdir()
        (source / "file").write_text("snapshot")
        (source / ".git").write_text("gitdir: /host/shared/git")
        (source / "escape").symlink_to("/etc/shadow")
        os.mkfifo(source / "fifo")
        sock = socket.socket(socket.AF_UNIX)
        self.addCleanup(sock.close)
        sock.bind(str(source / "host.sock")); sock.listen()
        session = Session(**self.config, workspace=source)
        self.addCleanup(session.close)
        session.start()
        self.assertEqual(probe(session, "import os,json; print(json.dumps(sorted(os.listdir('/workspace'))))"), ["escape", "file"])
        self.assertFalse(probe(session, "import os,json; print(json.dumps(os.path.exists('/workspace/escape')))"))
        probe(session, "import json; open('/workspace/file','w').write('changed'); print(json.dumps(True))")
        self.assertEqual((source / "file").read_text(), "snapshot")
        # Keep an attacker child trying to replace the protected destination
        # hierarchy while the parent remains the original shell.
        attack = """import os
for _ in range(20000):
 for p in ['/nix/store/evil','/run/goblins/packages/evil']:
  try: os.symlink('/tmp',p)
  except OSError: pass
"""
        session.input.write(str(session.python) + " -c " + shlex.quote(attack) + " &\n")
        session.grant("jq", self.paths["jq"])
        self.assertFalse(probe(session, "import os,json; print(json.dumps(os.path.lexists('/nix/store/evil')))"))
        # Fault injection on the TRUSTED side proves openat2 rejects symlinks
        # even if destination immutability were broken. No arbitrary mount RPC.
        victim = self.session()
        victim.fault = "symlink"
        with self.assertRaisesRegex(RuntimeError, "helper exited"):
            victim.grant("jq", self.paths["jq"])
        self.assertIsNotNone(victim.proc.poll())
        self.assertFalse((victim.directory / "packages/current").exists())

    def test_controller_death_kills_descendants(self):
        # A separate HOST driver simulates a crashing controller. Its shell still
        # starts through the same sandbox, and sleeps only inside that boundary.
        config = json.dumps(self.config)
        script = f'''import json,os,time
from support import Session
s=Session(**json.loads({config!r})); s.start()
s.input.write("while :; do :; done & wait\\n")
print(json.dumps([str(s.directory),s.proc.pid,s.identity['pid']]),flush=True)
time.sleep(60)
'''
        driver = subprocess.Popen(["python3", "-c", script], cwd=Path(__file__).resolve().parent, stdout=subprocess.PIPE, text=True)
        directory, helper, payload = json.loads(driver.stdout.readline())
        driver.kill(); driver.wait(); driver.stdout.close()
        self.addCleanup(lambda: __import__('shutil').rmtree(directory, ignore_errors=True))
        for _ in range(100):
            if not Path(f"/proc/{payload}").exists():
                break
            time.sleep(.02)
        self.assertFalse(Path(f"/proc/{payload}").exists(), "payload survived controller death")
        # Abrupt death cannot run Rust cleanup; document stale host GC roots.
        self.assertTrue(Path(directory).exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
