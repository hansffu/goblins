"""Docker in goblins, as a person and an agent use it: local images, scope engines and their lifecycle.

Run on the host as a regular user with newuidmap/newgidmap and subuid/subgid.
The default suite uses no host Docker socket, registry credentials or remote
images. The opt-in slow tier (GOBLINS_TEST_SLOW=1, `just test-feature slow`)
runs Docker Compose with online networking and, with
GOBLINS_TESTCONTAINERS_CLASSPATH, Testcontainers, which pulls Postgres and
Ryuk from Docker Hub.
"""
import contextlib
import fcntl
import http.server
import json
import os
from pathlib import Path
import resource
import shlex
import shutil
import signal
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch
import uuid

from build_support import fixture_app, fixture_path
from daemon_support import Daemon, ROOT
from support import step
from terminal_support import Terminal

SOCKET = "/run/goblins/docker-socket/docker.sock"


class HostServer:
    """An HTTP server on the host's own address, which only online goblins reach."""

    def __init__(self, test):
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
        test.addCleanup(server.server_close)
        test.addCleanup(server.shutdown)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.url = f"http://{address}:{server.server_port}"


class DockerWorkflow(unittest.TestCase):
    """Helpers for driving goblins of the docker-goblins fixture through their terminals."""

    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("docker-goblins")
        cls.image = fixture_path(f"git+file://{ROOT}#checks.x86_64-linux.docker-test-image")

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
        self.counter = 0

    def start_daemon(self, **options):
        self.d = Daemon(self.app, env=self.env, **options)
        self.addCleanup(self.d.close)
        return self.d

    def attach(self, session):
        terminal = Terminal([str(self.app), "--state-dir", str(self.d.state), "attach", session])
        self.addCleanup(terminal.close)
        try:
            terminal.expect("docker-test>", timeout=120)
        except AssertionError as error:
            raise AssertionError(self.d.get(session).get("detail") or str(error)) from error
        return terminal

    def shell(self, name, cwd=None, scope=None, **params):
        params = {"name": name, "configuration": self.d.manifest, "key": uuid.uuid4().hex,
                  "rows": 24, "cols": 100, **params}
        if name is None:
            del params["name"]  # a resumed goblin's configuration is recorded
        if cwd is not None:
            params["cwd"] = str(cwd)
        if scope is not None:
            params["scope"] = scope
        session = self.d.rpc.call("sessions.start", params)["session"]
        return session, self.attach(session)

    def member(self, owner, name, agent_name=None):
        """A goblin the raid owner runs (`goblin run`), attached from the host."""
        session = self.d.member(owner, name, agent_name=agent_name)["session"]
        return session, self.attach(session)

    def send(self, terminal, script):
        """Start a command; return the marker that end() waits for."""
        self.counter += 1
        terminal.send(f"printf 'BEGIN%s\\n' {self.counter}; {script}; printf '\\nEND{self.counter}=%s\\n' \"$?\"\n")
        return self.counter

    def end(self, terminal, n, timeout=60):
        match = terminal.expect(rf"BEGIN{n}\n([\s\S]*?)\nEND{n}=(\d+)\n", timeout=timeout)
        return match.group(1).decode(errors="replace"), int(match.group(2))

    def ok(self, terminal, script, timeout=60):
        output, code = self.end(terminal, self.send(terminal, script), timeout)
        self.assertEqual(code, 0, f"{script}\n{output}")
        return output

    def load(self, terminal):
        self.ok(terminal, f"docker load -i {shlex.quote(str(self.rw / 'image.tar.gz'))}")

    def enable(self, session, terminal, approved=True):
        n = self.send(terminal, "goblin enable-docker --reason 'integration test'")
        request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
        self.assertEqual(request["kind"], "docker")
        self.d.decide(request, approved)
        output, code = self.end(terminal, n)
        self.assertEqual(code, 0 if approved else 1, self.d.get(session).get("detail") or output)
        self.assertIn('"status":"ready"' if approved else '"status":"denied"', output)

    def grant(self, session, terminal, package):
        n = self.send(terminal, f"goblin request-package {package} --reason 'integration test'")
        request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
        self.d.decide(request, True)
        output, code = self.end(terminal, n, timeout=120)
        self.assertEqual(code, 0, output)

    def exit(self, session, terminal):
        terminal.send("exit\n")
        return self.d.wait(lambda: (r if (r := self.d.get(session))["state"] == "stopped" else None))

    def engines(self):
        """The dockerd processes the server runs."""
        pending, engines = [str(self.d.process.pid)], set()
        while pending:
            pid = pending.pop()
            try:
                if Path(f"/proc/{pid}/comm").read_text().strip() == "dockerd":
                    engines.add(pid)
                for task in Path(f"/proc/{pid}/task").iterdir():
                    pending.extend((task / "children").read_text().split())
            except FileNotFoundError:
                pass
        return engines

    def temporary_stores(self):
        return set(self.storage.glob("session-*"))


