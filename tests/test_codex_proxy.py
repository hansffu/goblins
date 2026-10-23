"""Offline transport regressions with a mock WebSocket server and real adapter."""
import json
import os
from pathlib import Path
import queue
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest

from codex_wire import Wire


class ProxyTransport(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='goblins-proxy-')
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.runtime = self.root/'runtime'
        self.runtime.mkdir(mode=0o700)
        target = Path(os.environ.get('CARGO_TARGET_DIR', 'target'))
        self.binary = Path(os.environ.get('GOBLINS_CODEX_ADAPTER',
            Path(__file__).resolve().parents[1]/target/'debug/goblins-codex')).resolve()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.root/'upstream.sock'))
        self.listener.listen()
        self.listener.settimeout(10)
        self.addCleanup(self.listener.close)
        self.errors = queue.Queue()
        self.workers = []
        self.peers = []
        self.addCleanup(self.stop)

    def stop(self):
        if hasattr(self, 'process'):
            self.process.terminate()
            self.process.wait(timeout=5)
        for peer in self.peers:
            try: peer.shutdown(socket.SHUT_RDWR)
            except OSError: pass
        for worker in self.workers:
            worker.join(timeout=10)
            self.assertFalse(worker.is_alive(), 'proxy fixture worker leaked')
        if not self.errors.empty():
            raise self.errors.get()

    def start(self, serve):
        def server():
            try:
                peer, _ = self.listener.accept()
                self.peers.append(peer)
                peer.settimeout(15)
                wire = Wire(peer, False)
                try: serve(wire)
                finally: wire.close()
            except (EOFError, BrokenPipeError, ConnectionResetError): pass
            except Exception as error: self.errors.put(error)
        worker = threading.Thread(target=server)
        worker.start()
        self.workers.append(worker)
        log = (self.root/'adapter.log').open('w')
        self.addCleanup(log.close)
        self.process = subprocess.Popen([str(self.binary), '--runtime-dir', str(self.runtime),
            '--request-socket', str(self.root/'no-daemon.sock'), 'run',
            '--socket', str(self.root/'upstream.sock'), '--native-socket', str(self.runtime/'native.sock')],
            stdout=log, stderr=log)
        deadline = time.monotonic()+5
        while not (self.runtime/'native.sock').exists() and time.monotonic()<deadline:
            self.assertIsNone(self.process.poll(), (self.root/'adapter.log').read_text())
            time.sleep(.01)
        peer = socket.socket(socket.AF_UNIX)
        peer.settimeout(10)
        peer.connect(str(self.runtime/'native.sock'))
        wire = Wire(peer, True)
        self.addCleanup(wire.close)
        return wire

    def status(self, predicate):
        deadline = time.monotonic()+5
        while time.monotonic()<deadline:
            path = self.runtime/'status.json'
            value = json.loads(path.read_text()) if path.exists() else {}
            if predicate(value): return value
            time.sleep(.01)
        self.fail('missing proxy status: '+str(value))

    def test_notification_burst_has_no_per_message_polling_delay(self):
        def serve(wire):
            wire.receive()
            for n in range(500):
                wire.send({'method':'fixture/event', 'params':{'sequence':n}})
            wire.receive()
        wire = self.start(serve)
        wire.send({'id':1, 'method':'fixture/burst', 'params':{}})
        started = time.monotonic()
        for n in range(500):
            self.assertEqual(wire.receive()['params']['sequence'], n)
        # The former 10 ms alternating reads require >5 seconds regardless of
        # CPU speed. Allow ample overhead without allowing that fixed throttle.
        self.assertLess(time.monotonic()-started, 2)

    def test_paused_reader_preserves_frames_and_reverse_direction(self):
        sent = threading.Event()
        reverse = threading.Event()
        padding = 'x'*(8 << 20)
        def serve(wire):
            wire.receive()
            wire.send({'id':1, 'result':{'padding':padding}})
            sent.set()
            self.assertEqual(wire.receive()['id'], 2)
            reverse.set()
            wire.send({'id':2, 'result':{'alive':True}})
            wire.receive()
        wire = self.start(serve)
        wire.send({'id':1, 'method':'fixture/large', 'params':{}})
        self.assertTrue(sent.wait(5))
        wire.send({'id':2, 'method':'fixture/reverse', 'params':{}})
        self.assertTrue(reverse.wait(2), 'a blocked writer stopped reverse traffic')
        # Exceed both stages of the former blocking write timeout. The adapter
        # must retain the partial frame while the other direction stays usable.
        time.sleep(7)
        self.assertEqual(wire.receive(), {'id':1, 'result':{'padding':padding}})
        self.assertEqual(wire.receive(), {'id':2, 'result':{'alive':True}})
        self.assertNotIn('native proxy:', (self.root/'adapter.log').read_text())

    def test_protocol_failure_reaches_close_reason_and_status(self):
        def serve(wire):
            wire.receive()
            wire.peer.sendall(b'\x81\x01\xff')  # invalid UTF-8 text frame
            wire.receive()
        wire = self.start(serve)
        wire.send({'id':1, 'method':'fixture/bad-frame', 'params':{}})
        header = wire.file.read(2)
        self.assertEqual(header[0] & 15, 8)
        size = header[1] & 127
        self.assertLess(size, 126)
        body = wire.file.read(size)
        self.assertEqual(struct.unpack('>H', body[:2])[0], 1011)
        self.assertIn('Goblins proxy:', body[2:].decode())
        status = self.status(lambda value:'native connection' in (value.get('diagnostic') or ''))
        self.assertIn('UTF-8', status['diagnostic'])

    def test_rejected_policy_repair_keeps_the_previous_selection_and_reports_error(self):
        def serve(wire):
            start = wire.receive()
            wire.send({'id':start['id'], 'result':{'thread':{'id':'original'},
                'approvalPolicy':'never', 'sandbox':{'type':'dangerFullAccess'}}})
            resume = wire.receive()
            wire.send({'id':resume['id'], 'result':{'thread':{'id':'loaded'},
                'approvalPolicy':'on-request', 'sandbox':{'type':'readOnly'}}})
            peer, _ = self.listener.accept()
            self.peers.append(peer)
            peer.settimeout(10)
            helper = Wire(peer, False)
            try:
                initialize = helper.receive()
                self.assertEqual(initialize['method'], 'initialize')
                helper.send({'id':initialize['id'], 'result':{}})
                self.assertEqual(helper.receive()['method'], 'initialized')
                update = helper.receive()
                self.assertEqual(update['method'], 'thread/settings/update')
                self.assertEqual(update['params'], {'threadId':'loaded',
                    'approvalPolicy':'never', 'sandboxPolicy':{'type':'dangerFullAccess'}})
                helper.send({'id':update['id'], 'error':{'code':-32601, 'message':'settings update unsupported'}})
            finally: helper.close()
            probe = wire.receive()
            self.assertEqual(probe['method'], 'fixture/still-alive')
            wire.send({'id':probe['id'], 'result':{}})
            wire.receive()
        wire = self.start(serve)
        wire.send({'id':1, 'method':'thread/start', 'params':{'approvalPolicy':'never'}})
        self.assertEqual(wire.receive()['result']['thread']['id'], 'original')
        wire.send({'id':2, 'method':'thread/resume', 'params':{'threadId':'loaded', 'approvalPolicy':'never'}})
        response = wire.receive()
        self.assertEqual(response['id'], 2)
        self.assertNotIn('result', response)
        self.assertIn('settings update unsupported', response['error']['message'])
        self.assertEqual(json.loads((self.runtime/'thread.json').read_text()), 'original')
        self.status(lambda value:'settings update unsupported' in (value.get('diagnostic') or ''))
        wire.send({'id':3, 'method':'fixture/still-alive', 'params':{}})
        self.assertEqual(wire.receive(), {'id':3, 'result':{}})
