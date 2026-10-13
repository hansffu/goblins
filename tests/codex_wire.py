"""Small RFC6455 test client/server for Codex's explicit Unix socket transport."""
import base64
import hashlib
import json
import os
import socket
import struct

class Wire:
    def __init__(self, peer, client):
        self.peer = peer
        self.client = client
        self.file = peer.makefile('rb')
        if client:
            key = base64.b64encode(os.urandom(16)).decode()
            peer.sendall(('GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ' + key + '\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
            assert b'101' in self.file.readline()
            while self.file.readline() != b'\r\n': pass
        else:
            self.file.readline()
            headers = {}
            while (line := self.file.readline()) != b'\r\n':
                k, v = line.decode().split(':', 1)
                headers[k.lower()] = v.strip()
            accept = base64.b64encode(hashlib.sha1((headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            peer.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n').encode())
    @classmethod
    def connect(cls, path):
        peer = socket.socket(socket.AF_UNIX)
        peer.connect(str(path))
        return cls(peer, True)
    def send(self, value):
        body = json.dumps(value).encode()
        n = len(body)
        header = bytes([0x81, (0x80 if self.client else 0) | (n if n < 126 else 126 if n < 65536 else 127)])
        if n >= 126:
            header += struct.pack('>H' if n < 65536 else '>Q', n)
        if self.client:
            mask = os.urandom(4)
            header += mask
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(body))
        self.peer.sendall(header + body)
    def receive(self):
        h = self.file.read(2)
        if not h: raise EOFError()
        n = h[1] & 127
        if n == 126: n = struct.unpack('>H', self.file.read(2))[0]
        if n == 127: n = struct.unpack('>Q', self.file.read(8))[0]
        mask = self.file.read(4) if h[1] & 128 else None
        body = self.file.read(n)
        if mask: body = bytes(b ^ mask[i % 4] for i, b in enumerate(body))
        if h[0] & 15 == 8: raise EOFError()
        return json.loads(body)
    def close(self):
        try: self.peer.shutdown(socket.SHUT_RDWR)
        except OSError: pass
        self.peer.close()
        self.file.close()
