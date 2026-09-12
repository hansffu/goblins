"""Host-only controller and launcher. Terminal presentation lives in terminal.py."""
import contextlib
import json
import os
from pathlib import Path
import platform
import re
import selectors
import shutil
import socket
import struct
import stat
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
STORE = re.compile(r"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._?=-]+\Z")
MAX_FRAME = 4096
PACKAGE = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*(?:\.[A-Za-z_][A-Za-z0-9_-]*)*\Z")
FIXTURES = {"script-tool", "data-tool"}


def package_name(name):
    if not isinstance(name, str) or len(name) > 200 or not PACKAGE.fullmatch(name):
        raise ValueError("package must be a nixpkgs attribute such as cowsay or python3Packages.black")
    return name


class RequestError(ValueError):
    def __init__(self, message, request_id):
        super().__init__(message)
        self.request_id = request_id


class PackageError(RuntimeError):
    def __init__(self, message, detail):
        super().__init__(message)
        self.detail = detail


def command(args):
    return subprocess.check_output([str(x) for x in args], text=True, stderr=subprocess.PIPE).strip()


def store_path(path):
    path = str(path)
    if not STORE.fullmatch(path) or len(Path(path).name) > 211:
        raise ValueError("not a canonical store output")
    if Path(path).is_symlink() or not (Path(path).is_dir() or Path(path).is_file()):
        raise ValueError("store output must be a regular file or directory")
    return Path(path)


def executable_store(name):
    return Path(shutil.which(name)).resolve().parents[1]


def seccomp():
    # Deliberate AF_UNIX allowance: pathname access is limited by mounts; abstract
    # sockets by a private network namespace. Deny namespace/mount authority and
    # ptrace. clone3 ENOSYS makes libc fall back to filterable clone on x86_64.
    if platform.machine() != "x86_64":
        raise RuntimeError("this PoC syscall filter currently supports x86_64 only")
    code = [(0x20, 0, 0, 4), (0x15, 1, 0, 0xC000003E), (0x06, 0, 0, 0x80000000),
            (0x20, 0, 0, 0), (0x35, 0, 1, 0x40000000), (0x06, 0, 0, 0x50001)]
    for nr in (165, 166, 155, 272, 308, 428, 429, 430, 431, 432, 442, 101, 304):
        code += [(0x15, 0, 1, nr), (0x06, 0, 0, 0x50001)]
    code += [(0x15, 0, 1, 435), (0x06, 0, 0, 0x50026),  # clone3 -> ENOSYS
             (0x15, 0, 4, 56), (0x20, 0, 0, 16),      # clone flags
             (0x45, 0, 1, 0x7E020000), (0x06, 0, 0, 0x50001),
             (0x06, 0, 0, 0x7FFF0000), (0x06, 0, 0, 0x7FFF0000)]
    return b"".join(struct.pack("=HBBI", *insn) for insn in code)


