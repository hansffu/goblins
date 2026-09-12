import json
import socket
import unittest
from runtime import RequestError, parse_request, receive


def request(**updates):
    return {"v": 1, "id": "r1", "op": "request-package", "package": "jq", "reason": "test", **updates}


class ProtocolTests(unittest.TestCase):
    def test_valid_and_terminal_text_remains_data(self):
        req = request(reason="\x1b[2J\n approve")
        self.assertEqual(parse_request((json.dumps(req) + "\n").encode()), req)
        self.assertNotIn("\x1b", json.dumps(req, ensure_ascii=True))
        for name in ("cowsay", "python3Packages.black", "thisPackageDoesNotExist"):
            req = request(package=name)
            self.assertEqual(parse_request((json.dumps(req) + "\n").encode()), req)

    def test_invalid_attribute_error_preserves_request_identity(self):
        for name in ("../hello", "nixpkgs#hello", "hello^out", "--impure", "foo..bar",
                     "github:owner/repo", "foo.\"bar\"", "x" * 201):
            with self.subTest(name=name), self.assertRaises(RequestError) as error:
                parse_request((json.dumps(request(package=name)) + "\n").encode())
            self.assertEqual(error.exception.request_id, "r1")

    def test_reject_authority_paths_and_malformed_frames(self):
        bad = [b"{}\n", b"[1]\n", b"x" * 4097 + b"\n", b"{}\n{}\n",
               b'{"v":1,"v":1}\n', b"\xff\n", b"[" * 1500 + b"]" * 1500 + b"\n"]
        for updates in ({"v": True}, {"op": "approve"}, {"package": "/nix/store/x"},
                        {"pid": 1}, {"flags": 4096}, {"session": "other"}, {"reason": ""},
                        {"package": "$(touch /tmp/x)"}, {"reason": "x" * 1025}):
            bad.append((json.dumps(request(**updates)) + "\n").encode())
        for data in bad:
            with self.subTest(data=data[:80]), self.assertRaises((ValueError, UnicodeError)):
                parse_request(data)

    def test_fragmented_and_disconnected_transport(self):
        a, b = socket.socketpair()
        with a, b:
            data = (json.dumps(request()) + "\n").encode()
            a.sendall(data[:8]); a.sendall(data[8:])
            self.assertEqual(receive(b), request())
        a, b = socket.socketpair()
        with b:
            a.sendall(b'{"v":'); a.close()
            with self.assertRaises(ValueError):
                receive(b)


if __name__ == "__main__":
    unittest.main()
