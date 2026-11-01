"""Deterministic input framing checks for the offline Codex terminal."""
import ast
from pathlib import Path
from types import SimpleNamespace
import unittest


class FakeCodexTests(unittest.TestCase):
    def test_coalesced_submissions_and_exit_need_no_further_input(self):
        # The fixture is an executable that connects on import. Execute its
        # actual terminal loop with controlled reads, without an App Server.
        source = ast.parse(Path(__file__).with_name("fake_codex.py").read_text())
        loop, = [node for node in source.body if isinstance(node, ast.Try)]
        code = compile(ast.Module(body=[loop], type_ignores=[]), "fake-codex-input", "exec")
        for chunks in ([b"failed\rinterrupted\rexit\r"],
                       [b"failed\rinterrupted\r", b"exit\r"],
                       [b"fai", b"led\rinter", b"rupted\rex", b"it\r"]):
            with self.subTest(chunks=chunks):
                remaining = iter(chunks)
                sent, closed, restored = [], [], []

                def read(*_):
                    return next(remaining)  # Extra reads fail instead of hiding buffered input.

                namespace = dict(
                    draft=b"", connected=True, serial=2, thread="probe", old="saved",
                    redraw=lambda: None,
                    os=SimpleNamespace(read=read),
                    select=SimpleNamespace(select=lambda *_: ([0], [], [])),
                    wire=SimpleNamespace(send=sent.append, close=lambda: closed.append(True)),
                    termios=SimpleNamespace(TCSANOW=0, tcsetattr=lambda *args: restored.append(args)),
                )
                exec(code, namespace)
                self.assertEqual([v["params"]["input"][0]["text"] for v in sent],
                                 ["failed", "interrupted"])
                self.assertEqual(namespace["draft"], b"")
                self.assertEqual(closed, [True])
                self.assertEqual(restored, [(0, 0, "saved")])