class DockerOnDemandWorkflow(DockerWorkflow):
    """A goblin asks for Docker, builds and runs local images, and gives it up when it exits."""

    def test_ask_for_docker_and_work_with_containers(self):
        # Match nix develop's nested TMPDIR: the host-side Docker socket path
        # exceeds sockaddr_un even though the engine's internal path is short.
        temp = tempfile.TemporaryDirectory(prefix="nix-shell-", dir="/tmp")
        self.addCleanup(temp.cleanup)
        with patch("tempfile.tempdir", temp.name):
            self.start_daemon()
        host = HostServer(self)

        with step("a goblin has no Docker until it asks"):
            # 'ondemand' is online and does not enable Docker itself.
            session, shell = self.shell("ondemand")
            resources = self.d.state / session / "resources"
            self.assertFalse((resources / "docker-socket/docker.sock").exists())
            self.assertFalse((resources / "docker.log").exists())
            self.assertEqual(self.temporary_stores(), set())
            self.ok(shell, "! command -v docker; readlink /proc/self/ns/net > /workspace/network-before")
            self.ok(shell, "(python -m http.server 18081 --bind 127.0.0.1 --directory /workspace >/tmp/http.log 2>&1 &)")

        with step("a denied request changes nothing"):
            self.enable(session, shell, approved=False)
            self.assertEqual(self.temporary_stores(), set())
            self.assertFalse(self.d.get(session)["docker_enabled"])
            self.assertFalse(self.d.record(session)["docker_enabled"])
            self.ok(shell, "! command -v docker")

        with step("an approved request attaches the running shell, whose server stays reachable"):
            self.enable(session, shell)
            record = self.d.get(session)
            self.assertTrue(record["docker_enabled"])
            self.assertNotIn("Docker engine", record["packages"])
            self.d.wait(lambda: self.d.record(session)["docker_enabled"])
            self.assertGreaterEqual(len(os.fsencode(resources / "docker-socket/docker.sock")), 108)
            self.ok(shell, 'test "$(readlink /proc/self/ns/net)" = "$(cat /workspace/network-before)"; '
                           'case "$(goblin status)" in *"Docker: enabled"*) true;; *) false;; esac')
            self.load(shell)
            self.ok(shell, "docker run --rm --network host goblins-test wget -q -O - -T 5 http://127.0.0.1:18081/network-before")
            store, = self.temporary_stores()
            # Asking again needs no approval and starts nothing new.
            self.ok(shell, "goblin enable-docker")
            self.assertEqual(self.temporary_stores(), {store})

        with step("build and run local images in a confined engine"):
            self.ok(shell, "test \"$(docker info --format '{{.Driver}}')\" = overlay2")
            self.ok(shell, f"mkdir /tmp/build && printf 'FROM goblins-test:latest\\nRUN echo built-ok > /proof\\n' > /tmp/build/Dockerfile && "
                           f"docker build --pull=false -t goblins-built /tmp/build")
            self.ok(shell, "docker run --rm --network none goblins-built sh -c 'test \"$(cat /proof)\" = built-ok'")
            self.ok(shell, "docker run --rm --network none --user 1234:1234 goblins-test sh -c 'test \"$(id -u)\" = 1234'")
            # Like the Postgres entrypoint: start as root, initialize a volume,
            # then drop supplementary groups, GID and UID before serving.
            as_postgres = 'test "$(id -u)" = 70 && test "$(id -g)" = 70 && echo written > /data/proof'
            entrypoint = ("mkdir -p /etc; echo postgres:x:70:70:postgres:/data:/bin/sh > /etc/passwd; "
                          "echo postgres:x:70: > /etc/group; chown 70:70 /data; "
                          f"su -s /bin/sh postgres -c {shlex.quote(as_postgres)}")
            self.ok(shell, f"docker run --rm --network none -v user-data:/data goblins-test sh -c {shlex.quote(entrypoint)}")
            self.ok(shell, "docker run --rm --network none --user 70:70 -v user-data:/data goblins-test sh -c 'test \"$(cat /data/proof)\" = written'")
            self.ok(shell, "docker run --rm --network none --user 70:70 --group-add 2345 goblins-test sh -c 'test \"$(id -G)\" = \"70 2345\"'")
            self.ok(shell, "! docker run --rm --network none --group-add 65537 goblins-test true")

        with step("containers get the goblin's binds and nothing else"):
            self.ok(shell, f"docker run --rm --network none -v {self.rw}:/work -v {self.ro}:/ro:ro goblins-test "
                           "sh -c 'cat /ro/marker && echo written > /work/result && ! echo bad > /ro/marker'")
            self.assertEqual((self.rw / "result").read_text(), "written\n")
            # Bind resolution happens in the confined engine, not in the host.
            self.ok(shell, f"! docker run --rm --network none --mount type=bind,src={self.root}/secret,dst=/secret goblins-test cat /secret")
            # Even a privileged container cannot remount a read-only grant or the cgroup tree.
            self.ok(shell, f"docker run --rm --privileged --network none -v /sys:/sys:ro -v {self.ro}:/ro:ro goblins-test "
                           "sh -c '! mount -o remount,rw /ro && ! echo bad > /ro/marker'")
            self.ok(shell, f"! docker run --rm --privileged --network none -v /sys:/sys:ro -v {self.ro}:/ro goblins-test sh -c 'echo bad > /ro/marker'")
            self.assertEqual((self.ro / "marker").read_text(), "readonly-ok\n")
            self.ok(shell, "docker run --rm --privileged --network none -v /sys:/sys:ro goblins-test sh -c '! mount -o remount,rw /sys/fs/cgroup'")
            self.ok(shell, "docker run --rm --privileged --network none -v /sys:/sys:ro goblins-test sh -c 'mkdir /cgroup-probe; ! mount -t cgroup2 none /cgroup-probe'")
            self.ok(shell, "! docker run --rm --cgroupns private --network none goblins-test true")
            # The engine's storage is hidden even from privileged containers.
            self.ok(shell, "docker run --rm --privileged --network none -v /sys:/sys:ro -v /run/goblins/docker-storage-parent:/probe:ro "
                           "goblins-test sh -c 'test -z \"$(ls -A /probe)\"'")
            self.ok(shell, f"! docker run --rm --network none --mount type=bind,src={self.storage},dst=/probe goblins-test true")
            self.ok(shell, "echo workspace-ok > /workspace/shell-file; docker run --rm --network none -v /workspace:/work goblins-test "
                           "sh -c 'test \"$(cat /work/shell-file)\" = workspace-ok'")

        with step("services find each other by name and publish ports to the goblin, as Compose uses them"):
            self.ok(shell, "docker network create services >/dev/null && "
                           "docker run -d --network services --name web -p 127.0.0.1::8080 goblins-test httpd -f -p 8080 -h /bin >/dev/null")
            self.ok(shell, "docker run --rm --network services goblins-test sh -c "
                           "'for i in $(seq 50); do wget -q -O /dev/null -T 2 http://web:8080/sh && exit 0; sleep .1; done; exit 1'")
            # Testcontainers asks the engine for the mapped port and connects to it.
            self.ok(shell, 'curl --retry 10 --retry-connrefused --retry-delay 1 --fail -o /dev/null "http://$(docker port web 8080/tcp | head -1)/sh"')
            # An online goblin's containers reach the network on both networks.
            self.ok(shell, f"docker run --rm goblins-test sh -c 'test \"$(wget -q -O - -T 5 {host.url})\" = network-ok'")
            self.ok(shell, f"docker run --rm --network services goblins-test sh -c 'test \"$(wget -q -O - -T 5 {host.url})\" = network-ok'")

        with step("a Testcontainers-style client drives the engine through its socket"):
            self.ok(shell, f'test "$DOCKER_HOST" = unix://{SOCKET} && test "$TESTCONTAINERS_RYUK_CONTAINER_PRIVILEGED" = false')
            self.ok(shell, f'case "$(curl --fail --silent --unix-socket {SOCKET} http://docker/version)" in *ApiVersion*) true;; *) false;; esac')
            # Like Ryuk: an unprivileged container given the socket removes the
            # client's labelled containers through it.
            self.ok(shell, "docker run -d --network none --label goblins.reaper=test goblins-test sleep 300 >/dev/null")
            self.ok(shell, f'docker run --rm --network none -v {SOCKET}:/var/run/docker.sock -v /nix/store:/nix/store:ro goblins-test '
                           '"$(readlink -f "$(command -v docker)")" rm -f $(docker ps -aq --filter label=goblins.reaper=test)')
            self.ok(shell, 'test -z "$(docker ps -aq --filter label=goblins.reaper=test)"')

        with step("goblins gc leaves a running engine alone"):
            self.d.cli("gc")
            self.assertEqual(self.temporary_stores(), {store})
            self.ok(shell, "docker run --rm --network none goblins-test true")

        with step("a package granted later runs in a container"):
            self.grant(session, shell, "hello")
            self.ok(shell, 'hello=$(readlink -f "$(command -v hello)"); docker run --rm --network none -v /nix/store:/nix/store:ro goblins-test "$hello"')
            self.ok(shell, 'package=$(dirname "$(dirname "$(readlink -f "$(command -v hello)")")"); '
                           'docker run --rm --network none -v "$package:/granted:ro" goblins-test test -x /granted/bin/hello')

        with step("an offline goblin in another scope has its own engine and reaches nothing"):
            # 'offline' attaches at launch.
            other, offline = self.shell("offline", scope="offline-b")
            other_store, = self.temporary_stores() - {store}
            self.ok(offline, 'test -z "$(docker image ls -q)"')
            self.load(offline)
            self.ok(offline, "docker run --rm --network none -v keep:/data goblins-test sh -c 'echo keep > /data/marker'")
            # 'host' means this goblin's network, never the real host network.
            self.ok(offline, f"! docker run --rm --network host goblins-test wget -q -O - -T 1 {host.url}")
            self.ok(offline, f"! docker run --rm goblins-test wget -q -O - -T 1 {host.url}")
            self.ok(offline, f"mkdir /tmp/fetch && printf 'FROM goblins-test:latest\\nRUN wget -q -O /proof -T 1 {host.url}\\n' > /tmp/fetch/Dockerfile && "
                             "! docker build --pull=false /tmp/fetch")

        with step("exiting removes the engine's data, even what containers made hard to delete"):
            (self.rw / "persistent").write_text("host-data\n")
            self.ok(shell, "docker run --rm --network none -v disposable:/data goblins-test sh -c "
                           "'mkdir /data/locked; echo secret > /data/locked/file; chown -R 1234:1234 /data/locked; chmod 000 /data/locked; "
                           f"ln -s /run/goblins/docker-storage-parent/{other_store.name} /data/sibling; ln -s {self.rw} /data/host'")
            locked = store / "volumes/disposable/_data/locked"
            self.assertNotEqual(locked.stat().st_uid, os.getuid())
            self.assertEqual(locked.stat().st_mode & 0o777, 0)
            self.ok(shell, "docker run -d --network none goblins-test sleep 300 >/dev/null")
            self.exit(session, shell)
            self.assertFalse(store.exists())
            self.assertFalse((resources / "docker-socket/docker.sock").exists())
            self.assertEqual((self.rw / "persistent").read_text(), "host-data\n")
            self.assertTrue(other_store.exists())
            self.ok(offline, "docker run --rm --network none -v keep:/data goblins-test sh -c 'test \"$(cat /data/marker)\" = keep'")

        with step("a resumed goblin has Docker before its first command"):
            self.d.wait_record_released(self.d.record(session)["id"])
            self.assertTrue(self.d.record(session)["docker_enabled"])
            resumed, shell = self.shell(None, recreate=session)
            record = self.d.get(resumed)
            self.assertTrue(record["docker_enabled"], record.get("detail"))
            self.ok(shell, f"test -S {SOCKET} && docker info >/dev/null")
            self.load(shell)
            self.ok(shell, "docker run --rm --network none goblins-test true")
            self.exit(resumed, shell)
            self.d.wait(lambda: resumed in self.d.record(session)["sessions"])
            self.assertTrue(self.d.record(session)["docker_enabled"])

        with step("when every goblin has exited no engine data is left"):
            self.exit(other, offline)
            self.d.wait(lambda: not self.temporary_stores())


