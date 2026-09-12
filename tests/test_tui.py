"""Actual fullscreen PTYs, interpreted with pyte (available in nix develop)."""
import codecs
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import struct
import tempfile
import termios
import time
import unittest
import pyte
from support import command, request
from test_connected import Terminal


class Screen(Terminal):
    def __init__(self, argv, env=None):
        env = {**(env or os.environ), 'TERM': 'xterm-256color', 'COLORTERM': 'truecolor'}
        env.pop('NO_COLOR', None)  # The tool runner disables colors; emulate a normal color terminal.
        super().__init__(argv, env)
        self.screen = pyte.Screen(100, 24)
        self.stream = pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
        self.raw = bytearray()

    def text(self):
        return '\n'.join(self.screen.display)

    def pump(self, timeout=.1):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            self.raw.extend(data)
            self.stream.feed(self.decoder.decode(data))

    def until(self, predicate, timeout=40):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump(.05)
            if predicate(self.text()):
                return self.text()
        raise AssertionError('screen timeout:\n' + self.text())

    def contains(self, text, timeout=40):
        return self.until(lambda s: text in s, timeout)

    def resize(self, cols, rows):
        self.screen.resize(rows, cols)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)


class TuiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory(prefix='goblins-tui-build-')
        cls.app = Path(command(['nix', 'build', '--print-out-paths', '--out-link', Path(cls.build.name) / 'app',
                                'path:' + str(Path(__file__).resolve().parents[1]) + '#goblins'])) / 'bin/goblins'
        cls.binary, cls.config = re.search(r'exec (\S+) --runtime (\S+)', cls.app.read_text()).groups()

    @classmethod
    def tearDownClass(cls):
        cls.build.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='goblins-tui-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / 'control'

    def start(self, env=None, theme=None):
        args = [self.binary, '--runtime', self.config, '--state-dir', str(self.state), 'serve']
        if theme: args += ['--theme', theme]
        self.server = Screen(args, env=env)
        self.addCleanup(self.server.close)
        self.server.contains('No connected sandbox')
        self.client = Terminal([str(self.app), '--state-dir', str(self.state), 'shell'])
        self.addCleanup(self.client.close)
        self.client.expect('workspace[>#]')
        text = self.server.until(lambda s: re.search(r'shell\s+(\d+)\s', s) is not None)
        pid = int(re.search(r'shell\s+(\d+)\s', text).group(1))
        for path in Path('/tmp').glob('goblins-*/events.jsonl'):
            with contextlib.suppress(OSError, ValueError):
                if any(e.get('event') == 'started' and e.get('fields', {}).get('pid') == pid
                       for e in map(json.loads, path.read_text().splitlines())):
                    self.directory = path.parent
                    break
        else: self.fail('no session event record')

    def peer(self, **fields):
        peer = socket.socket(socket.AF_UNIX)
        self.addCleanup(peer.close)
        peer.settimeout(90)
        peer.connect(str(self.directory / 'request.sock'))
        peer.sendall((json.dumps(request(**fields)) + '\n').encode())
        return peer

    def test_layout_preview_keys_mouse_colors_and_live_grant(self):
        self.start()
        self.assertIn(b'\x1b[?1049h', self.server.raw)
        self.assertFalse(termios.tcgetattr(self.server.fd)[3] & termios.ICANON)
        first = self.server.text()
        self.server.send('\x1b[F')
        self.server.until(lambda s: s != first)
        self.server.send('\x1b[H')
        self.server.contains('Startup')
        # The terminal's own ANSI palette is used, not a forced RGB background.
        self.assertTrue({'cyan', '00cdcd'} & {cell.fg for row in self.server.screen.buffer.values() for cell in row.values()})
        idle_table_rows = sum('Startup / Connected' in l for l in self.server.screen.display)
        peer = self.peer(package='hello', id='ui-hello', reason='A clear reason for the project')
        screen = self.server.contains('In host store: Yes')
        self.assertIn('Download: 0 B', screen)
        self.assertIn('Sandbox: shell', screen)
        self.assertIn('Package: hello', screen)
        self.assertIn('Reason: A clear reason', screen)
        self.assertIn('Yes [y]', screen)
        self.assertIn('No [n]', screen)
        Path('/tmp/goblins-tui-request.txt').write_text(screen)
        Path('/tmp/goblins-tui-request-cells.json').write_text(json.dumps([[{'text': cell.data, 'fg': cell.fg, 'bg': cell.bg, 'reverse': cell.reverse, 'bold': cell.bold} for cell in (self.server.screen.buffer[y][x] for x in range(100))] for y in range(24)]))
        request_row = next(i for i,l in enumerate(self.server.screen.display) if 'Package request' in l)
        self.assertTrue(all(i < request_row for i,l in enumerate(self.server.screen.display) if 'Startup / Approval' in l))
        self.assertLess(sum('Startup / Approval' in l for l in self.server.screen.display), idle_table_rows)
        # A duplicate public ID cannot dismiss the popup for another request.
        invalid = self.peer(package='hello^out', id='ui-hello')
        self.assertEqual(json.loads(invalid.recv(4096))['status'], 'error')
        self.server.send('\x1b[200~y\x1b[201~')
        self.server.pump(.3)
        self.assertFalse(select.select([peer], [], [], .15)[0], 'paste approved a request')
        self.assertIn('Package request', self.server.text())
        self.server.send('n')
        self.assertEqual(json.loads(peer.recv(4096))['status'], 'denied')
        self.server.until(lambda s: 'Package request' not in s)

        # Real remote metadata estimate, with no package download before consent.
        flake = json.loads(Path(self.config).read_text())['goblins']['shell']['flake']
        missing = None
        for candidate in ('figlet', 'toilet', 'sl'):
            path = Path(command(['nix', 'eval', '--raw', flake + '#legacyPackages.x86_64-linux.' + candidate + '.outPath']))
            if not path.exists(): missing = candidate, path; break
        self.assertIsNotNone(missing, 'cost test needs an uncached package')
        package, output = missing
        peer = self.peer(package=package, id='ui-missing', reason='Preview the transfer size')
        screen = self.server.until(lambda s: 'In host store: No' in s and re.search(r'Download: [\d.]+ [KMGT]?i?B', s) is not None, 90)
        self.assertFalse(output.exists(), 'preview realized a package before approval')
        # Click the No button using the coordinates of its rendered label.
        row, line = next((i,l) for i,l in enumerate(self.server.screen.display) if 'No [n]' in l)
        column = line.index('No [n]')
        self.server.send(f'\x1b[<0;{column+1};{row+1}M\x1b[<0;{column+1};{row+1}m')
        self.assertEqual(json.loads(peer.recv(4096))['status'], 'denied')
        self.server.until(lambda s: 'Package request' not in s)
        peer = self.peer(package='hello', id='ui-long', reason='Escaped control: \x1b[2J ' + 'long reason ' * 60 + 'END OF REASON')
        self.server.contains('Package: hello')
        self.server.resize(70, 18)
        self.server.contains('No [n]')
        self.server.send('\x1b[6~' * 30)
        self.server.contains('END OF REASON')
        self.assertIn('Yes [y]', self.server.text())
        self.assertIn('No [n]', self.server.text())
        self.server.send('\r')  # No is initially selected; Enter must deny.
        self.assertEqual(json.loads(peer.recv(4096))['status'], 'denied')
        self.server.until(lambda s: 'Package request' not in s)
        self.server.resize(100, 24)
        peer = self.peer(package='hello', id='ui-grant', reason='Live grant from fullscreen UI')
        self.server.contains('Package: hello')
        self.server.send('y')
        self.assertEqual(json.loads(peer.recv(4096))['status'], 'ready')
        self.server.send('\x1b[F')
        self.server.until(lambda s: re.search(r'hello\s+Granted / Connected', s) is not None)
        self.client.send('hello\n')
        self.client.expect(r'(?:^|\n)Hello, world!\n')
        self.server.resize(70, 18)
        self.server.contains('q quit')
        self.server.resize(100, 24)
        self.server.contains('q quit')
        Path('/tmp/goblins-tui-screen.txt').write_text(self.server.text())
        self.server.send('q')
        self.assertEqual(self.server.wait(), 0)
        self.server.pump()
        self.assertIn(b'\x1b[?1049l', self.server.raw)
        self.assertTrue(termios.tcgetattr(self.server.fd)[3] & termios.ICANON)
        self.assertEqual(self.client.wait(), 0)
        print('EVIDENCE fullscreen: scrolling, reserved popup layout, ANSI colors, cached/missing preview, paste rejection, y/n, mouse denial, live hello, resize and tty restoration', flush=True)

    def test_onedark_and_preview_cancellation(self):
        import shutil, sys
        fake = self.root / 'bin'; fake.mkdir()
        marker = self.root / 'preview'
        real_nix = shutil.which('nix')
        script = fake / 'nix'
        script.write_text(f'''#!{sys.executable}
import os,subprocess,sys,time,json
if 'eval' in sys.argv and 'false' in sys.argv:
 child=subprocess.Popen([{shutil.which('sleep')!r},'60'])
 open({str(marker)!r},'w').write(json.dumps([os.getpid(),child.pid]))
 time.sleep(60)
else: os.execv({real_nix!r},[{real_nix!r},*sys.argv[1:]])
''')
        script.chmod(0o700)
        self.start(env={**os.environ, 'PATH': str(fake) + ':' + os.environ['PATH']}, theme='onedark')
        self.assertIn('61afef', {cell.fg for row in self.server.screen.buffer.values() for cell in row.values()})
        for action in ('deny', 'disconnect', 'quit'):
            peer = self.peer(package='hello', id=action, reason='Cancel a slow preview')
            self.server.contains('Checking...')
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline: time.sleep(.02)
            self.assertTrue(marker.exists())
            pids = json.loads(marker.read_text())
            start = time.monotonic()
            if action == 'deny':
                self.server.send('n')
                self.assertEqual(json.loads(peer.recv(4096))['status'], 'denied')
                self.server.until(lambda s: 'Package request' not in s, 2)
            elif action == 'disconnect':
                peer.close()
                self.server.until(lambda s: 'Package request' not in s, 2)
            else:
                self.server.send('q')
                self.assertEqual(self.server.wait(), 0)
            self.assertLess(time.monotonic() - start, 2)
            deadline = time.monotonic() + 2
            while Path(f'/proc/{pids[0]}').exists() and time.monotonic() < deadline: time.sleep(.02)
            self.assertFalse(Path(f'/proc/{pids[0]}').exists(), 'preview client not reaped')
            for pid in pids:
                path = Path(f'/proc/{pid}/stat')
                self.assertTrue(not path.exists() or path.read_text().split()[2] == 'Z')
            marker.unlink()
        self.assertEqual(self.client.wait(), 0)
        print('EVIDENCE OneDark RGB colors; deny, request disconnect and quit cancel/reap preview without blocking UI', flush=True)
