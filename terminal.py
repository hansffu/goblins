"""Thin foreground terminal client. Approval input comes exclusively from /dev/tty."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import selectors
import sys
from runtime import Session, receive


def display(stream, value):
    # Render even shell output as JSON text, never raw terminal escape sequences.
    stream.write(value + "\n")
    stream.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", type=Path, help="JSON produced by nix build .#runtime")
    parser.add_argument("--demo", action="store_true")
    parser.add_argument("--workspace", type=Path, help="copy regular files/symlinks; omit .git and special files")
    parser.add_argument("--log", type=Path, help="copy host decision log here after exit")
    args = parser.parse_args()
    config = json.loads(args.runtime.read_text()) if args.runtime else {}
    # Refuse redirected/non-terminal approval channels, including piped 'yes'.
    with open("/dev/tty", "r") as terminal_input, open("/dev/tty", "w", buffering=1) as tty, Session(**config, workspace=args.workspace) as session:
        if not os.isatty(tty.fileno()):
            raise RuntimeError("approval requires a host controlling terminal")
        session.start()
        display(tty, "Goblins: enter shell commands; :status inspects; :quit stops. Output is escaped.")
        if args.demo:
            session.input.write('echo "original shell $$"; command -v jq || echo jq-absent; '
                                'goblins-request package jq --reason "Demonstrate a live package grant"; '
                                'echo "same shell $$"; jq -n \'{live:true}\'\n')
        try:
            with selectors.DefaultSelector() as events:
                events.register(terminal_input, selectors.EVENT_READ, "tty")
                events.register(session.output, selectors.EVENT_READ, "output")
                events.register(session.socket, selectors.EVENT_READ, "request")
                running = True
                while running:
                    for key, _ in events.select():
                        if key.data == "output":
                            chunk = os.read(session.output.fileno(), 4096)
                            if not chunk:
                                running = False
                                break
                            display(tty, "shell: " + json.dumps(chunk.decode(errors="replace"), ensure_ascii=True))
                        elif key.data == "tty":
                            line = terminal_input.readline()
                            if not line or line.strip() == ":quit":
                                running = False
                                break
                            if line.strip() == ":status":
                                display(tty, json.dumps({**session.identity, "packages": list(session.packages)}))
                            else:
                                session.input.write(line)
                        else:
                            conn, _ = session.socket.accept()
                            with conn:
                                try:
                                    req = receive(conn)
                                except (ValueError, OSError) as error:
                                    response = {"v": 1, "id": None, "status": "error", "message": "invalid request"}
                                    session.event("invalid-request", detail=str(error))
                                else:
                                    display(tty, "REQUEST " + json.dumps(req, ensure_ascii=True))
                                    tty.write("Approve this package? Type approve or deny: "); tty.flush()
                                    approved = terminal_input.readline().strip() == "approve"
                                    response = session.decide(req, approved)
                                with contextlib.suppress(OSError):
                                    conn.sendall((json.dumps(response) + "\n").encode())
                                display(tty, "RESULT " + json.dumps(response))
                                if session.proc.poll() is not None:
                                    running = False
                                    break
        finally:
            session.stop()
            if args.log:
                args.log.write_bytes(session.log.read_bytes())


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, KeyboardInterrupt) as error:
        print("goblins: " + str(error), file=sys.stderr)
        sys.exit(1)
