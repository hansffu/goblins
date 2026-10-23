"""Isolated Codex 0.162.1 protocol smoke tests; synthetic auth, local provider."""
import json
import struct
import zlib
import uuid
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from codex_wire import Wire

class Client:
    def __init__(self, socket, name):
        self.wire = Wire.connect(socket)
        self.next = 0
        self.events = []
        self.call('initialize', {'clientInfo': {'name': name, 'version': '0.1'}, 'capabilities': {'experimentalApi': True}})
        self.wire.send({'method': 'initialized'})
    def call(self, method, params, request_id=None):
        self.next += 1
        id = self.next if request_id is None else request_id
        self.wire.send({'id': id, 'method': method, 'params': params})
        while True:
            v = self.wire.receive()
            if 'method' not in v and v.get('id') == id:
                if 'error' in v: raise ValueError(v['error'])
                return v['result']
            self.events.append(v)
    def event(self, method, predicate=lambda _: True, envelope=False):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            for i, v in enumerate(self.events):
                if v.get('method') == method and predicate(v['params']):
                    v = self.events.pop(i)
                    return v if envelope else v['params']
            self.events.append(self.wire.receive())
        raise AssertionError('missing event: ' + method)

@unittest.skipUnless(shutil.which('codex'), 'native Codex unavailable')
class NativeProtocol(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='goblins-native-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {k:v for k,v in os.environ.items() if k not in ('OPENAI_API_KEY','CODEX_API_KEY','ACCESS_TOKEN','CODEX_AUTH_JSON')}
        self.env['CODEX_HOME'] = str(self.root)
        # Terminal supplies a PTY and emulates ANSI/xterm responses. Do not
        # inherit TERM=dumb from a headless runner and trigger Codex's prompt.
        self.env['TERM'] = 'xterm-256color'
        target = Path(os.environ.get('CARGO_TARGET_DIR', 'target'))
        self.adapter = Path(os.environ.get('GOBLINS_CODEX_ADAPTER',
            Path(__file__).resolve().parents[1]/target/'debug/goblins-codex')).resolve()
        self.scripts = queue.Queue()
        self.inputs = []
        scripts, inputs = self.scripts, self.inputs
        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_POST(self):
                inputs.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                script = scripts.get(timeout=15)
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                try:
                    for event in script:
                        if isinstance(event, threading.Event): event.wait(10); continue
                        self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode()); self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError): pass
        self.provider = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        threading.Thread(target=self.provider.serve_forever, daemon=True).start()
        self.addCleanup(self.provider.server_close)
        self.addCleanup(self.provider.shutdown)
        (self.root/'auth.json').write_text('{"OPENAI_API_KEY":"synthetic-local-test"}')
        (self.root/'hook.py').write_text("import json,sys,pathlib; v=json.load(sys.stdin); pathlib.Path(__file__).with_name('hook-thread.json').write_text(json.dumps(v)); print('{}')")
        (self.root/'config.toml').write_text(f'''
model="fixture-model"
model_provider="scripted"
thread_unload_delay_secs=0
[features]
hooks=true
[projects."{self.root}"]
trust_level="trusted"
[hooks]
SessionStart=[{{hooks=[{{type="command",command="{shutil.which('python3')} {self.root}/hook.py",timeout=5}}]}}]
[model_providers.scripted]
name="scripted"
base_url="http://127.0.0.1:{self.provider.server_port}/v1"
wire_api="responses"
supports_websockets=false
requires_openai_auth=false
''')
        self.addCleanup(self.stop)
        self.start_server()
        self.owner = self.client('native-terminal')
        self.observer = self.client('goblins-observer')
        self.thread = self.owner.call('thread/start', {'cwd': str(self.root), 'approvalPolicy': 'on-request', 'sandbox': 'read-only', 'model': 'fixture-model'})['thread']['id']
    def start_server(self):
        log = (self.root/'log').open('a'); self.addCleanup(log.close)
        self.server = subprocess.Popen(['codex', 'app-server', '--listen', 'unix://'+str(self.root/'server.sock')], cwd=self.root, env=self.env, stdout=subprocess.DEVNULL, stderr=log)
        for _ in range(1200):
            if (self.root/'server.sock').exists(): break
            if self.server.poll() is not None: self.fail((self.root/'log').read_text())
            time.sleep(.025)
        self.assertTrue((self.root/'server.sock').exists(), (self.root/'log').read_text())
    def restart_server(self):
        """A fresh server, which has no thread loaded."""
        self.stop()
        (self.root/'server.sock').unlink(missing_ok=True)
        self.start_server()
    def client(self, name):
        c = Client(self.root/'server.sock', name); c.wire.peer.settimeout(15)
        self.addCleanup(c.wire.close)
        return c
    def start_adapter(self, request_socket=None, bypass=False, name='adapter'):
        binary = self.adapter
        self.assertTrue(binary.exists(), 'build goblins-codex before native proxy smoke tests')
        runtime = self.root/name
        runtime.mkdir(mode=0o700)
        log = (runtime/'log').open('w'); self.addCleanup(log.close)
        # Never the real sandbox endpoint, which these tests may run inside.
        daemon = ['--request-socket', str(request_socket or self.root/'no-daemon.sock')]
        mode = ['--bypass-permissions'] if bypass else []
        adapter = subprocess.Popen([str(binary),'--runtime-dir',str(runtime),*daemon,'run','--socket',str(self.root/'server.sock'),'--native-socket',str(runtime/'native.sock'),*mode],cwd=self.root,stdout=subprocess.DEVNULL,stderr=log)
        def stop():
            adapter.terminate(); adapter.wait(timeout=5)
        self.addCleanup(stop)
        for _ in range(100):
            if (runtime/'native.sock').exists(): break
            time.sleep(.025)
        return runtime
    def stop(self):
        self.server.terminate(); self.server.wait(timeout=10)
    def script(self, *items):
        id = str(uuid.uuid4())
        events = [{'type':'response.created','response':{'id':id}}]
        events += list(items)
        events += [{'type':'response.output_item.done','item':{'type':'message','role':'assistant','id':str(uuid.uuid4()),'content':[{'type':'output_text','text':'done'}]}}]
        events += [{'type':'response.completed','response':{'id':id,'usage':{'input_tokens':0,'output_tokens':0,'total_tokens':0}}}]
        self.scripts.put(events)
    def subscribe(self):
        deadline = time.monotonic() + 5
        while True:
            try:
                return self.observer.call('thread/resume', {'threadId': self.thread, 'excludeTurns': True})
            except ValueError:
                if time.monotonic() >= deadline: raise
                time.sleep(.1)
    def add(self, id):
        return self.observer.call('thread/queue/add', {'threadId': self.thread, 'clientUserMessageId': id, 'input': [{'type': 'text', 'text': 'fixed reminder'}]})
    def test_busy_idle_reconnect_history_and_observer_events(self):
        gate = threading.Event()
        self.script(gate)
        self.owner.call('turn/start', {'threadId': self.thread, 'input': [{'type': 'text', 'text': 'human task'}]})
        self.subscribe()
        q = self.add('busy-reminder')['queuedSubmission']
        self.assertEqual(self.observer.call('thread/queue/list', {'threadId': self.thread})['data'][0]['id'], q['id'])
        self.script()
        gate.set()
        self.observer.event('item/started', lambda p: p.get('item', {}).get('clientId') == 'busy-reminder')
        self.observer.event('turn/completed')
        again = self.client('reconnected-observer')
        self.assertEqual(again.call('thread/queue/list', {'threadId': self.thread})['data'], [])
        items = again.call('thread/items/list', {'threadId': self.thread, 'sortDirection': 'desc'})
        self.assertIn('busy-reminder', json.dumps(items))
        self.script()
        self.add('idle-reminder')
        self.observer.event('item/started', lambda p: p.get('item', {}).get('clientId') == 'idle-reminder')
    def test_interruption_leaves_queued_inbox_work_eligible(self):
        gate = threading.Event()
        self.script(gate)
        turn = self.owner.call('turn/start', {'threadId':self.thread,'input':[{'type':'text','text':'interrupt human work'}]})['turn']['id']
        self.subscribe()
        self.add('after-interruption')
        self.script()
        self.owner.call('turn/interrupt', {'threadId':self.thread,'turnId':turn})
        self.owner.event('turn/completed',lambda p:p['turn']['status']=='interrupted')
        gate.set()
        queued = self.observer.call('thread/queue/list',{'threadId':self.thread})['data']
        if queued:
            try:
                self.observer.call('thread/queue/start',{'threadId':self.thread,'queuedSubmissionId':queued[0]['id']})
            except ValueError as error:
                self.assertIn('active or pending',str(error))
        self.observer.event('item/started',lambda p:p.get('item',{}).get('clientId')=='after-interruption')

    def test_profile_hook_preferences_preserve_native_trust_checks(self):
        import tomllib
        base = tomllib.loads((self.root/'config.toml').read_text())
        profile = self.root/'fixture.config.toml'
        profile.write_text('[hooks]\nSessionStart=[{hooks=[{type="command",command='+json.dumps(base['hooks']['SessionStart'][0]['hooks'][0]['command'])+',timeout=5}]}]\n')
        hook = self.owner.call('hooks/list', {'cwds':[str(self.root)]})['data'][0]['hooks'][0]
        key = str(profile)+':'+hook['key'].split(':', 1)[1]
        self.owner.call('config/batchWrite', {'edits':[{'keyPath':'hooks.state.'+json.dumps(key)+'.trusted_hash','value':hook['currentHash'],'mergeStrategy':'replace'}],'reloadUserConfig':True})
        binary = self.adapter
        for changed in (False, True):
            if changed:
                profile.write_text('[hooks]\nSessionStart=[{hooks=[{type="command",command="/bin/false",timeout=5}]}]\n')
            values = subprocess.check_output([str(binary),'profile-args','fixture'], env=self.env).decode().split('\0')[:-1]
            args = [part for value in values for part in ('-c', value)]
            socket = self.root/('changed.sock' if changed else 'profile.sock')
            process = subprocess.Popen(['codex','app-server','--listen','unix://'+str(socket),*args],env=self.env,cwd=self.root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            def stop(process=process):
                process.terminate()
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired: process.kill(); process.wait()
            self.addCleanup(stop)
            for _ in range(200):
                if socket.exists(): break
                time.sleep(.025)
            server = Client(socket, 'profile-observer'); self.addCleanup(server.wire.close)
            entries = server.call('hooks/list', {'cwds':[str(self.root)]})['data'][0]['hooks']
            entry = next(h for h in entries if h['key'].startswith('/<session-flags>/config.toml:'))
            self.assertEqual(entry['trustStatus'], 'modified' if changed else 'trusted', (entry, values))

    def test_hidden_native_threads_do_not_replace_visible_binding(self):
        runtime = self.start_adapter()
        native = Client(runtime/'native.sock', 'native-terminal'); self.addCleanup(native.wire.close)
        visible = native.call('thread/start', {'cwd':str(self.root)}, request_id='startup-thread-start-test')['thread']['id']
        self.assertEqual(json.loads((runtime/'thread.json').read_text()), visible)
        hidden = native.call('thread/start', {'cwd':str(self.root),'ephemeral':True,'threadSource':'thread_title'}, request_id='temporary-structured-test')['thread']['id']
        native.call('thread/unsubscribe', {'threadId':hidden})
        deadline = time.monotonic() + 5
        while hidden in (loaded := native.call('thread/loaded/list', {})['data']) and time.monotonic() < deadline:
            time.sleep(.025)
        self.assertIn(visible, loaded)
        self.assertNotIn(hidden, loaded)
        self.assertEqual(native.call('thread/read', {'threadId':visible,'includeTurns':False})['thread']['status']['type'], 'idle')
        background = native.call('thread/start', {'cwd':str(self.root)}, request_id='tui-dynamic-test')['thread']['id']
        self.assertNotEqual(background, visible)
        for file in ('thread.json', 'native-thread.json'):
            self.assertEqual(json.loads((runtime/file).read_text()), visible)
        # An actual foreground clear/switch still binds immediately.
        switched = native.call('thread/start', {'cwd':str(self.root),'sessionStartSource':'clear'})['thread']['id']
        self.assertEqual(json.loads((runtime/'thread.json').read_text()), switched)

    def test_loaded_resume_repairs_and_verifies_effective_permissions(self):
        for bypass, saved_policy, saved_sandbox, expected, busy in (
            (True, 'on-request', 'read-only', 'never', False),
            (False, 'never', 'danger-full-access', 'on-request', False),
            (True, 'on-request', 'read-only', 'never', True),
        ):
            with self.subTest(bypass=bypass, busy=busy):
                thread = self.owner.call('thread/start', {'cwd':str(self.root),
                    'approvalPolicy':saved_policy, 'sandbox':saved_sandbox})['thread']['id']
                gate = threading.Event()
                self.addCleanup(gate.set)
                self.script(*([gate] if busy else []))
                calls = len(self.inputs)+1
                self.owner.call('turn/start', {'threadId':thread,
                    'input':[{'type':'text','text':'materialize a still-loaded conversation'}]})
                self.owner.event('turn/started' if busy else 'turn/completed', lambda p:p['threadId']==thread)
                runtime = self.start_adapter(bypass=bypass, name='loaded-'+str(bypass)+'-'+str(busy))
                native = Client(runtime/'native.sock', 'native-terminal')
                self.addCleanup(native.wire.close)
                resumed = native.call('thread/resume', {'threadId':thread})
                self.assertEqual(resumed['approvalPolicy'], expected)
                self.assertEqual(resumed['sandbox']['type'], 'dangerFullAccess')
                # Check the server's live state too, rather than trusting a
                # proxy-rewritten response that could hide the old settings.
                actual = self.observer.call('thread/resume', {'threadId':thread, 'excludeTurns':True})
                self.assertEqual(actual['approvalPolicy'], expected)
                self.assertEqual(actual['sandbox']['type'], 'dangerFullAccess')
                self.assertEqual(json.loads((runtime/'thread.json').read_text()), thread)
                if busy:
                    self.assertEqual(actual['thread']['status']['type'], 'active')
                    gate.set()
                    self.owner.event('turn/completed', lambda p:p['threadId']==thread)
                self.assertEqual(len(self.inputs), calls, 'policy repair must not submit a turn')

    def test_unrecognized_selection_suspends_delivery_and_warns_without_guessing(self):
        request_socket, reports = self.fake_daemon()
        runtime = self.start_adapter(request_socket)
        native = Client(runtime/'native.sock', 'native-terminal')
        self.addCleanup(native.wire.close)
        visible = native.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.reported(reports, {'session_id':visible, 'session_name':''})
        unknown = native.call('thread/start', {'cwd':str(self.root)}, request_id='future-foreground-id')['thread']['id']
        self.assertNotEqual(visible, unknown)
        self.assertFalse((runtime/'thread.json').exists())
        self.assertEqual(json.loads((runtime/'native-thread.json').read_text()), '')
        warning = native.event('configWarning')
        self.assertIn('automatic inbox delivery is suspended', warning['summary'])
        deadline = time.monotonic()+5
        while time.monotonic()<deadline:
            status = json.loads((runtime/'status.json').read_text())
            if 'cannot identify' in (status.get('diagnostic') or ''): break
            time.sleep(.025)
        self.assertIn('cannot identify', status['diagnostic'])
        self.assertIsNone(status['thread_id'])
        switched = native.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.reported(reports, {'session_id':switched, 'session_name':''})
        self.assertEqual(json.loads((runtime/'thread.json').read_text()), switched)

    def test_resume_picker_connects_while_the_foreground_stays_open(self):
        from terminal_support import Terminal, screen_wait
        self.script()
        self.owner.call('turn/start', {'threadId':self.thread, 'input':[{'type':'text','text':'saved picker conversation'}]})
        self.owner.event('turn/completed')
        self.owner.call('thread/name/set', {'threadId':self.thread, 'name':'Picker target'})
        # Resume a historical conversation, not another client's still-loaded
        # task (whose live permissions Codex deliberately preserves).
        self.owner.call('thread/unsubscribe', {'threadId':self.thread})
        deadline = time.monotonic() + 5
        while self.thread in (loaded := self.owner.call('thread/loaded/list', {})['data']) and time.monotonic() < deadline:
            time.sleep(.025)
        self.assertNotIn(self.thread, loaded)
        request_socket, reports = self.fake_daemon()
        runtime = self.start_adapter(request_socket)
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(runtime/'native.sock'),
            '--dangerously-bypass-hook-trust'],
            cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        screen_wait(terminal, lambda text: 'fixture-model' in text, timeout=30)
        deadline = time.monotonic() + 10
        while not (runtime/'thread.json').exists() and time.monotonic() < deadline:
            time.sleep(.025)
        visible = json.loads((runtime/'thread.json').read_text())
        self.reported(reports, {'session_id':visible, 'session_name':''})
        # Materialize the fresh thread before subscribing: an empty native
        # thread has no saved rollout yet. Hold its response until subscribed.
        gate = threading.Event()
        self.addCleanup(gate.set)
        self.script(gate)
        terminal.send('fresh picker conversation')
        screen_wait(terminal, lambda text: 'fresh picker conversation' in text)
        terminal.send('\r')
        saved = self.thread
        self.thread = visible
        started = self.subscribe()
        self.thread = saved
        gate.set()
        self.observer.event('turn/completed', lambda p:p['threadId']==visible)
        screen_wait(terminal, lambda text: 'Worked for' in text)
        calls = len(self.inputs)
        self.assertEqual((started['sandbox']['type'], started['approvalPolicy']), ('dangerFullAccess', 'on-request'))
        for _ in range(2):
            terminal.send('/resume')
            screen_wait(terminal, lambda text: '/resume' in text)
            terminal.send('\r')
            screen_wait(terminal, lambda text: 'Resume a previous session' in text, timeout=20)
            for file in ('thread.json', 'native-thread.json'):
                self.assertEqual(json.loads((runtime/file).read_text()), visible)
            terminal.send('\x1b')
            screen_wait(terminal, lambda text: 'Resume a previous session' not in text)
            # Closing the picker must not clear the still-open terminal's
            # binding or allow a late SessionStart hook to replace it.
            hook = {'hook_event_name':'SessionStart', 'session_id':self.thread}
            subprocess.run([str(self.adapter), '--runtime-dir', str(runtime), 'hook'],
                input=json.dumps(hook), text=True, check=True)
            for file in ('thread.json', 'native-thread.json'):
                self.assertEqual(json.loads((runtime/file).read_text()), visible)
        terminal.send('/resume')
        screen_wait(terminal, lambda text: '/resume' in text)
        terminal.send('\r')
        screen_wait(terminal, lambda text: 'Resume a previous session' in text and 'Picker target' in text)
        terminal.send('Picker target')
        screen_wait(terminal, lambda text: 'Search: Picker target' in text)
        terminal.send('\r')
        screen_wait(terminal, lambda text: 'Resume a previous session' not in text and 'saved picker conversation' in text)
        self.reported(reports, {'session_id':self.thread, 'session_name':'Picker target'})
        for file in ('thread.json', 'native-thread.json'):
            self.assertEqual(json.loads((runtime/file).read_text()), self.thread)
        resumed = self.observer.call('thread/resume', {'threadId':self.thread, 'excludeTurns':True})
        self.assertEqual((resumed['sandbox']['type'], resumed['approvalPolicy']), ('dangerFullAccess', 'on-request'))
        self.assertEqual(len(self.inputs), calls, 'opening, cancelling and resuming must not call the model')

    def test_native_draft_and_image_survive_queue_arrival(self):
        from terminal_support import Terminal, screen_wait
        image = self.root/'input.png'
        def chunk(kind,data):
            return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
        image.write_bytes(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\xff\0\0'))+chunk(b'IEND',b''))
        hooks = self.owner.call('hooks/list',{'cwds':[str(self.root)]})['data'][0]['hooks']
        self.assertTrue(hooks, 'synthetic SessionStart hook was not discovered')
        edits = [{'keyPath':'hooks.state.'+json.dumps(h['key'])+'.trusted_hash', 'value':h['currentHash'], 'mergeStrategy':'replace'} for h in hooks]
        self.owner.call('config/batchWrite', {'edits':edits,'reloadUserConfig':True})
        runtime = self.start_adapter()
        self.script()
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(runtime/'native.sock'),
            '--dangerously-bypass-hook-trust', '--sandbox', 'danger-full-access', '--ask-for-approval', 'never',
            '-i', str(image), '--', 'image task'], cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        screen_wait(terminal,lambda text: 'image task' in text,timeout=30)
        for _ in range(200):
            if (self.root/'hook-thread.json').exists(): break
            time.sleep(.025)
        self.assertTrue((self.root/'hook-thread.json').exists(),(self.root/'log').read_text())
        hook = json.loads((self.root/'hook-thread.json').read_text())
        self.assertEqual(hook['hook_event_name'],'SessionStart')
        self.assertNotEqual(hook['session_id'],self.thread)
        self.assertEqual(json.loads((runtime/'thread.json').read_text()), hook['session_id'])
        self.thread = hook['session_id']
        self.subscribe()
        self.observer.event('turn/completed',lambda p:p['threadId']==self.thread)
        self.assertIn('input_image',json.dumps(self.inputs))
        terminal.send('my unsent draft')
        screen_wait(terminal,lambda text:'my unsent draft' in text)
        self.script()
        self.add('draft-reminder')
        self.observer.event('turn/completed',lambda p:p['threadId']==self.thread)
        screen_wait(terminal,lambda text:'my unsent draft' in text)

    def fake_daemon(self):
        """A sandbox endpoint that records the adapter's conversation reports."""
        from daemon_support import frame, receive
        import socket
        path = self.root/'daemon.sock'
        listener = socket.socket(socket.AF_UNIX); listener.bind(str(path)); listener.listen()
        self.addCleanup(listener.close)
        reports = queue.Queue()
        self.states = queue.Queue()
        def serve():
            while True:
                try: peer, _ = listener.accept()
                except OSError: return
                with peer:
                    try:
                        init = receive(peer)
                        peer.sendall(frame({'jsonrpc':'2.0','id':init['id'],'result':{'api':1,'role':'sandbox'}}))
                        call = receive(peer)
                        if call['method'] == 'sessions.set-variables':
                            reports.put(call['params']['variables'])
                            peer.sendall(frame({'jsonrpc':'2.0','id':call['id'],'result':{'accepted':True,'recorded':True}}))
                        elif call['method'] == 'sessions.set-state':
                            self.states.put(call['params']['state'])
                            peer.sendall(frame({'jsonrpc':'2.0','id':call['id'],'result':{}}))
                        else:
                            peer.sendall(frame({'jsonrpc':'2.0','id':call['id'],'error':{'code':-32601,'message':'not in this test'}}))
                    except (OSError, EOFError, ValueError): pass
        threading.Thread(target=serve, daemon=True).start()
        return path, reports
    def reported(self, reports, expected, timeout=30):
        deadline = time.monotonic() + timeout
        seen = []
        while time.monotonic() < deadline:
            try: seen.append(reports.get(timeout=.5))
            except queue.Empty: continue
            if seen[-1] == expected: return
        self.fail(f'missing report {expected}; saw {seen}')

    def test_remote_resume_reports_the_conversation_through_rename_and_switch(self):
        from terminal_support import Terminal, screen_wait
        hooks = self.owner.call('hooks/list',{'cwds':[str(self.root)]})['data'][0]['hooks']
        edits = [{'keyPath':'hooks.state.'+json.dumps(h['key'])+'.trusted_hash', 'value':h['currentHash'], 'mergeStrategy':'replace'} for h in hooks]
        self.owner.call('config/batchWrite', {'edits':edits,'reloadUserConfig':True})
        self.script()
        self.owner.call('turn/start', {'threadId': self.thread, 'input': [{'type': 'text', 'text': 'earlier task'}]})
        self.owner.event('turn/completed')
        self.owner.call('thread/name/set', {'threadId': self.thread, 'name': 'Original name'})
        calls = len(self.inputs)
        # Codex refuses permission overrides when resuming a remote thread:
        # the reason the launcher leaves them off.
        refused = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(self.root/'server.sock'), '--sandbox',
                            'danger-full-access', '--ask-for-approval', 'never', 'resume', self.thread],
                           cwd=self.root, env=self.env)
        self.addCleanup(refused.close)
        refused.expect('Permission overrides are not supported', timeout=30)
        self.assertNotEqual(refused.wait(), 0)
        request_socket, reports = self.fake_daemon()
        runtime = self.start_adapter(request_socket)
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(runtime/'native.sock'),
            '--dangerously-bypass-hook-trust', 'resume', self.thread], cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        self.reported(reports, {'session_id': self.thread, 'session_name': 'Original name'})
        self.assertEqual(json.loads((runtime/'thread.json').read_text()), self.thread)
        screen_wait(terminal, lambda text: 'earlier task' in text, timeout=30)
        self.assertEqual(len(self.inputs), calls, 'resuming must not call the model')
        # A rename of the foreground thread reaches the proxied connection.
        self.observer.call('thread/name/set', {'threadId': self.thread, 'name': 'Renamed'})
        self.reported(reports, {'session_id': self.thread, 'session_name': 'Renamed'})
        # A foreground switch replaces both values.
        terminal.send('/new')
        time.sleep(1)
        terminal.send('\r')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            current = json.loads((runtime/'thread.json').read_text()) if (runtime/'thread.json').exists() else None
            if current and current != self.thread: break
            time.sleep(.1)
        self.assertNotEqual(current, self.thread)
        self.reported(reports, {'session_id': current, 'session_name': ''})

    def test_native_owns_approval_and_input_while_observer_does_not_answer(self):
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'touch /tmp/native-approval-test', 'sandbox_permissions': 'require_escalated', 'justification': 'test approval'})}})
        self.owner.call('turn/start', {'threadId': self.thread, 'input': [{'type': 'text', 'text': 'approval task'}]})
        self.subscribe()
        envelope = self.owner.event('item/commandExecution/requestApproval', envelope=True)
        status = self.observer.call('thread/read', {'threadId': self.thread, 'includeTurns': False})['thread']['status']
        self.assertIn('waitingOnApproval', status['activeFlags'])
        self.script()
        self.owner.wire.send({'id': envelope['id'], 'result': {'decision': 'decline'}})
        self.owner.event('turn/completed')
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'input', 'name': 'request_user_input', 'arguments': json.dumps({'questions': [{'id': 'q', 'header': 'Choice', 'question': 'Choose?', 'options': [{'label': 'A', 'description': 'A'}, {'label': 'B', 'description': 'B'}]}]})}})
        self.owner.call('turn/start', {'threadId': self.thread, 'input': [{'type': 'text', 'text': 'input task'}], 'collaborationMode': {'mode': 'plan', 'settings': {'model': 'fixture-model', 'reasoning_effort': None, 'developer_instructions': None}}})
        v = self.owner.event('item/tool/requestUserInput', envelope=True)
        status = self.observer.call('thread/read', {'threadId': self.thread, 'includeTurns': False})['thread']['status']
        self.assertIn('waitingOnUserInput', status['activeFlags'])
        self.script()
        self.owner.wire.send({'id': v['id'], 'result': {'answers': {'q': {'answers': ['A']}}}})
        self.owner.event('turn/completed')

    def saved(self, approval, sandbox, cwd=None):
        """A thread saved with these permissions, not loaded in a fresh server."""
        thread = self.owner.call('thread/start', {'cwd': str(cwd or self.root), 'approvalPolicy': approval, 'sandbox': sandbox, 'model': 'fixture-model'})['thread']['id']
        self.script()
        self.owner.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'saved task'}]})
        self.owner.event('turn/completed')
        self.restart_server()
        return thread

    def test_a_resumed_thread_gets_the_configured_approval_policy_not_its_saved_one(self):
        thread = self.saved('never', 'danger-full-access')
        runtime = self.start_adapter()
        # The terminal's connection, through the adapter.
        terminal = Client(runtime/'native.sock', 'native-terminal'); terminal.wire.peer.settimeout(15)
        self.addCleanup(terminal.wire.close)
        resumed = terminal.call('thread/resume', {'threadId': thread})
        # The config leaves approval_policy unset: Codex's own default.
        self.assertEqual(resumed['approvalPolicy'], 'on-request')
        self.assertEqual(resumed['sandbox']['type'], 'dangerFullAccess')
        # With full access, on-request does not ask even for a command the
        # model wants to escalate: the docs say so, and this notices a change.
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'true', 'sandbox_permissions': 'require_escalated', 'justification': 'test approval'})}})
        self.script()
        terminal.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'escalated task'}]})
        terminal.event('turn/completed')
        self.assertNotIn('item/commandExecution/requestApproval', [e.get('method') for e in terminal.events])
        # A configured policy replaces the default.
        config = self.root/'config.toml'
        config.write_text('approval_policy="never"\n' + config.read_text())
        self.restart_server()
        runtime = self.start_adapter(name='configured')
        terminal = Client(runtime/'native.sock', 'native-terminal'); terminal.wire.peer.settimeout(15)
        self.addCleanup(terminal.wire.close)
        self.assertEqual(terminal.call('thread/resume', {'threadId': thread})['approvalPolicy'], 'never')

    def test_the_configured_policy_is_the_saved_threads_directorys(self):
        # Your config says never; the thread's project says on-request.
        project = self.root/'project'
        (project/'.codex').mkdir(parents=True)
        (project/'.codex'/'config.toml').write_text('approval_policy="on-request"\n')
        config = self.root/'config.toml'
        config.write_text('approval_policy="never"\n' + config.read_text() + f'[projects."{project}"]\ntrust_level="trusted"\n')
        thread = self.saved('never', 'danger-full-access', cwd=project)
        # The adapter runs in the root, whose policy is never.
        runtime = self.start_adapter()
        terminal = Client(runtime/'native.sock', 'native-terminal'); terminal.wire.peer.settimeout(15)
        self.addCleanup(terminal.wire.close)
        resumed = terminal.call('thread/resume', {'threadId': thread})
        self.assertEqual((resumed['cwd'], resumed['approvalPolicy']), (str(project), 'on-request'))
        forked = terminal.call('thread/fork', {'threadId': thread})
        self.assertEqual(forked['approvalPolicy'], 'on-request')

    def test_a_bypassing_goblin_resumes_a_prompting_thread_without_approvals(self):
        from terminal_support import Terminal, screen_wait
        thread = self.saved('on-request', 'danger-full-access')
        owner = self.client('native-terminal')
        hooks = owner.call('hooks/list', {'cwds': [str(self.root)]})['data'][0]['hooks']
        owner.call('config/batchWrite', {'edits': [{'keyPath': 'hooks.state.'+json.dumps(h['key'])+'.trusted_hash', 'value': h['currentHash'], 'mergeStrategy': 'replace'} for h in hooks], 'reloadUserConfig': True})
        runtime = self.start_adapter(bypass=True)
        # Codex's own terminal, which sends no permissions on resume.
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(runtime/'native.sock'), '--dangerously-bypass-hook-trust', 'resume', thread], cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        screen_wait(terminal, lambda text: 'saved task' in text, timeout=30)
        # The thread is loaded now; resuming it again reports its live policy.
        live = self.client('inspector').call('thread/resume', {'threadId': thread, 'excludeTurns': True})
        self.assertEqual(live['approvalPolicy'], 'never')
        self.assertEqual(live['sandbox']['type'], 'dangerFullAccess')

    def test_approvals_reach_the_terminal_through_the_adapter_and_report_waiting(self):
        thread = self.saved('on-request', 'read-only')
        request_socket, _ = self.fake_daemon()
        runtime = self.start_adapter(request_socket)
        terminal = Client(runtime/'native.sock', 'native-terminal'); terminal.wire.peer.settimeout(15)
        self.addCleanup(terminal.wire.close)
        # A policy the request gives itself stays: here one that asks.
        resumed = terminal.call('thread/resume', {'threadId': thread, 'approvalPolicy': 'on-request', 'sandbox': 'read-only'})
        self.assertEqual(resumed['approvalPolicy'], 'on-request')
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'touch /tmp/native-approval-test', 'sandbox_permissions': 'require_escalated', 'justification': 'test approval'})}})
        terminal.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'approval task'}]})
        envelope = terminal.event('item/commandExecution/requestApproval', envelope=True)
        deadline = time.monotonic() + 15
        while self.states.get(timeout=max(deadline - time.monotonic(), .1)) != 'waiting-approval':
            pass
        self.script()
        terminal.wire.send({'id': envelope['id'], 'result': {'decision': 'decline'}})
        terminal.event('turn/completed')

if __name__ == '__main__': unittest.main()
