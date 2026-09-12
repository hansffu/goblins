"""Host CLI. Only the separate request.py executable enters the sandbox."""
import argparse
import array
import contextlib
import errno
import fcntl
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import stat
import struct
import sys
import termios
import tty

from runtime import HERE, Session, receive

LIMIT = 4096


def private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError("control directory must be a non-symlink directory owned by you with mode 0700")
    return path


def default_state():
    root = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
    return root / "goblins" if root.is_dir() else Path(f"/tmp/goblins-control-{os.getuid()}")


def packet(value):
    return json.dumps(value, ensure_ascii=True).encode()


def announce(stream, text):
    stream.write(text + "\n")
    stream.flush()


def start_request(conn):
    conn.settimeout(3)
    raw = conn.recv(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ValueError("oversized host request")
    req = json.loads(raw)
    if not isinstance(req, dict) or set(req) != {"v", "op", "rows", "cols"}:
        raise ValueError("invalid host request")
    if type(req["v"]) is not int or req["v"] != 1 or req["op"] != "shell":
        raise ValueError("unsupported host operation")
    for key in ("rows", "cols"):
        if type(req[key]) is not int or not 1 <= req[key] <= 1000:
            raise ValueError("invalid terminal dimensions")
    return req


def serve(config, state, workspace=None):
    """Foreground owner; approvals come only from this process's host terminal."""
    state = private_directory(state)
    socket_path = state / "serve.sock"
    lock = os.open(state / "serve.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(lock)
        raise RuntimeError("goblins serve is already running")
    session = None
    control = None
    try:
        with open("/dev/tty", "r") as approval, open("/dev/tty", "w", buffering=1) as screen, \
                socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as listener, \
                selectors.DefaultSelector() as events:
            if socket_path.is_symlink():
                raise RuntimeError("refusing symlink at control socket")
            socket_path.unlink(missing_ok=True)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            events.register(listener, selectors.EVENT_READ, "attach")
            events.register(approval, selectors.EVENT_READ, "terminal")
            announce(screen, "Goblins serving. Run goblins shell in another terminal.")
            announce(screen, "Packages: pinned nixpkgs attributes (e.g. hello, cowsay, python3Packages.black). Type quit to stop.")

            def stop_session():
                nonlocal session, control
                if control is not None:
                    with contextlib.suppress(KeyError):
                        events.unregister(control)
                    control.close()
                    control = None
                if session is not None:
                    with contextlib.suppress(KeyError):
                        events.unregister(session.socket)
                    session.close()
                    session = None
                    announce(screen, "Shell stopped. Ready for goblins shell.")

            try:
                while True:
                    for key, _ in events.select():
                        if key.data == "terminal":
                            line = approval.readline()
                            if not line or line.strip() in ("quit", ":quit"):
                                return
                            if line.strip() in ("status", ":status"):
                                announce(screen, json.dumps({"session": session.identity if session else None,
                                                            "packages": list(session.packages) if session else []}))
                            else:
                                announce(screen, "No approval pending. Type status or quit.")
                        elif key.data == "attach":
                            peer, _ = listener.accept()
                            master = slave = None
                            try:
                                req = start_request(peer)
                                if session is not None:
                                    raise ValueError("one shell at a time; exit the existing shell first")
                                master, slave = os.openpty()
                                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", req["rows"], req["cols"], 0, 0))
                                session = Session(**config, workspace=workspace)
                                session.start(terminal_fd=slave)
                                os.close(slave); slave = None
                                # Only the host shell client receives the PTY master.
                                # Neither this connection nor the host listener is mounted.
                                peer.sendmsg([packet({"v": 1, "status": "attached"})],
                                             [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [master]))])
                                peer.settimeout(None)
                                control = peer
                                events.register(control, selectors.EVENT_READ, "control")
                                events.register(session.socket, selectors.EVENT_READ, "package")
                                announce(screen, "Fish connected. Package requests will appear here.")
                            except (OSError, ValueError, RuntimeError) as error:
                                with contextlib.suppress(OSError):
                                    peer.send(packet({"v": 1, "status": "error", "message": str(error)}))
                                peer.close()
                                # A rejected second attachment must not stop the first.
                                if session is not None and control is None:
                                    session.close(); session = None
                                announce(screen, "Attach error: " + json.dumps(str(error)))
                            finally:
                                for fd in (master, slave):
                                    if fd is not None:
                                        os.close(fd)
                        elif key.data == "control":
                            # EOF means the host attachment went away. There are no
                            # additional host commands on an attached connection.
                            stop_session()
                        elif key.data == "package" and session is not None:
                            conn, _ = session.socket.accept()
                            with conn:
                                try:
                                    req = receive(conn)
                                except (OSError, ValueError) as error:
                                    session.event("invalid-request", detail=str(error))
                                    reply = {"v": 1, "id": getattr(error, "request_id", None),
                                             "status": "error", "message": str(error)}
                                else:
                                    announce(screen, "REQUEST " + json.dumps(req, ensure_ascii=True))
                                    screen.write("Approve package? Type approve or deny: "); screen.flush()
                                    decision = approval.readline().strip()
                                    if decision in ("quit", ":quit"):
                                        return
                                    reply = session.decide(req, decision == "approve")
                                    if session.last_error:
                                        announce(screen, "DETAIL " + json.dumps(session.last_error[-12000:], ensure_ascii=True))
                                with contextlib.suppress(OSError):
                                    conn.sendall(packet(reply) + b"\n")
                                announce(screen, "RESULT " + json.dumps(reply, ensure_ascii=True))
                            if session.proc.poll() is not None:
                                stop_session()
            finally:
                stop_session()
    finally:
        socket_path.unlink(missing_ok=True)
        os.close(lock)


