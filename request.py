"""Untrusted in-sandbox request CLI; it cannot approve requests."""
import argparse
import json
import socket
import sys
import uuid
from pathlib import Path


def main():
    legacy = Path(sys.argv[0]).name == "goblins-request"
    parser = argparse.ArgumentParser(prog="goblins-request" if legacy else "goblins")
    parser.add_argument("kind", choices=["package" if legacy else "request-package"])
    parser.add_argument("package")
    parser.add_argument("--reason", default="Requested from the sandbox shell")
    args = parser.parse_args()
    req = {"v": 1, "id": uuid.uuid4().hex, "op": "request-package",
           "package": args.package, "reason": args.reason}
    data = (json.dumps(req, ensure_ascii=True) + "\n").encode()
    if len(data) > 4096:
        parser.error("request exceeds 4096 bytes")
    try:
        with socket.socket(socket.AF_UNIX) as sock:
            sock.connect("/run/goblins/request.sock")
            sock.sendall(data)
            result = bytearray()
            while not result.endswith(b"\n"):
                chunk = sock.recv(4097 - len(result))
                if not chunk or len(result) + len(chunk) > 4096:
                    raise ValueError("missing or oversized reply")
                result.extend(chunk)
            reply = json.loads(result)
            if not isinstance(reply, dict) or reply.get("v") != 1 or reply.get("id") != req["id"] or reply.get("status") not in ("ready", "denied", "error"):
                raise ValueError("invalid reply")
        print(json.dumps(reply, ensure_ascii=True))
        return 0 if reply["status"] == "ready" else 1
    except (OSError, ValueError) as error:
        print("goblins-request: outcome unknown; do not retry automatically: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
