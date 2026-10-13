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
        log = (self.root/'log').open('w'); self.addCleanup(log.close)
        self.server = subprocess.Popen(['codex', 'app-server', '--listen', 'unix://'+str(self.root/'server.sock')], cwd=self.root, env=self.env, stdout=subprocess.DEVNULL, stderr=log)
        self.addCleanup(self.stop)
        for _ in range(1200):
            if (self.root/'server.sock').exists(): break
            if self.server.poll() is not None: self.fail((self.root/'log').read_text())
            time.sleep(.025)
        self.assertTrue((self.root/'server.sock').exists(), (self.root/'log').read_text())
        self.owner = self.client('native-terminal')
        self.observer = self.client('goblins-observer')
        self.thread = self.owner.call('thread/start', {'cwd': str(self.root), 'approvalPolicy': 'on-request', 'sandbox': 'read-only', 'model': 'fixture-model'})['thread']['id']
    def client(self, name):
        c = Client(self.root/'server.sock', name); c.wire.peer.settimeout(15)
        self.addCleanup(c.wire.close)
        return c
    def start_adapter(self):
        binary = Path(__file__).resolve().parents[1]/'target/debug/goblins-codex'
        self.assertTrue(binary.exists(), 'build goblins-codex before native proxy smoke tests')
        runtime = self.root/'adapter'
        runtime.mkdir(mode=0o700)
        log = (runtime/'log').open('w'); self.addCleanup(log.close)
        adapter = subprocess.Popen([str(binary),'--runtime-dir',str(runtime),'run','--socket',str(self.root/'server.sock'),'--native-socket',str(runtime/'native.sock')],stdout=subprocess.DEVNULL,stderr=log)
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
        binary = Path(__file__).resolve().parents[1]/'target/debug/goblins-codex'
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

if __name__ == '__main__': unittest.main()
