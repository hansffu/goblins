"""Fast PTY regressions for the terminal harness, without a sandbox or Fish."""
import os
import pty
import select
import termios
import time
import unittest

from terminal_support import Terminal


class TerminalSupportTests(unittest.TestCase):
    def test_echoed_query_replies_do_not_hide_a_line_marker(self):
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        self.addCleanup(os.close, slave)
        settings = termios.tcgetattr(slave)
        settings[3] |= termios.ICANON | termios.ECHO | termios.ECHOCTL
        raw = list(settings)
        raw[3] &= ~(termios.ICANON | termios.ECHO)
        termios.tcsetattr(slave, termios.TCSANOW, raw)
        terminal = Terminal.__new__(Terminal)
        terminal.fd, terminal.buffer, terminal.query_tail = master, b"", b""
        # A query was emitted while raw, but its reply arrives once the shell
        # has restored cooked mode. read_output itself supplies both replies.
        os.write(slave, b"\x1b[0c\x1b[6n")
        termios.tcsetattr(slave, termios.TCSANOW, settings)
        self.assertTrue(terminal.read_output())

        def read_through(marker):
            deadline = time.monotonic() + 1
            while marker not in terminal.buffer:
                remaining = max(0, deadline - time.monotonic())
                self.assertTrue(select.select([master], [], [], remaining)[0], terminal.buffer)
                self.assertTrue(terminal.read_output())

        # The fake program consumes both replies before printing its marker.
        # Noncanonical reads release the replies without adding an input newline;
        # ECHO/ECHOCTL stay enabled to reproduce the actual caret-text prefix.
        settings[3] &= ~termios.ICANON
        termios.tcsetattr(slave, termios.TCSANOW, settings)
        replies = b"\x1b[?1;2c\x1b[1;1R"
        received = b""
        deadline = time.monotonic() + 1
        while len(received) < len(replies):
            ready = select.select([slave, master], [], [], max(0, deadline - time.monotonic()))[0]
            self.assertTrue(ready, (received, terminal.buffer))
            if master in ready:
                self.assertTrue(terminal.read_output())
            if slave in ready:
                chunk = os.read(slave, len(replies) - len(received))
                self.assertTrue(chunk)
                received += chunk
        self.assertEqual(received, replies)
        # Also observe the actual echo before writing from the other PTY end.
        read_through(b"^[[?1;2c^[[1;1R")
        os.write(slave, b"RESTRICTED=2\n")
        read_through(b"RESTRICTED=2\r\n")
        # This is kernel-generated ECHOCTL text, not a fabricated output log.
        self.assertIn(b"^[[?1;2c^[[1;1RRESTRICTED=2\r\n", terminal.buffer)
        terminal.expect(r"(?:^|\n)RESTRICTED=2\n", timeout=0)

    def test_query_reply_normalization_keeps_marker_boundaries_and_status(self):
        for marker in (b"NOT_RESTRICTED=2\n", b"RESTRICTED=1\n"):
            with self.subTest(marker=marker):
                terminal = Terminal.__new__(Terminal)
                terminal.buffer = b"^[[?1;2c^[[1;1R" + marker
                with self.assertRaisesRegex(AssertionError, "timeout waiting"):
                    terminal.expect(r"(?:^|\n)RESTRICTED=2\n", timeout=0)

    def test_printable_output_including_other_caret_sequences_is_preserved(self):
        terminal = Terminal.__new__(Terminal)
        ordinary = b"literal ^[[?1;3c and ^[[2;1R text"
        terminal.buffer = ordinary + b"\nRESTRICTED=2\n"
        match = terminal.expect(r"^(literal .*)\n", timeout=0)
        self.assertEqual(match.group(1), ordinary)
        terminal.expect(r"^RESTRICTED=2\n", timeout=0)
