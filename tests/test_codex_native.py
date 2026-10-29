"""Isolated Codex protocol smoke tests for the goblins-codex adapter and the
native terminal as a client of the goblin's App Server; synthetic auth, local
provider. A fake sandbox endpoint stands in for the daemon and its attachment."""
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
    def start_adapter(self, request_socket=None, bypass=False, name='adapter', thread=None, wait=True):
        """goblins-codex on its own connection, as the goblin's server
        launcher runs it. Returns its runtime directory and process."""
        binary = self.adapter
        self.assertTrue(binary.exists(), 'build goblins-codex before native smoke tests')
        runtime = self.root/name
        runtime.mkdir(mode=0o700)
        log = (runtime/'log').open('w'); self.addCleanup(log.close)
        # Never the real sandbox endpoint, which these tests may run inside.
        daemon = ['--request-socket', str(request_socket or self.root/'no-daemon.sock')]
        mode = ['--bypass-permissions'] if bypass else []
        resume = ['--thread', thread] if thread else []
        adapter = subprocess.Popen([str(binary),'--runtime-dir',str(runtime),*daemon,'run','--socket',str(self.root/'server.sock'),*resume,*mode],cwd=self.root,stdout=subprocess.DEVNULL,stderr=log)
        def stop():
            if adapter.poll() is None:
                adapter.terminate(); adapter.wait(timeout=5)
        self.addCleanup(stop)
        if wait:
            self.bound(runtime)
        return runtime, adapter
    def bound(self, runtime, thread=None, timeout=15):
        """The conversation the adapter delivers to, once it is `thread`."""
        deadline = time.monotonic() + timeout
        status = None
        while time.monotonic() < deadline:
            try: status = json.loads((runtime/'status.json').read_text())
            except (OSError, ValueError): status = None
            if status and status['thread_id'] and (thread is None or status['thread_id'] == thread):
                return status['thread_id']
            time.sleep(.05)
        self.fail(f'adapter not bound to {thread}: {status}; log: {(runtime/"log").read_text()}')
    def stays(self, runtime, thread, seconds=2):
        """The adapter keeps delivering to `thread` for a while."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.assertEqual(json.loads((runtime/'status.json').read_text())['thread_id'], thread)
            time.sleep(.1)
    def policy(self, thread):
        """The thread's live approval policy and sandbox, from the server."""
        live = self.client('inspector-'+uuid.uuid4().hex).call('thread/resume', {'threadId': thread, 'excludeTurns': True})
        return live['approvalPolicy'], live['sandbox']['type']
    def trust_hooks(self):
        """Trust this configuration's own hook, as you would once in Codex."""
        owner = self.client('trust')
        hooks = owner.call('hooks/list', {'cwds': [str(self.root)]})['data'][0]['hooks']
        owner.call('config/batchWrite', {'edits': [{'keyPath': 'hooks.state.'+json.dumps(h['key'])+'.trusted_hash', 'value': h['currentHash'], 'mergeStrategy': 'replace'} for h in hooks], 'reloadUserConfig': True})
    def turn(self, client, thread, text='human task', **params):
        """A user message in `thread` and the turn it starts, completed."""
        self.script()
        client.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': text}], **params})
        client.event('turn/completed', lambda p: p['threadId'] == thread)
    def stop(self):
        self.server.terminate()
        try: self.server.wait(timeout=10)
        except subprocess.TimeoutExpired: self.server.kill(); self.server.wait()
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

    def fake_daemon(self):
        """A sandbox endpoint that records the adapter's conversation reports.
        `self.attached_since` is the attachment it reports, None when detached,
        and `self.inbox` what inbox.status returns."""
        from daemon_support import frame, receive
        import socket
        path = self.root/'daemon.sock'
        listener = socket.socket(socket.AF_UNIX); listener.bind(str(path)); listener.listen()
        self.addCleanup(listener.close)
        reports = queue.Queue()
        self.states = queue.Queue()
        self.attached_since = None
        self.status_held, self.reported_since = False, None
        self.inbox = {'queued': 0, 'claim': None, 'head': None, 'audit_failed': False}
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
                        elif call['method'] == 'sessions.status':
                            if not self.status_held: self.reported_since = self.attached_since
                            since = self.reported_since
                            peer.sendall(frame({'jsonrpc':'2.0','id':call['id'],'result':{'agent_name':'gobbo','attached':since is not None,'attached_since':since}}))
                        elif call['method'] == 'inbox.status':
                            peer.sendall(frame({'jsonrpc':'2.0','id':call['id'],'result':self.inbox}))
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
    def attach(self, held=False):
        """A client attaches now: messages from here on count. While `held`,
        the endpoint keeps reporting the previous attachment."""
        self.status_held = held
        self.attached_since = int(time.time() * 1000)
        if not held:
            time.sleep(1.2)  # the adapter polls twice a second

    def test_a_fresh_conversation_is_named_and_a_terminal_can_join_it_before_any_turn(self):
        from terminal_support import Terminal, screen_wait
        request_socket, reports = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        thread = self.bound(runtime)
        # Named, so saved: recorded at once, and another connection resumes it.
        self.reported(reports, {'session_id': thread, 'session_name': 'gobbo'})
        self.assertEqual(self.policy(thread), ('on-request', 'dangerFullAccess'))
        # Codex's own terminal, as onAttach starts it, shows it normally.
        self.trust_hooks()
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(self.root/'server.sock'), 'resume', thread], cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        screen_wait(terminal, lambda text: 'fixture-model' in text and 'rror' not in text, timeout=30)
        # Quitting the terminal, or hanging it up, leaves the server and the thread.
        terminal.send('/quit')
        time.sleep(.5)
        terminal.send('\r')
        self.assertEqual(terminal.wait(), 0)
        again = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(self.root/'server.sock'), 'resume', thread], cwd=self.root, env=self.env)
        self.addCleanup(again.close)
        screen_wait(again, lambda text: 'fixture-model' in text and 'rror' not in text, timeout=30)
        again.close()
        time.sleep(1)
        self.assertIn(thread, self.observer.call('thread/loaded/list', {})['data'])
        self.assertEqual(json.loads((runtime/'status.json').read_text())['thread_id'], thread)

    def test_a_terminal_resuming_the_conversation_draws_its_history(self):
        from terminal_support import Terminal, screen_wait
        thread = self.saved('on-request', 'danger-full-access')
        runtime, _ = self.start_adapter(thread=thread)
        self.assertEqual(self.bound(runtime), thread)
        self.trust_hooks()
        terminal = Terminal([shutil.which('codex'), '--remote', 'unix://'+str(self.root/'server.sock'), 'resume', thread], cwd=self.root, env=self.env)
        self.addCleanup(terminal.close)
        screen_wait(terminal, lambda text: 'saved task' in text, timeout=30)

    def test_a_recorded_conversation_gets_the_goblins_policy_and_a_missing_one_fails(self):
        thread = self.saved('never', 'read-only')
        runtime, _ = self.start_adapter(thread=thread)
        self.assertEqual(self.bound(runtime), thread)
        # The config leaves approval_policy unset: Codex's own default.
        self.assertEqual(self.policy(thread), ('on-request', 'dangerFullAccess'))
        # Bypassing always means never.
        self.restart_server()
        runtime, _ = self.start_adapter(thread=thread, bypass=True, name='bypass')
        self.assertEqual(self.policy(self.bound(runtime)), ('never', 'dangerFullAccess'))
        # A conversation that no longer exists stops the adapter, and so the goblin.
        runtime, adapter = self.start_adapter(thread=str(uuid.uuid4()), name='missing', wait=False)
        self.assertNotEqual(adapter.wait(timeout=15), 0)
        self.assertIn('goblins-codex:', (runtime/'log').read_text())

    def test_the_configured_policy_is_the_saved_threads_directorys(self):
        # Your config says never; the thread's project says on-request.
        project = self.root/'project'
        (project/'.codex').mkdir(parents=True)
        (project/'.codex'/'config.toml').write_text('approval_policy="on-request"\n')
        config = self.root/'config.toml'
        config.write_text('approval_policy="never"\n' + config.read_text() + f'[projects."{project}"]\ntrust_level="trusted"\n')
        thread = self.saved('never', 'danger-full-access', cwd=project)
        runtime, _ = self.start_adapter(thread=thread)
        self.assertEqual(self.policy(self.bound(runtime)), ('on-request', 'dangerFullAccess'))

    def test_a_loaded_thread_that_keeps_its_own_policy_is_repaired_without_a_turn(self):
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        original = self.bound(runtime)
        for busy in (False, True):
            with self.subTest(busy=busy):
                self.attach()
                thread = self.owner.call('thread/start', {'cwd':str(self.root), 'approvalPolicy':'never', 'sandbox':'read-only'})['thread']['id']
                gate = threading.Event()
                self.addCleanup(gate.set)
                self.script(*([gate] if busy else []))
                calls = len(self.inputs) + 1
                self.owner.call('turn/start', {'threadId':thread, 'input':[{'type':'text','text':'switch here'}]})
                self.owner.event('turn/started' if busy else 'turn/completed', lambda p:p['threadId']==thread)
                self.bound(runtime, thread)
                self.assertEqual(self.policy(thread), ('on-request', 'dangerFullAccess'))
                gate.set()
                if busy: self.owner.event('turn/completed', lambda p:p['threadId']==thread)
                self.assertEqual(len(self.inputs), calls, 'policy repair must not submit a turn')
        self.assertNotEqual(original, thread)

    def test_only_the_first_message_while_attached_moves_reminders(self):
        request_socket, reports = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        first = self.bound(runtime)
        # While detached, a new conversation and its messages never move them.
        detached = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, detached)
        self.stays(runtime, first)
        # Attached: switching alone does not move them; the first message does,
        # also when it is the turn that saves a new conversation.
        self.attach()
        # Nor do a background task's messages, also in a saved thread.
        background = self.owner.call('thread/start', {'cwd':str(self.root), 'threadSource':'thread_title'})['thread']['id']
        self.turn(self.owner, background, text='title this')
        self.stays(runtime, first)
        switched = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.stays(runtime, first, seconds=1.5)
        self.turn(self.owner, switched)
        self.bound(runtime, switched)
        self.reported(reports, {'session_id': switched, 'session_name': ''})
        # Old history, from before the attachment, does not count either.
        self.owner.call('thread/resume', {'threadId': detached})
        self.stays(runtime, switched)

    def test_a_first_turn_that_ends_before_it_can_be_joined_is_caught_up(self):
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        self.bound(runtime)
        self.attach()
        # The turn that saves the conversation ends before the adapter's next
        # poll can join it. (An interrupted first turn keeps no message.)
        thread = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, thread, text='quick')
        self.bound(runtime, thread)

    def test_a_first_message_before_the_adapter_learns_of_the_attachment_counts(self):
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        first = self.bound(runtime)
        # A conversation the adapter has joined, with history from while detached.
        other = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, other, text='while detached')
        self.stays(runtime, first)
        # The terminal attaches and sends at once, while the adapter still
        # believes nobody is attached.
        self.attach(held=True)
        self.turn(self.owner, other, text='first while attached')
        self.stays(runtime, first, seconds=1)
        self.assertIsNone(self.reported_since, 'the message must arrive before the attachment is reported')
        self.status_held = False
        self.bound(runtime, other)

    def test_the_rejoin_sequence_follows_each_first_message(self):
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        self.bound(runtime)
        self.attach()
        a = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, a)
        self.bound(runtime, a)
        b = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, b)
        self.bound(runtime, b)
        # Back to A, still loaded: no signal until the next message in it.
        self.owner.call('thread/resume', {'threadId': a})
        self.stays(runtime, b)
        self.turn(self.owner, a, text='back again')
        self.bound(runtime, a)

    def test_resuming_an_unloaded_conversation_while_attached(self):
        request_socket, _ = self.fake_daemon()
        thread = self.saved('on-request', 'danger-full-access')
        runtime, _ = self.start_adapter(request_socket)
        first = self.bound(runtime)
        self.attach()
        terminal = self.client('native-terminal')
        terminal.call('thread/resume', {'threadId': thread})
        self.stays(runtime, first, seconds=1.5)
        self.turn(terminal, thread, text='continue')
        self.bound(runtime, thread)
        self.assertEqual(self.policy(thread), ('on-request', 'dangerFullAccess'))

    def test_reminders_never_move_the_binding_even_once_completed(self):
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket)
        home = self.bound(runtime)
        self.attach()
        terminal = self.client('native-terminal')
        terminal.call('thread/resume', {'threadId': home})
        # The person is busy in the bound conversation when a message arrives:
        # the reminder waits in its queue.
        gate = threading.Event()
        self.addCleanup(gate.set)
        self.script(gate)
        terminal.call('turn/start', {'threadId': home, 'input': [{'type': 'text', 'text': 'long task'}]})
        terminal.event('turn/started', lambda p: p['threadId'] == home)
        self.bound(runtime, home)
        self.inbox = {'queued': 1, 'claim': None, 'head': ['m', 0], 'audit_failed': False}
        deadline = time.monotonic() + 10
        while not self.observer.call('thread/queue/list', {'threadId': home})['data']:
            self.assertLess(time.monotonic(), deadline, 'no reminder queued')
            time.sleep(.1)
        # Then they move to another conversation, and the reminder's turn runs
        # in the old one after the person's turn there ends.
        other = self.owner.call('thread/start', {'cwd':str(self.root)})['thread']['id']
        self.turn(self.owner, other)
        self.bound(runtime, other)
        self.script()
        gate.set()
        reminder = terminal.event('item/started', lambda p: p['threadId'] == home and p['item'].get('type') == 'userMessage' and p['item'].get('clientId'))
        self.assertEqual(reminder['item']['content'][0]['text'], 'Check your Goblins inbox and process the next item.')
        terminal.event('turn/completed', lambda p: p['threadId'] == home)
        self.inbox = {'queued': 0, 'claim': None, 'head': None, 'audit_failed': False}
        self.stays(runtime, other)
        # Rebinding by hand.
        subprocess.run([str(self.adapter), '--runtime-dir', str(runtime), 'bind', home], check=True, stdout=subprocess.DEVNULL)
        self.bound(runtime, home)

    def test_a_pending_approval_reaches_a_terminal_that_attaches_later(self):
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'touch /tmp/native-approval-test', 'sandbox_permissions': 'require_escalated', 'justification': 'test approval'})}})
        self.owner.call('turn/start', {'threadId': self.thread, 'input': [{'type': 'text', 'text': 'approval task'}]})
        self.owner.event('item/commandExecution/requestApproval', envelope=True)
        late = self.client('late-terminal')
        late.call('thread/resume', {'threadId': self.thread})
        envelope = late.event('item/commandExecution/requestApproval', envelope=True)
        self.script()
        late.wire.send({'id': envelope['id'], 'result': {'decision': 'decline'}})
        late.event('turn/completed')

    def test_approvals_reach_the_terminal_and_report_waiting(self):
        thread = self.saved('on-request', 'read-only')
        request_socket, _ = self.fake_daemon()
        runtime, _ = self.start_adapter(request_socket, thread=thread)
        self.bound(runtime, thread)
        terminal = self.client('native-terminal')
        terminal.call('thread/resume', {'threadId': thread})
        self.script({'type': 'response.output_item.done', 'item': {'type': 'function_call', 'call_id': 'cmd', 'name': 'exec_command', 'arguments': json.dumps({'cmd': 'touch /tmp/native-approval-test', 'sandbox_permissions': 'require_escalated', 'justification': 'test approval'})}})
        # The person's own turn may ask for a stricter policy, here one that asks.
        terminal.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'approval task'}], 'approvalPolicy': 'on-request', 'sandboxPolicy': {'type': 'readOnly'}})
        envelope = terminal.event('item/commandExecution/requestApproval', envelope=True)
        deadline = time.monotonic() + 15
        while self.states.get(timeout=max(deadline - time.monotonic(), .1)) != 'waiting-approval':
            pass
        self.script()
        terminal.wire.send({'id': envelope['id'], 'result': {'decision': 'decline'}})
        terminal.event('turn/completed')

if __name__ == '__main__': unittest.main()
