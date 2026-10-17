# Offline App Server/native terminal fixture; this executes inside Bubblewrap.
import json
import os
import select
import signal
import socket
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path
import termios
import tty
if 'Wire' not in globals():
    from codex_wire import Wire

CLI = '/run/goblins/bin/goblin'
PROMPT = 'Check your Goblins inbox and process the next item.'

def cli(*args):
    return json.loads(subprocess.check_output([CLI, *args], stderr=subprocess.DEVNULL))

def serve(path):
    Path('/tmp/goblins-codex/server-args.json').write_text(json.dumps(sys.argv[1:]))
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(path)
    listener.listen()
    clients = []
    lock = threading.RLock()
    state = {'thread': str(uuid.uuid4()), 'status': {'type': 'idle'}, 'queue': [], 'items': []}
    def send(c, value):
        wire, write_lock = c
        try:
            with write_lock: wire.send(value)
        except (OSError, EOFError): pass
    def event(method, params):
        for c in clients[:]: send(c, {'method': method, 'params': params})
    def status(value):
        state['status'] = value
        event('thread/status/changed', {'threadId': state['thread'], 'status': value})
    def start(text, client_id):
        status({'type': 'active', 'activeFlags': []})
        item = {'type': 'userMessage', 'id': str(uuid.uuid4()), 'clientId': client_id, 'content': [{'type': 'text', 'text': text}]}
        state['items'].append({'item': item})
        event('item/started', {'threadId': state['thread'], 'item': item})
        def work():
            outcome = 'completed'
            if text in ('busy', 'long-busy'): time.sleep(2 if text == 'busy' else 8)
            elif text in ('failed', 'interrupted'): outcome = text
            elif text == 'notifier-fail':
                for path in Path('/proc').glob('[0-9]*/cmdline'):
                    try:
                        if b'goblins-codex\x00run\x00' in path.read_bytes(): os.kill(int(path.parent.name), signal.SIGTERM)
                    except (OSError, ValueError): pass
            elif text == PROMPT:
                while True:
                    item = cli('inbox', 'next')
                    if item['message'] is None: break
                    args = [item['message']['id'], '--claim-generation', str(item['claim_generation'])]
                    if item['message'].get('operation'): cli('inbox', 'complete', *args)
                    else: cli('reply', *args, '--message', 'processed: ' + item['message']['body'])
            else: raise RuntimeError('unexpected human text: ' + repr(text))
            with lock:
                event('turn/completed', {'threadId': state['thread'], 'turn': {'id': str(uuid.uuid4()), 'status': outcome}})
                status({'type': 'idle'})
                schedule()
        threading.Thread(target=work, daemon=True).start()
        return {'turn': {'id': str(uuid.uuid4()), 'status': 'inProgress'}}
    def schedule():
        if state['status']['type'] == 'idle' and state['queue']:
            q = state['queue'].pop(0)
            start(q['input'][0]['text'], q['clientUserMessageId'])
    def client(peer):
        c = (Wire(peer, False), threading.Lock())
        clients.append(c)
        try:
            while True:
                request = c[0].receive()
                method, params = request['method'], request.get('params', {})
                if 'id' not in request: continue
                with lock:
                    error = None
                    if method == 'initialize': result = {'userAgent': 'fake-codex/0.162.1'}
                    elif method == 'thread/start':
                        subprocess.run(['/run/goblins/bin/goblins-codex', 'hook'], input=json.dumps({'hook_event_name': 'SessionStart', 'session_id': state['thread']}).encode(), stdout=subprocess.DEVNULL, check=True)
                        result = {'thread': {'id': state['thread'], 'status': state['status']}}
                    elif method == 'config/read': result = {'config': {'approval_policy': None}}
                    elif method == 'thread/resume' and params.get('threadId') != state['thread'] and not state['items']:
                        # The native terminal resuming a recorded thread, with
                        # the permissions the adapter set.
                        Path('/tmp/goblins-codex/resume-params.json').write_text(json.dumps(params))
                        state['thread'] = params['threadId']
                        result = {'thread': {'id': state['thread'], 'status': state['status'], 'name': 'Resumed thread'}}
                    elif method in ('thread/read', 'thread/resume'): result = {'thread': {'id': state['thread'], 'status': state['status'], 'cwd': os.getcwd()}}
                    elif method == 'thread/unsubscribe': result = {}
                    elif method == 'thread/items/list': result = {'data': list(reversed(state['items'])), 'nextCursor': None}
                    elif method == 'thread/queue/list': result = {'data': state['queue'][:], 'nextCursor': None}
                    elif method == 'thread/queue/add':
                        q = {'id': str(uuid.uuid4()), 'clientUserMessageId': params['clientUserMessageId'], 'input': params['input']}
                        state['queue'].append(q)
                        result = {'queuedSubmission': q}
                    elif method == 'thread/queue/start':
                        if state['status']['type'] != 'idle' or not state['queue']: error = {'code': -32600, 'message': 'thread already has an active or pending turn'}
                        result = {}
                    elif method == 'turn/start': result = start(params['input'][0]['text'], params.get('clientUserMessageId'))
                    else: raise RuntimeError('unexpected method: ' + method)
                    send(c, {'id': request['id'], **({'error': error} if error else {'result': result})})
                    schedule()
        except (OSError, EOFError): pass
        finally:
            clients.remove(c)
            c[0].close()
    while True:
        peer, _ = listener.accept()
        threading.Thread(target=client, args=(peer,), daemon=True).start()