class DockerSharedScopeWorkflow(DockerWorkflow):
    """Goblins in one scope share its engine; another scope's goblins see none of it."""

    def test_goblins_share_their_scope_engine(self):
        extra_ro, extra_rw = self.root / "extra-ro", self.root / "extra-rw"
        extra_ro.mkdir()
        extra_rw.mkdir()
        (extra_ro / "marker").write_text("scope-shared\n")
        # Joining an engine grants each member's store closure. A server
        # started with a small soft limit must neither run out of descriptors
        # nor pass its raised limit on to sandbox payloads.
        _, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        self.start_daemon(preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (128, hard)))

        with step("a goblin without a Docker scope cannot ask for Docker"):
            loner, alone = self.shell("unscoped")
            self.ok(alone, "! command -v docker; ! goblin enable-docker")
            self.assertFalse(self.d.permissions(session=loner, state="pending"))
            self.assertFalse(self.d.get(loner)["docker_enabled"])

        with step("A starts the scope's engine and makes an image, a volume and a published port"):
            a, shell_a = self.shell("scoped")
            self.assertEqual(self.d.get(a)["scope"], "work")
            self.assertEqual(self.engines(), set())
            self.assertFalse(self.storage.exists())
            self.ok(shell_a, "echo inherited > /workspace/marker")
            self.enable(a, shell_a)
            self.load(shell_a)
            self.ok(shell_a, "mkdir /tmp/build && printf 'FROM goblins-test:latest\\nRUN echo made-by-a > /made-by\\n' > /tmp/build/Dockerfile && "
                             "docker build --pull=false -t made-by-a /tmp/build")
            self.ok(shell_a, "docker run --rm --network none -v shared:/data goblins-test sh -c 'echo shared-value > /data/value'")
            self.ok(shell_a, "docker run -d --name shared-web -p 18082:8080 goblins-test httpd -f -p 8080 -h /bin >/dev/null")
            engine, = self.engines()

        with step("a goblin A runs starts attached, without asking"):
            helper, shell_m = self.member(a, "scoped", agent_name="helper")
            record = self.d.get(helper)
            self.assertTrue(record["docker_enabled"])
            self.assertEqual(record["scope"], "work")
            self.assertFalse(self.d.permissions(session=helper, state="pending"))
            self.ok(shell_m, "docker run --rm --network none made-by-a cat /made-by")

        with step("B in the same scope asks and sees A's image, volume and port"):
            b, shell_b = self.shell("scoped", cwd=self.rw)
            self.enable(b, shell_b)
            sockets = [self.d.state / s / "resources/docker-socket/docker.sock" for s in (a, b)]
            self.assertEqual(sockets[0].stat().st_ino, sockets[1].stat().st_ino)
            self.assertEqual(self.engines(), {engine})
            self.ok(shell_b, "docker run --rm --network none made-by-a sh -c 'test \"$(cat /made-by)\" = made-by-a'")
            self.ok(shell_b, "docker run --rm --network none -v shared:/data goblins-test sh -c 'test \"$(cat /data/value)\" = shared-value'")
            # Members share the scope network: published ports need no forwarding.
            self.ok(shell_b, "curl --retry 10 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:18082/sh -o /dev/null")
            self.ok(shell_b, 'test "$(ulimit -n)" = 128')

        with step("a goblin that attaches at launch brings its package and its binds, read-only kept read-only"):
            eager, shell_e = self.shell("eager-extra", cwd=self.rw)
            record = self.d.get(eager)
            self.assertTrue(record["docker_enabled"])
            self.assertEqual(record["scope"], "work")
            self.assertEqual(self.engines(), {engine})
            self.ok(shell_e, 'hello=$(readlink -f "$(command -v hello)"); docker run --rm --network none -v /nix/store:/nix/store:ro goblins-test "$hello"')
            # Its grants join the engine, for every member.
            self.ok(shell_a, f"docker run --rm --network none -v {extra_ro}:/ro:ro -v {extra_rw}:/rw goblins-test "
                             "sh -c 'test \"$(cat /ro/marker)\" = scope-shared; echo shared > /rw/marker'")
            self.assertEqual((extra_rw / "marker").read_text(), "shared\n")
            self.ok(shell_a, f"docker run --rm --privileged --network none -v /sys:/sys:ro -v {extra_ro}:/ro:ro goblins-test "
                             "sh -c '! mount -o remount,rw /ro && ! echo bad > /ro/marker'")
            self.ok(shell_a, f"! docker run --rm --privileged --network none -v /sys:/sys:ro -v {extra_ro}:/ro goblins-test sh -c 'echo bad > /ro/marker'")
            self.exit(eager, shell_e)
            self.ok(shell_a, f"docker run --rm --network none -v {extra_ro}:/ro:ro goblins-test cat /ro/marker")
            self.assertEqual((extra_ro / "marker").read_text(), "scope-shared\n")

        with step("C in another scope sees none of it; its member started that scope's engine"):
            c, shell_c = self.shell("member", cwd=self.rw)
            self.assertEqual(self.d.get(c)["scope"], "other")
            worker, shell_w = self.member(c, "member")
            self.assertFalse(self.d.get(worker)["docker_enabled"])
            self.enable(worker, shell_w)
            self.load(shell_w)
            self.assertFalse(self.d.get(c)["docker_enabled"])
            self.ok(shell_c, f"test ! -S {SOCKET}")
            self.enable(c, shell_c)
            self.assertEqual(len(self.engines()), 2)
            self.ok(shell_c, "docker image inspect goblins-test >/dev/null; ! docker image inspect made-by-a")
            self.ok(shell_c, '! docker volume inspect shared; ! curl --fail --max-time 2 http://127.0.0.1:18082/sh -o /dev/null')
            self.exit(worker, shell_w)
            self.exit(c, shell_c)
            self.d.wait(lambda: self.engines() == {engine})

        with step("A exits and its member's agent crashes; the engine keeps their workspace for the others"):
            workspace = self.d.state / a / "resources/workspace"
            self.exit(a, shell_a)
            # The member's payload dies by SIGKILL while the engine stays up.
            shell_m.send("kill -KILL $$\n")
            crashed = self.d.wait(lambda: (r if (r := self.d.get(helper))["state"] == "stopped" else None))
            self.assertNotEqual(crashed["exit_code"], 0, crashed)
            self.d.cli("gc")
            self.assertEqual(self.engines(), {engine})
            # gc kept both donors' resources: the engine still holds their leases.
            for donor in (a, helper):
                resources = self.d.state / donor / "resources"
                self.assertTrue(resources.exists(), donor)
                with open(resources / ".gc-lease", "rb") as lease:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.ok(shell_b, "docker run --rm --network none -v /workspace:/inherited goblins-test "
                             "sh -c 'test \"$(cat /inherited/marker)\" = inherited; echo container-write > /inherited/after'")
            self.assertEqual((workspace / "after").read_text(), "container-write\n")

        with step("a package granted after they left still reaches the engine"):
            self.grant(b, shell_b, "hello")
            self.ok(shell_b, 'hello=$(readlink -f "$(command -v hello)"); docker run --rm --network none -v /nix/store:/nix/store:ro goblins-test "$hello"')
            self.assertEqual(self.engines(), {engine})

        with step("when the last member leaves the engine stops; the scope keeps its data"):
            self.exit(b, shell_b)
            self.d.wait(lambda: not self.engines())
            for donor in (a, helper):
                self.d.wait(lambda donor=donor: not (self.d.state / donor / "resources").exists())
            self.assertTrue((self.storage / "scope-work").exists())
            fresh, shell_f = self.shell("scoped", cwd=self.rw)
            self.enable(fresh, shell_f)
            self.ok(shell_f, "docker run --rm --network none -v shared:/data goblins-test sh -c 'test \"$(cat /data/value)\" = shared-value'")
            self.ok(shell_f, "docker image inspect made-by-a >/dev/null")

        with step("scope clear waits for the scope to be unused, then empties it"):
            self.ok(shell_f, "docker run --rm --network none -v database:/data goblins-test sh -c "
                             "'mkdir /data/private; echo database > /data/private/value; chown -R 1234:1234 /data/private; chmod 000 /data/private'")
            (self.rw / "keep").write_text("host-side effect")
            refused = self.d.cli("scope", "clear", "work", check=False)
            self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
            self.assertIn("in use", refused.stderr)
            self.d.cli("gc")
            self.ok(shell_f, "docker image inspect made-by-a >/dev/null")
            self.exit(fresh, shell_f)
            self.d.wait(lambda: not (self.d.state / fresh / "resources").exists())
            self.d.cli("scope", "clear", "work")
            self.assertFalse((self.storage / "scope-work").exists())
            self.assertEqual((self.rw / "keep").read_text(), "host-side effect")
            again, shell_g = self.shell("scoped", cwd=self.rw)
            self.enable(again, shell_g)
            self.ok(shell_g, 'test -z "$(docker image ls -q)"; test -z "$(docker volume ls -q)"')
            self.exit(again, shell_g)
            self.d.wait(lambda: not (self.d.state / again / "resources").exists())
            self.d.process.terminate()
            self.d.process.wait(timeout=10)
            self.assertTrue(all(not (self.d.state / s).exists() for s in (a, b, fresh, again)))


