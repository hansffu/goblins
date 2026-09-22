"""Real per-shell Docker engines: local images, bind authority and lifecycle.

Run on the host as a regular user with newuidmap/newgidmap and subuid/subgid.
The default suite uses no host Docker socket, registry credentials or remote
images. The opt-in Testcontainers check pulls Postgres and Ryuk from Docker Hub.
"""
import os
import http.server
import json
from pathlib import Path
import shlex
import shutil
import tempfile
import socket
import threading
import unittest
from unittest.mock import patch

from daemon_support import Daemon, ROOT, RPC
from support import command
from terminal_support import Terminal


class DockerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                               f"git+file://{ROOT}#checks.x86_64-linux.docker-goblins"])) / "bin/goblins"
        cls.image = Path(command(["nix", "build", "--no-link", "--print-out-paths",
                                 f"git+file://{ROOT}#checks.x86_64-linux.docker-test-image"]))

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="goblins-docker-test-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.rw = self.root / "writable"
        self.ro = self.root / "readonly"
        self.rw.mkdir()
        self.ro.mkdir()
        (self.ro / "marker").write_text("readonly-ok\n")
        (self.root / "secret").write_text("host-secret\n")
        shutil.copyfile(self.image, self.rw / "image.tar.gz")
        self.env = {**os.environ, "GOBLINS_TEST_ROOT": str(self.root), "XDG_CACHE_HOME": str(self.root / "cache")}
        self.storage = self.root / "cache/goblins/docker"
        self.addCleanup(self.assert_storage_empty)
        self.d = Daemon(self.app, env=self.env)
        self.addCleanup(self.d.close)

    def assert_storage_empty(self):
        if self.storage.exists():
            self.assertEqual(list(self.storage.iterdir()), [], "Docker storage survived session cleanup")

    def shell(self, name="offline"):
        session = self.d.start(name, wait=False)["session"]
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        try:
            terminal.expect("docker-test>", timeout=60)
        except AssertionError as error:
            raise AssertionError(self.d.get(session).get("detail") or str(error)) from error
        return session, terminal

    def run_ok(self, terminal, text, timeout=40):
        terminal.send(text + "; printf '\\nDOCKER_TEST_STATUS=%s\\n' \"$?\"\n")
        result = terminal.expect(r"([\s\S]*?)\nDOCKER_TEST_STATUS=(\d+)\n", timeout=timeout)
        self.assertEqual(result[2], b"0", result[1].decode(errors="replace"))
        terminal.expect("docker-test>")

    def load(self, terminal):
        self.run_ok(terminal, f"docker load -i {shlex.quote(str(self.rw / 'image.tar.gz'))}")

    def enable(self, session, terminal, approved=True):
        terminal.send("goblins enable-docker --reason 'integration test'\n")
        request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
        self.assertEqual(request["kind"], "docker")
        self.d.decide(request, approved)
        terminal.expect('"status":"' + ("ready" if approved else "denied") + '"', timeout=60)
        terminal.expect("docker-test>")

    def test_enable_docker_on_demand(self):
        session, terminal = self.shell("ondemand")
        resources = self.d.state / session / "resources"
        self.assertFalse((resources / "docker.log").exists())
        self.assertFalse((resources / "docker-socket/docker.sock").exists())
        self.assert_storage_empty()
        self.run_ok(terminal, "! command -v docker; readlink /proc/self/ns/net > /workspace/network-before")
        self.enable(session, terminal, approved=False)
        self.assert_storage_empty()
        self.assertFalse(self.d.get(session)["docker_enabled"])
        # A server already running in the shell must remain reachable after activation.
        self.run_ok(terminal, "(python -m http.server 18081 --bind 127.0.0.1 --directory /workspace >/tmp/http.log 2>&1 &)")
        self.enable(session, terminal)
        self.assertTrue(self.d.get(session)["docker_enabled"])
        self.assertNotIn("Docker engine", self.d.get(session)["packages"])
        self.run_ok(terminal, "test \"$(readlink /proc/self/ns/net)\" = \"$(cat /workspace/network-before)\"")
        self.load(terminal)
        self.run_ok(terminal, "docker run --rm --network host goblins-test wget -q -O - -T 5 http://127.0.0.1:18081/network-before")
        store, = self.storage.iterdir()
        self.run_ok(terminal, "goblins enable-docker")
        self.assertEqual(list(self.storage.iterdir()), [store])
        terminal.send("exit\n")
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")
        self.assertFalse(store.exists())

    def test_on_demand_offline_and_private_scopes(self):
        first, terminal = self.shell("plain")
        second, other = self.shell("plain")
        self.enable(first, terminal)
        self.load(terminal)
        self.assertFalse(self.d.get(second)["docker_enabled"])
        self.run_ok(other, "! command -v docker")
        self.enable(second, other)
        self.run_ok(other, "test -z \"$(docker image ls -q)\"")
        self.run_ok(terminal, "docker run --rm --network host goblins-test sh -c '! ip route | grep default'")
        self.run_ok(terminal, f"! docker run --rm --mount type=bind,src={self.root}/secret,dst=/secret goblins-test true")

    def test_docker_request_authority_and_withdrawal(self):
        session, terminal = self.shell("plain")
        endpoint = self.d.state / session / "resources/request.sock"
        for params in (
            {"kind": "docker", "package": "hello", "reason": "test"},
            {"kind": "docker", "reason": ""},
            {"kind": "docker", "reason": "test", "daemon": "/bin/true"},
            {"kind": "docker", "reason": "test", "session": "other"},
        ):
            peer = RPC(endpoint)
            try:
                with self.assertRaises(ValueError):
                    peer.call("permissions.request", params)
            finally:
                peer.close()
        peer = RPC(endpoint)
        self.assertIn("docker-enable", peer.init["features"])
        peer.send("permissions.request", {"kind": "docker", "reason": "test"})
        request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
        self.assertIn("private rootless", request["preview"]["description"])
        self.assert_storage_empty()
        concurrent = RPC(endpoint)
        try:
            with self.assertRaises(ValueError):
                concurrent.call("permissions.request", {"kind": "docker", "reason": "duplicate"})
        finally:
            concurrent.close()
        peer.close()
        self.d.wait(lambda: not self.d.permissions(session=session, state="pending"))
        with self.assertRaises(ValueError):
            self.d.decide(request, True)
        self.assertFalse(self.d.get(session)["docker_enabled"])
        self.assert_storage_empty()
        self.run_ok(terminal, "! command -v docker")
        # A new, explicitly approved request can still succeed after withdrawal.
        self.enable(session, terminal)
        self.run_ok(terminal, 'case "$(goblins status)" in *"Docker: enabled"*) true;; *) false;; esac')

    def test_children_require_their_own_activation(self):
        parent, terminal = self.shell("plain")
        self.enable(parent, terminal)
        self.load(terminal)
        peer = RPC(self.d.state / parent / "resources/request.sock")
        try:
            child = peer.call("sessions.start", {"key": "docker-child", "name": "plain",
                              "agent_name": "child", "detached": True})["session"]
        finally:
            peer.close()
        other = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", child])
        self.addCleanup(other.close)
        other.expect("docker-test>", timeout=60)
        self.assertFalse(self.d.get(child)["docker_enabled"])
        self.enable(child, other)  # Must present a new approval, not inherit it.
        self.run_ok(other, "test -z \"$(docker image ls -q)\"")

    def test_activation_failure_preserves_shell_and_cleans_storage(self):
        manifest = json.loads(Path(self.d.manifest).read_text())
        # A trusted host manifest with a deliberately invalid engine executable.
        config = manifest["goblins"]["plain"]
        config["docker"]["daemon"] = config["docker"]["client"] + "/bin/docker"
        path = self.root / "broken-engine.json"
        path.write_text(json.dumps(manifest))
        session = self.d.start("plain", manifest=path)["session"]
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        terminal.expect("docker-test>", timeout=60)
        for _ in range(2):
            terminal.send("goblins enable-docker\n")
            request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
            self.d.decide(request, True)
            terminal.expect('"status":"error"', timeout=60)
            terminal.expect("docker-test>")
            self.assert_storage_empty()
            self.assertFalse(self.d.get(session)["docker_enabled"])
            self.run_ok(terminal, "test -d /workspace")

    def test_long_socket_path(self):
        # Match nix develop's nested TMPDIR: the host-side Docker socket path
        # exceeds sockaddr_un even though the engine's internal path is short.
        temp = tempfile.TemporaryDirectory(prefix="nix-shell-", dir="/tmp")
        self.addCleanup(temp.cleanup)
        with patch("tempfile.tempdir", temp.name):
            self.d = Daemon(self.app, env=self.env)
        self.addCleanup(self.d.close)
        session, terminal = self.shell()
        socket_path = self.d.state / session / "resources/docker-socket/docker.sock"
        self.assertGreaterEqual(len(os.fsencode(socket_path)), 108)
        self.run_ok(terminal, "docker info >/dev/null")

    def test_local_image_and_bind_permissions(self):
        session, terminal = self.shell()
        self.run_ok(terminal, "docker info --format '{{json .SecurityOptions}}'")
        self.run_ok(terminal, "test \"$(docker info --format '{{.Driver}}')\" = overlay2")
        self.load(terminal)
        self.run_ok(terminal, "docker run --rm --network none goblins-test sh -c 'echo container-ok'")
        self.run_ok(terminal, "docker run --rm --network none --user 1234:1234 goblins-test sh -c 'test \"$(id -u)\" = 1234'")
        self.run_ok(terminal, f"docker run --rm --network none -v {self.rw}:/work -v {self.ro}:/ro:ro goblins-test sh -c 'cat /ro/marker && echo written > /work/result && ! echo bad > /ro/marker'")
        self.assertEqual((self.rw / "result").read_text(), "written\n")
        self.assertEqual((self.ro / "marker").read_text(), "readonly-ok\n")
        self.run_ok(terminal, "docker run --rm --privileged --network none -v /sys:/sys:ro goblins-test sh -c '! mount -o remount,rw /sys/fs/cgroup'")
        self.run_ok(terminal, "docker run --rm --privileged --network none -v /sys:/sys:ro goblins-test sh -c 'mkdir /cgroup-probe; ! mount -t cgroup2 none /cgroup-probe'")
        self.run_ok(terminal, "! docker run --rm --cgroupns private --network none goblins-test true")
        # Bind resolution happens in the confined engine, not in the host.
        self.run_ok(terminal, f"! docker run --rm --network none --mount type=bind,src={self.root}/secret,dst=/secret goblins-test cat /secret")
        # Even a privileged container cannot remount an inherited readonly grant.
        self.run_ok(terminal, f"docker run --rm --privileged --network none -v /sys:/sys:ro -v {self.ro}:/ro:ro goblins-test sh -c '! mount -o remount,rw /ro && ! echo bad > /ro/marker'")
        self.run_ok(terminal, f"! docker run --rm --privileged --network none -v /sys:/sys:ro -v {self.ro}:/ro goblins-test sh -c 'echo bad > /ro/marker'")
        self.assertEqual((self.ro / "marker").read_text(), "readonly-ok\n")
        print("EVIDENCE Docker loaded local image, ran containers, wrote allowed bind, rejected host secret and readonly remount", flush=True)

    def test_local_build(self):
        _, terminal = self.shell()
        self.load(terminal)
        context = self.rw / "build"
        context.mkdir()
        (context / "Dockerfile").write_text("FROM goblins-test:latest\nRUN echo built-ok > /proof\n")
        self.run_ok(terminal, f"docker build --pull=false -t goblins-built {context}", timeout=60)
        self.run_ok(terminal, "docker run --rm --network none goblins-built sh -c 'test \"$(cat /proof)\" = built-ok'")

    def test_entrypoint_switches_user(self):
        _, terminal = self.shell()
        self.load(terminal)
        # Like the Postgres entrypoint: start as root, initialize a volume,
        # then drop supplementary groups, GID and UID before serving.
        as_postgres = 'test "$(id -u)" = 70 && test "$(id -g)" = 70 && echo written > /data/proof'
        entrypoint = (
            "mkdir -p /etc; "
            "echo postgres:x:70:70:postgres:/data:/bin/sh > /etc/passwd; "
            "echo postgres:x:70: > /etc/group; "
            "chown 70:70 /data; "
            f"su -s /bin/sh postgres -c {shlex.quote(as_postgres)}"
        )
        self.run_ok(terminal, f"docker run --rm --network none -v user-data:/data goblins-test sh -c {shlex.quote(entrypoint)}")
        self.run_ok(terminal, "docker run --rm --network none --user 70:70 -v user-data:/data goblins-test sh -c 'test \"$(cat /data/proof)\" = written'")
        self.run_ok(terminal, "docker run --rm --network none --user 70:70 --group-add 2345 goblins-test sh -c 'test \"$(id -G)\" = \"70 2345\"'")
        self.run_ok(terminal, "! docker run --rm --network none --group-add 65537 goblins-test true")

    @unittest.skipUnless(os.environ.get("GOBLINS_TESTCONTAINERS_CLASSPATH"),
                         "requires Testcontainers Java jars and registry access")
    def test_testcontainers_postgres_and_ryuk(self):
        session, terminal = self.shell("java")
        self.enable(session, terminal)
        jars = self.rw / "jars"
        jars.mkdir()
        for source in os.environ["GOBLINS_TESTCONTAINERS_CLASSPATH"].split(os.pathsep):
            shutil.copyfile(source, jars / Path(source).name)
        source = self.rw / "TestcontainersSmoke.java"
        shutil.copyfile(ROOT / "tests/TestcontainersSmoke.java", source)
        self.run_ok(terminal, f"java -cp '{jars}/*' {source}", timeout=240)
        # The JVM halts without shutdown hooks, leaving cleanup to Ryuk.
        query = "docker ps -aq --filter label=goblins.testcontainers-smoke=true"
        self.run_ok(terminal, f'for attempt in $(seq 1 45); do test -z "$({query})" && break; sleep 1; done; test -z "$({query})"', timeout=60)

    def test_independent_engines_and_workspace(self):
        one, first = self.shell()
        two, second = self.shell()
        self.load(first)
        self.run_ok(second, "test -z \"$(docker image ls -q)\"")
        self.run_ok(first, "echo workspace-ok > /workspace/shell-file; docker run --rm --network none -v /workspace:/work goblins-test sh -c 'test \"$(cat /work/shell-file)\" = workspace-ok'")
        self.run_ok(first, "docker run -d --network none --name survivor goblins-test sleep 300")
        first.send("exit\n")
        self.d.wait(lambda: self.d.get(one)["state"] == "stopped")
        self.run_ok(second, "docker info >/dev/null")
        self.assertFalse((self.d.state / one / "resources/docker-socket/docker.sock").exists())
        print("EVIDENCE isolated image stores, shell/container shared workspace, per-session engine cleanup", flush=True)

    def test_disk_cleanup_subuids_symlinks_and_other_sessions(self):
        one, first = self.shell()
        first_store, = self.storage.iterdir()
        _, second = self.shell()
        second_store, = set(self.storage.iterdir()) - {first_store}
        self.load(first)
        self.load(second)
        self.run_ok(second, "docker run --rm --network none -v keep:/data goblins-test sh -c 'echo keep > /data/marker'")
        (self.rw / "persistent").write_text("host-data\n")
        self.run_ok(first, f"docker run --rm --network none -v disposable:/data goblins-test sh -c 'mkdir /data/locked; echo secret > /data/locked/file; chown -R 1234:1234 /data/locked; chmod 000 /data/locked; ln -s /run/goblins/docker-storage-parent/{second_store.name} /data/sibling; ln -s {self.rw} /data/host; dd if=/dev/zero of=/data/bulk bs=1M count=128'")
        locked = first_store / "volumes/disposable/_data/locked"
        self.assertNotEqual(locked.stat().st_uid, os.getuid())
        self.assertEqual(locked.stat().st_mode & 0o777, 0)
        self.assertEqual(first_store.stat().st_dev, self.root.stat().st_dev)
        self.assertGreater((first_store / "volumes/disposable/_data/bulk").stat().st_blocks, 200000)
        # The cleanup parent is hidden even from privileged containers.
        self.run_ok(first, "docker run --rm --privileged --network none -v /sys:/sys:ro -v /run/goblins/docker-storage-parent:/probe:ro goblins-test sh -c 'test -z \"$(ls -A /probe)\"'")
        self.run_ok(first, f"! docker run --rm --network none --mount type=bind,src={self.storage},dst=/probe goblins-test true")
        first.send("exit\n")
        self.d.wait(lambda: self.d.get(one)["state"] == "stopped")
        self.assertFalse(first_store.exists())
        self.assertTrue(second_store.exists())
        self.assertEqual((self.rw / "persistent").read_text(), "host-data\n")
        self.run_ok(second, "docker run --rm --network none -v keep:/data goblins-test sh -c 'test \"$(cat /data/marker)\" = keep'")

    def test_cancel_startup_cleans_storage(self):
        session = self.d.start("offline", wait=False)["session"]
        self.d.wait(lambda: self.storage.exists() and list(self.storage.iterdir()))
        self.d.rpc.call("sessions.stop", {"session": session})
        self.d.wait(lambda: self.d.get(session)["state"] == "stopped")
        self.assert_storage_empty()

    def test_storage_cannot_be_granted_or_snapshotted(self):
        self.storage.mkdir(mode=0o700, parents=True)
        snapshot = Daemon(self.app, env=self.env, workspace=self.root)
        self.addCleanup(snapshot.close)
        for name in ("offline", "plain"):
            session = snapshot.start(name, wait=False)["session"]
            record = snapshot.wait(lambda: (r if (r := snapshot.get(session))["state"] == "failed" else None))
            self.assertIn("protected Docker storage", record["detail"])
        self.rw.rename(self.root / "writable-original")
        self.rw.symlink_to(self.storage, target_is_directory=True)
        for name in ("offline", "plain"):
            session = self.d.start(name, wait=False)["session"]
            record = self.d.wait(lambda: (r if (r := self.d.get(session))["state"] == "failed" else None))
            self.assertIn("protected path", record["detail"])

    def test_shared_session_network(self):
        _, terminal = self.shell("shell")
        self.load(terminal)
        # 'host' means this session's network, never the real host network.
        self.run_ok(terminal, "docker run -d --network host --name web goblins-test httpd -f -p 8088 -h /bin")
        self.run_ok(terminal, "curl --retry 10 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:8088/sh -o /dev/null")
        _, offline = self.shell()
        self.load(offline)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect(("192.0.2.1", 9))  # Route lookup only; sends no packet.
            address = probe.getsockname()[0]

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"network-ok\n")

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer((address, 0), Handler)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        url = f"http://{address}:{server.server_port}"
        self.run_ok(terminal, f"test \"$(curl --fail --max-time 5 {url})\" = network-ok")
        self.run_ok(terminal, f"docker run --rm --network host goblins-test sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.run_ok(terminal, f"docker run --rm goblins-test sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.run_ok(offline, f"! docker run --rm --network host goblins-test wget -q -O - -T 2 {url}")
        self.run_ok(offline, f"! docker run --rm goblins-test wget -q -O - -T 2 {url}")

        # Ordinary Compose builds use the default bridge; services use a
        # user-defined bridge with embedded DNS and session-local published ports.
        context = self.rw / "compose"
        context.mkdir()
        (context / "Dockerfile").write_text(
            f"FROM goblins-test:latest\nRUN wget -q -O /proof -T 5 {url}\n")
        (context / "compose.yaml").write_text("""services:
  web:
    build: .
    image: goblins-compose-test
    command: ["httpd", "-f", "-p", "8080", "-h", "/"]
    ports: ["127.0.0.1:18080:8080"]
  client:
    image: goblins-test
    command: ["sleep", "300"]
""")
        compose = f"docker compose -p network-test -f {context}/compose.yaml"
        self.run_ok(terminal, f"{compose} build --progress plain", timeout=60)
        self.run_ok(terminal, f"{compose} up -d --no-build", timeout=60)
        self.run_ok(terminal, f"{compose} exec -T client sh -c 'test \"$(wget -q -O - -T 5 http://web:8080/proof)\" = network-ok'")
        self.run_ok(terminal, f"{compose} exec -T client sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.run_ok(terminal, "test \"$(curl --retry 5 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:18080/proof)\" = network-ok")
        self.run_ok(offline, "! curl --fail --max-time 2 http://127.0.0.1:18080/proof")
        # Building must not give an offline session an upstream connection.
        self.run_ok(offline, f"! docker build --pull=false {context}", timeout=60)
        self.run_ok(terminal, f"{compose} down", timeout=60)

    def test_server_death_reaps_containers(self):
        _, terminal = self.shell()
        self.load(terminal)
        self.run_ok(terminal, "docker run --rm --network none -v crash-data:/data goblins-test sh -c 'mkdir /data/locked; echo saved > /data/locked/file; chown -R 1234:1234 /data/locked; chmod 000 /data/locked'")
        self.run_ok(terminal, "docker run -d --network none goblins-test sleep 300")
        descendants = set()

        def children(pid):
            for task in Path(f"/proc/{pid}/task").iterdir():
                try:
                    ids = (task / "children").read_text().split()
                except FileNotFoundError:
                    continue
                for child in ids:
                    if child not in descendants:
                        descendants.add(child)
                        children(child)

        children(self.d.process.pid)
        self.assertTrue(any(Path(f"/proc/{pid}/comm").read_text().strip() == "dockerd" for pid in descendants))
        identities = {pid: Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19] for pid in descendants}
        self.d.process.kill()
        self.d.process.wait(timeout=5)

        def dead(pid, start):
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                return fields[0] == "Z" or fields[19] != start
            except FileNotFoundError:
                return True

        self.d.wait(lambda: all(dead(pid, start) for pid, start in identities.items()))
        self.d.wait(lambda: not list(self.storage.iterdir()))


if __name__ == "__main__":
    unittest.main(verbosity=2)