def shell(state):
    """Attach the caller's real terminal to its sandbox PTY; never approve."""
    if not os.isatty(0) or not os.isatty(1):
        raise RuntimeError("goblins shell requires an interactive terminal")
    state = private_directory(state)
    master = None
    with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as control:
        try:
            control.connect(str(state / "serve.sock"))
        except (FileNotFoundError, ConnectionRefusedError):
            raise RuntimeError("start goblins serve in another terminal first") from None
        size = os.get_terminal_size(0)
        control.send(packet({"v": 1, "op": "shell", "rows": max(1, size.lines), "cols": max(1, size.columns)}))
        data, ancillary, flags, _ = control.recvmsg(LIMIT + 1, socket.CMSG_SPACE(array.array("i").itemsize))
        received = []
        for level, kind, raw in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                fds = array.array("i")
                fds.frombytes(raw[:len(raw) - len(raw) % fds.itemsize])
                received.extend(fds)
        try:
            if flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC):
                raise RuntimeError("truncated attachment response")
            reply = json.loads(data)
            if not isinstance(reply, dict) or reply.get("status") != "attached":
                raise RuntimeError(reply.get("message", "attachment failed") if isinstance(reply, dict) else "invalid response")
            if len(received) != 1:
                raise RuntimeError("attachment did not provide exactly one terminal")
            master = received.pop()
        finally:
            for fd in received:
                os.close(fd)
        attributes = termios.tcgetattr(0)
        previous_handlers = {}

        def resize(*_):
            fcntl.ioctl(master, termios.TIOCSWINSZ, fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))

        def interrupted(*_):
            raise KeyboardInterrupt

        try:
            previous_handlers[signal.SIGWINCH] = signal.signal(signal.SIGWINCH, resize)
            previous_handlers[signal.SIGTERM] = signal.signal(signal.SIGTERM, interrupted)
            tty.setraw(0)
            resize()
            with selectors.DefaultSelector() as events:
                events.register(0, selectors.EVENT_READ, "input")
                events.register(master, selectors.EVENT_READ, "output")
                events.register(control, selectors.EVENT_READ, "server")
                while True:
                    for key, _ in events.select():
                        if key.data == "server":
                            return
                        try:
                            chunk = os.read(key.fd, 65536)
                        except OSError as error:
                            if error.errno == errno.EIO and key.data == "output":
                                return
                            raise
                        if not chunk:
                            return
                        target = master if key.data == "input" else 1
                        while chunk:
                            count = os.write(target, chunk)
                            chunk = chunk[count:]
        finally:
            termios.tcsetattr(0, termios.TCSADRAIN, attributes)
            for sig, handler in previous_handlers.items():
                signal.signal(sig, handler)
            os.close(master)


def main():
    parser = argparse.ArgumentParser(prog="goblins")
    parser.add_argument("--runtime", type=Path, default=HERE / "result-shell-runtime")
    parser.add_argument("--state-dir", type=Path, default=default_state())
    commands = parser.add_subparsers(dest="command", required=True)
    server = commands.add_parser("serve", help="own the sandbox and approve packages in this terminal")
    server.add_argument("--workspace", type=Path, help="snapshot a workspace before launching Fish")
    commands.add_parser("shell", help="start and attach a sandboxed Fish shell")
    args = parser.parse_args()
    if args.command == "serve":
        serve(json.loads(args.runtime.read_text()), args.state_dir, args.workspace)
    else:
        shell(args.state_dir)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
    except (OSError, ValueError, RuntimeError) as error:
        print("goblins: " + str(error), file=sys.stderr)
        sys.exit(1)
