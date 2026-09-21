"""Real per-shell Docker engines: local images, bind authority and lifecycle.

Run on the host as a regular user with newuidmap/newgidmap and subuid/subgid.
No host Docker socket, registry credentials or remote images are used.
"""
import os
import http.server
from pathlib import Path
import shlex
import shutil
import tempfile
import socket
import threading
import unittest
from unittest.mock import patch

from daemon_support import Daemon, ROOT
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
        self.d = Daemon(self.app, env={**os.environ, "GOBLINS_TEST_ROOT": str(self.root)})
        self.addCleanup(self.d.close)

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

    def test_long_socket_path(self):
        # Match nix develop's nested TMPDIR: the host-side Docker socket path
        # exceeds sockaddr_un even though the engine's internal path is short.
        temp = tempfile.TemporaryDirectory(prefix="nix-shell-", dir="/tmp")
        self.addCleanup(temp.cleanup)
        with patch("tempfile.tempdir", temp.name):
            self.d = Daemon(self.app, env={**os.environ, "GOBLINS_TEST_ROOT": str(self.root)})
        self.addCleanup(self.d.close)
        session, terminal = self.shell()
        socket_path = self.d.state / session / "resources/docker-socket/docker.sock"
        self.assertGreaterEqual(len(os.fsencode(socket_path)), 108)
        self.run_ok(terminal, "docker info >/dev/null")

    def test_local_image_and_bind_permissions(self):
        session, terminal = self.shell()
        self.run_ok(terminal, "docker info --format '{{json .SecurityOptions}}'")
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
        self.run_ok(terminal, f"docker build --network none --pull=false -t goblins-built {context}", timeout=60)
        self.run_ok(terminal, "docker run --rm --network none goblins-built sh -c 'test \"$(cat /proof)\" = built-ok'")

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
        self.run_ok(offline, f"! docker run --rm --network host goblins-test wget -q -O - -T 2 {url}")

    def test_server_death_reaps_containers(self):
        _, terminal = self.shell()
        self.load(terminal)
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