if '--version' in sys.argv:
    print('codex-cli 0.162.1')
    sys.exit(0)
if sys.argv[1:2] == ['app-server']:
    serve(sys.argv[sys.argv.index('--listen') + 1].removeprefix('unix://'))
    sys.exit(0)

wire = Wire.connect(sys.argv[sys.argv.index('--remote') + 1].removeprefix('unix://'))
wire.send({'id': 1, 'method': 'initialize', 'params': {'clientInfo': {'name': 'native', 'version': '0.1'}}})
while wire.receive().get('id') != 1: pass
wire.send({'method': 'initialized'})
Path('/tmp/goblins-codex/native-args.json').write_text(json.dumps(sys.argv[1:]))
if 'resume' in sys.argv:
    wire.send({'id': 2, 'method': 'thread/resume', 'params': {'threadId': sys.argv[sys.argv.index('resume') + 1]}})
else:
    wire.send({'id': 2, 'method': 'thread/start', 'params': {}})
while (result := wire.receive()).get('id') != 2: pass
thread = result['result']['thread']['id']
draft = b''
active = False
connected = True
serial = 2
write_lock = threading.Lock()
def redraw():
    Path('/tmp/goblins-codex/native-status.json').write_text(json.dumps({'active':active,'draft':draft.decode(errors='replace')}))
    with write_lock:
        sys.stdout.write('\x1b[2J\x1b[H' + ('Working (esc to interrupt)\x1b[?25l' if active else '› ' + draft.decode(errors='replace') + '\x1b[?25h'))
        sys.stdout.flush()
def events():
    global active, connected
    while True:
        try: v = wire.receive()
        except (EOFError, OSError):
            connected = False
            active = False
            redraw()
            return
        view = v.get('params', {}).get('status') if v.get('method') == 'thread/status/changed' else v.get('result', {}).get('thread', {}).get('status')
        if view:
            active = view['type'] == 'active'
            redraw()
threading.Thread(target=events, daemon=True).start()
old = termios.tcgetattr(0)
tty.setraw(0)
try:
    redraw()
    while True:
        if not select.select([0], [], [], .5)[0]:
            if not connected: continue
            serial += 1
            try: wire.send({'id':serial,'method':'thread/read','params':{'threadId':thread,'includeTurns':False}})
            except OSError: connected = False
            continue
        data = os.read(0, 4096)
        if not data: break
        draft += data
        draft = draft.replace(b'\x1b[I', b'').replace(b'\x1b[O', b'')
        if b'\r' in draft:
            submitted, draft = draft.split(b'\r', 1)
            text = submitted.replace(b'\x1b[200~', b'').replace(b'\x1b[201~', b'').decode()
            if text == 'exit': break
            serial += 1
            wire.send({'id': serial, 'method': 'turn/start', 'params': {'threadId': thread, 'input': [{'type': 'text', 'text': text}]}})
        redraw()
finally:
    wire.close()
    termios.tcsetattr(0, termios.TCSANOW, old)