class DockerFailureWorkflow(DockerWorkflow):
    """When an engine or the server fails, goblins and their data are not left behind."""

    def test_an_engine_that_cannot_start_leaves_the_goblin_working(self):
        self.start_daemon()
        manifest = json.loads(Path(self.d.manifest).read_text())
        # A trusted host manifest with a deliberately invalid engine executable.
        config = manifest["goblins"]["plain"]["scopes"]["offline-a"]
        config["docker"]["daemon"] = config["docker"]["client"] + "/bin/docker"
        path = self.root / "broken-engine.json"
        path.write_text(json.dumps(manifest))
        session, shell = self.shell("plain", configuration=str(path))
        for _ in range(2):
            n = self.send(shell, "goblin enable-docker")
            request = self.d.wait(lambda: next(iter(self.d.permissions(session=session, state="pending")), None))
            self.d.decide(request, True)
            output, code = self.end(shell, n)
            self.assertEqual(code, 1, output)
            self.assertIn('"status":"error"', output)
            self.assertEqual(self.temporary_stores(), set())
            self.assertFalse(self.d.get(session)["docker_enabled"])
            self.ok(shell, "test -d /workspace")

    def test_an_engine_crash_stops_the_goblins_of_its_scope(self):
        self.start_daemon()
        user, shell = self.shell("scoped", cwd=self.rw)
        idle, _ = self.shell("scoped", cwd=self.rw)
        elsewhere, other = self.shell("member", cwd=self.rw)
        self.enable(user, shell)
        engine, = self.engines()
        engine_fd = os.pidfd_open(int(engine))
        try:
            signal.pidfd_send_signal(engine_fd, signal.SIGKILL)
        finally:
            os.close(engine_fd)
        for member in (user, idle):
            record = self.d.wait(lambda: (r if (r := self.d.get(member))["state"] == "stopped" else None))
            self.assertEqual(record["exit_code"], 1, record)
            self.assertIsNone(record["stop_reason"], record)
        self.assertEqual(self.d.get(elsewhere)["state"], "running")
        self.ok(other, "test -d /workspace")

    def test_containers_and_engine_data_go_when_the_server_dies(self):
        self.start_daemon()
        # 'offline' attaches at launch, to an engine of its own.
        session, shell = self.shell("offline")
        self.load(shell)
        self.ok(shell, "docker run --rm --network none -v crash-data:/data goblins-test sh -c "
                       "'mkdir /data/locked; echo saved > /data/locked/file; chown -R 1234:1234 /data/locked; chmod 000 /data/locked'")
        self.ok(shell, "docker run -d --network none goblins-test sleep 300 >/dev/null")
        self.assertTrue(self.temporary_stores())
        descendants = {}
        pending = [str(self.d.process.pid)]
        while pending:
            for task in Path(f"/proc/{pending.pop()}/task").iterdir():
                with contextlib.suppress(FileNotFoundError):
                    for child in (task / "children").read_text().split():
                        if child not in descendants:
                            descendants[child] = Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()[19]
                            pending.append(child)
        self.assertTrue(any(Path(f"/proc/{pid}/comm").read_text().strip() == "dockerd" for pid in descendants))
        self.d.process.kill()
        self.d.process.wait(timeout=5)

        def dead(pid, start):
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                return fields[0] == "Z" or fields[19] != start
            except FileNotFoundError:
                return True

        self.d.wait(lambda: all(dead(pid, start) for pid, start in descendants.items()))
        self.d.wait(lambda: not list(self.storage.iterdir()))
        self.d.cli("gc")
        self.assertFalse((self.d.state / session).exists())