def parse_request(data):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate key")
            result[key] = value
        return result
    if len(data) > MAX_FRAME or not data.endswith(b"\n") or b"\n" in data[:-1]:
        raise ValueError("one bounded JSON line required")
    try:
        req = json.loads(data, object_pairs_hook=unique)
    except RecursionError as error:
        raise ValueError("JSON nesting too deep") from error
    if not isinstance(req, dict) or set(req) != {"v", "id", "op", "package", "reason"}:
        raise ValueError("invalid request fields")
    if type(req["v"]) is not int or req["v"] != 1 or req["op"] != "request-package":
        raise ValueError("unsupported operation")
    if not isinstance(req["id"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", req["id"]):
        raise ValueError("invalid request id")
    try:
        package_name(req["package"])
        if not isinstance(req["reason"], str) or not 1 <= len(req["reason"]) <= 1024:
            raise ValueError("reason must contain 1..1024 characters")
    except ValueError as error:
        raise RequestError(str(error), req["id"]) from error
    return req


def receive(conn):
    conn.settimeout(3)
    data = bytearray()
    while not data.endswith(b"\n"):
        chunk = conn.recv(min(1024, MAX_FRAME + 1 - len(data)))
        if not chunk:
            raise ValueError("incomplete request")
        data.extend(chunk)
        if len(data) > MAX_FRAME:
            raise ValueError("request too large")
    return parse_request(bytes(data))


def snapshot(source, destination):
    """Copy into an unshared disposable directory without following source links.

    Special files (including sockets) and every .git entry are deliberately
    omitted. Agent-writable destination exists only after this host operation.
    """
    def copy_dir(source_fd, dest):
        for name in os.listdir(source_fd):
            if name == ".git":
                continue
            info = os.stat(name, dir_fd=source_fd, follow_symlinks=False)
            target = dest / name
            if stat.S_ISLNK(info.st_mode):
                target.symlink_to(os.readlink(name, dir_fd=source_fd))
            elif stat.S_ISDIR(info.st_mode):
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=source_fd)
                try:
                    target.mkdir()
                    copy_dir(child, target)
                finally:
                    os.close(child)
            elif stat.S_ISREG(info.st_mode):
                source_file = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=source_fd)
                with os.fdopen(source_file, "rb") as stream:
                    current = os.fstat(stream.fileno())
                    if not stat.S_ISREG(current.st_mode):
                        raise ValueError("workspace file changed type during snapshot")
                    with target.open("xb") as output:
                        shutil.copyfileobj(stream, output)
                target.chmod(current.st_mode & 0o777)
    source_fd = os.open(source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        copy_dir(source_fd, destination)
    finally:
        os.close(source_fd)


class Session:
    def __init__(self, *, shell=None, python=None, helper=None, bwrap=None, flake=None,
                 workspace=None, initial_packages=(), shell_args=None):
        self.directory = Path(tempfile.mkdtemp(prefix="goblins-"))
        self.closed = False
        self.proc = None
        self.socket = None
        self.mounted = set()
        self.packages = {}
        self.last_error = None
        self.flake = str(flake or ("path:" + str(HERE)))
        self.shell = Path(shell or executable_store("bash") / "bin/bash")
        self.python = Path(python or Path(shutil.which("python3")).resolve())
        self.helper = str(helper or HERE / "target/debug/goblins-mount-helper")
        self.bwrap = str(bwrap or shutil.which("bwrap"))
        self.initial_packages = [store_path(path) for path in initial_packages]
        self.shell_args = shell_args or ["--noprofile", "--norc"]
        for name in ("store", "packages", "roots", "workspace", "bin"):
            (self.directory / name).mkdir()
        if workspace:
            try:
                snapshot(workspace, self.directory / "workspace")
            except Exception:
                shutil.rmtree(self.directory)
                raise
        self.log = self.directory / "events.jsonl"
        self.stderr = open(self.directory / "helper.log", "w+")
        self.input = self.output = None

    def event(self, event, **fields):
        with self.log.open("a") as stream:
            stream.write(json.dumps({"time": time.time(), "event": event, **fields}, ensure_ascii=True) + "\n")

    def root(self, path):
        path = store_path(path)
        link = self.directory / "roots" / path.name
        if not link.is_symlink():
            # Host Nix creates an indirect GC root before closure enumeration.
            command(["nix-store", "--realise", path, "--add-root", link, "--indirect"])
        return path

    def closure(self, outputs):
        roots = [self.root(p) for p in outputs]
        return {store_path(p) for p in command(["nix-store", "--query", "--requisites", *roots]).splitlines()}

    def placeholders(self, paths):
        for path in paths:
            dst = self.directory / "store" / path.name
            if path.is_dir():
                dst.mkdir(exist_ok=True)
            else:
                dst.touch(exist_ok=True)

    def start(self, argv=None, *, terminal_fd=None):
        initial = self.closure([self.shell.parents[1], self.python.parents[1], *self.initial_packages])
        self.placeholders(initial)
        client = (HERE / "request.py").read_text()
        for name in ("goblins", "goblins-request"):
            target = self.directory / "bin" / name
            target.write_text(f"#!{self.python}\n" + client)
            target.chmod(0o555)
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(self.directory / "request.sock"))
        os.chmod(self.directory / "request.sock", 0o600)
        self.socket.listen(1)
        path = ":".join(["/run/goblins/packages/current/bin", "/run/goblins/bin",
                         str(self.shell.parent), *[str(p / "bin") for p in self.initial_packages]])
        args = ["--unshare-all", "--hostname", "sandbox", "--cap-drop", "ALL", "--die-with-parent",
                "--clearenv", "--setenv", "HOME", "/home/agent", "--setenv", "LC_ALL", "C",
                "--setenv", "USER", "agent", "--setenv", "LOGNAME", "agent",
                "--setenv", "PATH", path,
                "--setenv", "SHELL", str(self.shell), "--setenv", "PYTHONNOUSERSITE", "1",
                "--setenv", "TERM", "xterm-256color",
                "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
                "--tmpfs", "/home/agent", "--bind", str(self.directory / "workspace"), "/workspace",
                "--ro-bind", str(self.directory / "store"), "/nix/store",
                "--ro-bind", str(self.directory / "packages"), "/run/goblins/packages",
                "--ro-bind", str(self.directory / "bin"), "/run/goblins/bin",
                "--ro-bind", str(self.directory / "request.sock"), "/run/goblins/request.sock",
                "--symlink", str(self.shell), "/bin/sh", "--chdir", "/workspace"]
        for path in sorted(initial):
            args += ["--ro-bind", str(path), str(path)]
        args += ["--remount-ro", "/"]
        if terminal_fd is None:
            args += ["--new-session"]
        filt = os.memfd_create("goblins-seccomp", os.MFD_CLOEXEC)
        os.write(filt, seccomp()); os.lseek(filt, 0, 0)
        if terminal_fd is None:
            in_r, in_w = os.pipe()
            out_r, out_w = os.pipe()
            self.input = os.fdopen(in_w, "w", buffering=1)
            self.output = os.fdopen(out_r, "r", buffering=1)
        else:
            in_r, out_w = os.dup(terminal_fd), os.dup(terminal_fd)
        try:
            self.proc = subprocess.Popen(
                [self.helper, str(in_r), str(out_w), str(filt), self.bwrap,
                 *args, "--seccomp", str(filt), "--", *(argv or [str(self.shell), *self.shell_args])],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
                pass_fds=(in_r, out_w, filt), text=True, bufsize=1)
        finally:
            os.close(in_r); os.close(out_w); os.close(filt)
        ready = self.helper_reply(timeout=15)
        if not ready.startswith("READY "):
            raise RuntimeError(f"helper failed: {ready}")
        _, pid, owner, child_owner, source_mountns, mountns = ready.split()
        self.identity = dict(pid=int(pid), helper_userns=int(owner), mount_owner=int(child_owner), source_mountns=int(source_mountns), mountns=int(mountns))
        self.mounted = initial
        self.event("started", **self.identity)
        return self

    def helper_reply(self, timeout=30):
        with selectors.DefaultSelector() as sel:
            sel.register(self.proc.stdout, selectors.EVENT_READ)
            if not sel.select(timeout):
                raise RuntimeError("helper response timed out")
        result = self.proc.stdout.readline().strip()
        if not result:
            self.stderr.flush()
            raise RuntimeError("helper exited: " + (self.directory / "helper.log").read_text())
        return result

    def realize(self, name):
        # Only attribute components reach the pinned HOST flake. No expressions,
        # URLs, output selectors or workspace flakes come from the requester.
        package_name(name)
        namespace = "packages" if name in FIXTURES else "legacyPackages"
        attr = f"{self.flake}#{namespace}.x86_64-linux.{name}"
        nix = ["nix", "--extra-experimental-features", "nix-command flakes"]
        select_output = ('p: if builtins.isAttrs p && (p.type or null) == "derivation" '
                         'then (p.bin or p).outputName '
                         'else throw "attribute is not a package derivation"')
        try:
            output = json.loads(command([*nix, "eval", "--no-write-lock-file", "--json",
                                         attr, "--apply", select_output]))
        except subprocess.CalledProcessError as error:
            raise PackageError(f"cannot resolve package '{name}' from pinned nixpkgs; see serve terminal",
                               error.stderr) from error
        if not isinstance(output, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", output):
            raise ValueError("package has an unsupported output name")
        link = self.directory / "roots" / ("catalog-" + name)
        try:
            paths = command([*nix, "build", "--no-write-lock-file", "--out-link", link,
                             "--print-out-paths", attr + "^" + output]).splitlines()
        except subprocess.CalledProcessError as error:
            raise PackageError(f"could not build package '{name}'; see serve terminal", error.stderr) from error
        if len(paths) != 1:
            raise RuntimeError("catalog must resolve to exactly one output")
        return store_path(paths[0])

    def profile(self, packages):
        # Detect executable collisions before mounting anything. A stable parent
        # is bound read-only, so only the host can publish generations.
        entries = {}
        for package in [*self.initial_packages, *packages.values()]:
            bindir = package / "bin"
            if not bindir.is_dir():
                continue
            for entry in sorted(bindir.iterdir()):
                if entry.name in entries and entries[entry.name] != entry:
                    raise ValueError("package executable collision: " + entry.name)
                entries[entry.name] = entry
        generation = self.directory / "packages" / f"generation-{len(self.packages)+1}"
        generation.mkdir()
        (generation / "bin").mkdir()
        for name, entry in entries.items():
            (generation / "bin" / name).symlink_to(entry)
        return generation

    def grant(self, name, path=None, *, fail_after=None):
        package_name(name)
        if name in self.packages:
            return
        path = self.root(path or self.realize(name))
        packages = {**self.packages, name: path}
        closure = self.closure([path])
        generation = self.profile(packages)
        missing = sorted(closure - self.mounted)
        self.placeholders(missing)
        try:
            for count, item in enumerate(missing):
                if fail_after is not None and count == fail_after:
                    raise RuntimeError("injected partial mount failure")
                self.proc.stdin.write(item.name + "\n"); self.proc.stdin.flush()
                if self.helper_reply() != "OK":
                    raise RuntimeError("mount not acknowledged")
                self.mounted.add(item)
            staged = self.directory / "packages/next"
            staged.symlink_to(generation.name)
            staged.replace(self.directory / "packages/current")
        except Exception:
            self.event("mount-failure", package=name)
            self.stop()  # no rollback claim; kill the disposable session
            raise
        self.packages = packages
        self.event("ready", package=name, output=str(path), closure=[str(x) for x in sorted(closure)])

    def decide(self, req, approved):
        self.last_error = None
        self.event("decision", request=req, approved=approved)
        if not approved:
            return {"v": 1, "id": req["id"], "status": "denied"}
        try:
            self.grant(req["package"])
            return {"v": 1, "id": req["id"], "status": "ready"}
        except Exception as error:
            self.last_error = getattr(error, "detail", str(error))
            self.event("error", detail=self.last_error)
            message = str(error) if isinstance(error, PackageError) else "grant failed; see serve terminal"
            return {"v": 1, "id": req["id"], "status": "error", "message": message}

    def stop(self):
        if self.proc and self.proc.poll() is None:
            with contextlib.suppress(BrokenPipeError, OSError):
                self.proc.stdin.write("STOP\n"); self.proc.stdin.flush()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill(); self.proc.wait()
        self.event("stopped")

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.stop()
        if self.socket:
            self.socket.close()
        for stream in (self.input, self.output, self.stderr,
                       self.proc.stdin if self.proc else None, self.proc.stdout if self.proc else None):
            if stream:
                stream.close()
        shutil.rmtree(self.directory)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