@unittest.skipUnless(os.environ.get("GOBLINS_TEST_SLOW"),
                     "slow tier: set GOBLINS_TEST_SLOW=1 or run `just test-feature slow`")
class DockerSlowTier(DockerWorkflow):
    """Real tools with online networking. The behaviour they rely on is also
    checked, faster, in DockerOnDemandWorkflow and DockerSharedScopeWorkflow."""

    def test_compose_with_online_networking(self):
        self.start_daemon()
        _, shell = self.shell("shell")
        self.load(shell)
        # 'host' means this goblin's network, never the real host network.
        self.ok(shell, "docker run -d --network host --name web goblins-test httpd -f -p 8088 -h /bin")
        self.ok(shell, "curl --retry 10 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:8088/sh -o /dev/null")
        _, offline = self.shell("offline")
        self.load(offline)
        url = HostServer(self).url
        self.ok(shell, f"test \"$(curl --fail --max-time 5 {url})\" = network-ok")
        self.ok(shell, f"docker run --rm --network host goblins-test sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.ok(shell, f"docker run --rm goblins-test sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.ok(offline, f"! docker run --rm --network host goblins-test wget -q -O - -T 2 {url}")
        self.ok(offline, f"! docker run --rm goblins-test wget -q -O - -T 2 {url}")
        # Ordinary Compose builds use the default bridge; services use a
        # user-defined bridge with embedded DNS and goblin-local published ports.
        context = self.rw / "compose"
        context.mkdir()
        (context / "Dockerfile").write_text(f"FROM goblins-test:latest\nRUN wget -q -O /proof -T 5 {url}\n")
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
        self.ok(shell, f"{compose} build --progress plain")
        self.ok(shell, f"{compose} up -d --no-build")
        self.ok(shell, f"{compose} exec -T client sh -c 'test \"$(wget -q -O - -T 5 http://web:8080/proof)\" = network-ok'")
        self.ok(shell, f"{compose} exec -T client sh -c 'test \"$(wget -q -O - -T 5 {url})\" = network-ok'")
        self.ok(shell, "test \"$(curl --retry 5 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:18080/proof)\" = network-ok")
        self.ok(offline, "! curl --fail --max-time 2 http://127.0.0.1:18080/proof")
        # Building must not give an offline goblin an upstream connection.
        self.ok(offline, f"! docker build --pull=false {context}")
        self.ok(shell, f"{compose} down")

    @unittest.skipUnless(os.environ.get("GOBLINS_TESTCONTAINERS_CLASSPATH"),
                         "requires Testcontainers Java jars and registry access")
    def test_testcontainers_postgres_and_ryuk(self):
        self.start_daemon()
        session, shell = self.shell("java")
        self.enable(session, shell)
        jars = self.rw / "jars"
        jars.mkdir()
        for source in os.environ["GOBLINS_TESTCONTAINERS_CLASSPATH"].split(os.pathsep):
            shutil.copyfile(source, jars / Path(source).name)
        source = self.rw / "TestcontainersSmoke.java"
        shutil.copyfile(ROOT / "tests/TestcontainersSmoke.java", source)
        self.ok(shell, f"java -cp '{jars}/*' {source}", timeout=240)
        # The JVM halts without shutdown hooks, leaving cleanup to Ryuk.
        query = "docker ps -aq --filter label=goblins.testcontainers-smoke=true"
        self.ok(shell, f'for attempt in $(seq 1 45); do test -z "$({query})" && break; sleep 1; done; test -z "$({query})"')


if __name__ == "__main__":
    unittest.main(verbosity=2)
