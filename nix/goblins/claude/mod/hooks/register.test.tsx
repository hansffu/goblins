import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const PLUGIN = 'goblins-daemon'
const HELPER = /goblins-claude-message$/
const CLAUDE = /\/bin\/goblins-claude$/
const IDLE = JSON.stringify({ hook_event_name: 'Notification', notification_type: 'idle_prompt' })
const GOBLIN = '/run/goblins/bin/goblin'

const reply = {
  id: 'm2',
  conversation: 'c1',
  sender: 's2',
  recipient: 's1',
  sender_name: 'reviewer',
  recipient_name: 'skreek',
  in_reply_to: 'm1',
  body: 'Looks good.\nOne nit in hook.sh.',
  created_sequence: 2,
  created_at: Date.UTC(2026, 9, 24, 10, 30),
  state: 'queued',
  claim_generation: 0,
  failure: null,
}

const notice = {
  ...reply,
  id: 'm3',
  sender: 'goblins',
  sender_name: 'goblins',
  in_reply_to: null,
  body: 'Operation finished.',
  operation: { request: 'r1', kind: 'docker', subject: 'goblins', status: 'ready', message: 'Docker is attached' },
}

const wakeText = (task: string, id?: string) =>
  `<task-notification>\n<task-id>${task}</task-id>\n<summary>Monitor event: "Goblins daemon inbox messages"</summary>\n` +
  `<event>Check your Goblins inbox and process the next item. [goblins-inbox-wake${id ? ' ' + id : ''}]</event>\n</task-notification>`

// Every line of the one monitor task arrives as a row of its own.
const TASK = 'monitor-task'

const row = (uuid: string, extra = {}) =>
  ({
    plugin: PLUGIN,
    component: 'UserMessage',
    requestId: uuid,
    props: {
      text: 'Monitor event: "Goblins daemon inbox messages"',
      origin: { kind: 'task-notification' },
      isExpanded: false,
      task: { id: TASK },
      ...extra,
    },
  }) as const

const append = (uuid: string, text: string, origin = { kind: 'task-notification' }) =>
  ({
    message: { type: 'user', role: 'user', content: [{ type: 'text', text }] },
    door: 'prompt',
    origin,
    uuid,
  }) as never

const ran = (stdout: string, exitCode = 0) => ({
  value: { exitCode, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false },
})

// The daemon beneath the plugin: `goblin`, the message helper and
// goblins-claude, whose status is `status.integration`. A call's input
// follows its arguments.
function daemon(on: On, status: Record<string, unknown>, mail: Record<string, unknown>[]) {
  const calls: string[][] = []
  on('process.run', (_, e) => {
    calls.push(e.init?.stdin === undefined ? [...e.argv] : [...e.argv, e.init.stdin])
    if (HELPER.test(e.argv[0] ?? '')) {
      const found = mail.find(m => m.id === e.argv[1])
      return found ? ran(JSON.stringify(found)) : ran('', 1)
    }
    if (e.argv[0] === GOBLIN && e.argv[1] === 'inbox') return ran(JSON.stringify(status))
    if (CLAUDE.test(e.argv[0] ?? '') && e.argv[1] === 'status') return ran(JSON.stringify(status.integration))
    if (CLAUDE.test(e.argv[0] ?? '') && e.argv[1] === 'hook') return ran('{}')
    return ran('', 127)
  })
  return calls
}

function engine(on: On, conversation: string | (() => string) = 'conv-a') {
  const opened: unknown[] = []
  on('session.id', () => ({ value: typeof conversation === 'string' ? conversation : conversation() }))
  on('command.register', (_, e) => ({ value: { command: e.name } }))
  on('ui.open', (_, e) => {
    opened.push(e)
    return { value: { isPlaced: true } }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.end', (_, e) => ({ sessionId: e.sessionId }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  return opened
}

const STATUS = {
  queued: 1,
  claim: null,
  head: ['m2', 0],
  revision: 7,
  integration: { health: 'healthy' },
}

test('a wake row names the sender and opens the message in a pane', async ($, on) => {
  const clock = mock.clock(on, { now: 0 })
  mock.store(on)
  daemon(on, STATUS, [reply])
  const opened = engine(on)
  await $.session.append(append('row-1', wakeText(TASK, 'm2')))
  await clock.settle()
  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...row('row-1'), surface })
    const open = await ui.find({ key: 'open' })
    expect(open?.text).toContain('✉ reviewer replied')
    expect(open?.text).toContain('Looks good. One nit in hook.sh.')
    await ui.press({ key: 'open' })
    await ui.unmount()
  }
  expect(opened).toHaveLength(2)
  expect(opened[0]).toEqual(expect.objectContaining({ id: 'goblins-inbox', focus: true, closeOnEscape: true }))
  const pane = await $.ui.mount(PANE)
  expect((await pane.find({ type: 'Markdown' }))?.text).toBe('Looks good.\nOne nit in hook.sh.')
  expect((await pane.find({ type: 'Text', text: /reviewer · \d\d:\d\d · queued · reply/ })) !== undefined).toBe(true)
  await pane.unmount()
})

test('two lines of the one monitor task keep their own messages', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on)
  const calls = daemon(on, { ...STATUS, head: ['m3', 0] }, [reply, notice])
  const opened = engine(on)
  await $.session.append(append('row-a', wakeText(TASK, 'm2')))
  await $.session.append(append('row-b', wakeText(TASK, 'm3')))
  await clock.settle()
  const b = await $.ui.mount({ ...row('row-b'), surface: 'terminal' })
  expect((await b.find({ key: 'open' }))?.text).toContain('✉ Goblins: docker goblins ready · Docker is attached')
  await b.unmount()
  const a = await $.ui.mount({ ...row('row-a'), surface: 'terminal' })
  expect((await a.find({ key: 'open' }))?.text).toContain('✉ reviewer replied')
  await a.press({ key: 'open' })
  await a.unmount()
  expect(opened).toHaveLength(1)
  // The pane opened on row A's message, which it refetches for its state.
  expect(calls.filter(call => HELPER.test(call[0] ?? '')).map(call => call[1])).toEqual(['m2', 'm3', 'm2'])
})

test('only the exact monitor notification names a row', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on)
  const calls = daemon(on, STATUS, [reply])
  engine(on)
  // A person quoting the marker, another monitor, and a near miss.
  await $.session.append(append('typed', 'see [goblins-inbox-wake m2]', { kind: 'composer' }))
  await $.session.append(append('other', wakeText(TASK, 'm2').replace('Goblins daemon inbox messages', 'builds')))
  await $.session.append(append('near', wakeText(TASK, 'm2').replace('next item.', 'next item!')))
  await clock.settle()
  expect(calls.filter(call => HELPER.test(call[0] ?? ''))).toEqual([])
  const ui = await $.ui.mount({ ...row('near'), surface: 'terminal' })
  expect((await ui.find({ key: 'open' }))?.text).toBe('✉ Goblins inbox')
  await ui.unmount()
})

const STORED = {
  'conversation:conv-a': {
    at: 1,
    wakes: { 'row-old': { id: 'm2', at: 1 } },
    messages: {
      m2: { id: 'm2', sender: 's2', senderName: 'reviewer', inReplyTo: 'm1', body: 'Old news', createdAt: 1, state: 'completed', operation: null, raid: null },
    },
  },
}

const PANE = {
  plugin: PLUGIN,
  surface: 'terminal',
  component: 'Pane',
  requestId: 'goblins-inbox',
  props: {
    title: '✉ Goblins inbox',
    isFocused: true,
    bodyColumns: 60,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 20 },
    view: {},
  },
} as const

test('a resumed conversation names its rows from the store', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on, STORED)
  daemon(on, { ...STATUS, queued: 0, head: null }, [])
  engine(on, 'conv-a')
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  const ui = await $.ui.mount({ ...row('row-old'), surface: 'terminal' })
  expect((await ui.find({ key: 'open' }))?.text).toContain('✉ reviewer replied · Old news')
  await ui.unmount()
})

test('another conversation sharing the store never sees those messages', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on, STORED)
  const calls = daemon(on, { ...STATUS, queued: 0, head: null }, [])
  engine(on, 'conv-b')
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  const ui = await $.ui.mount({ ...row('row-old'), surface: 'terminal' })
  expect((await ui.find({ key: 'open' }))?.text).toBe('✉ Goblins inbox')
  await ui.unmount()
  await $.command.run({ command: 'inbox', args: '', origin: { kind: 'composer' }, presentation: { isFullscreen: false, columns: 100 } } as never)
  await clock.settle()
  const pane = await $.ui.mount(PANE)
  expect((await pane.find({ type: 'Text', text: /No Goblins messages in this conversation yet/ })) !== undefined).toBe(true)
  expect((await pane.find({ type: 'Markdown' })) === undefined).toBe(true)
  await pane.unmount()
  expect(calls.filter(call => HELPER.test(call[0] ?? ''))).toEqual([])
})

test('the pane reads a message afresh when it is selected after its state changed', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on)
  const first = { ...reply, id: 'mA', body: 'first', created_at: 1000 }
  const second = { ...reply, id: 'mB', body: 'second', created_at: 2000 }
  const status = { ...STATUS, head: ['mA', 0], revision: 1 }
  daemon(on, status, [first, second])
  engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  await $.session.append(append('row-a', wakeText(TASK, 'mA')))
  await $.session.append(append('row-b', wakeText(TASK, 'mB')))
  await clock.settle()
  // B is open while A is completed and that revision is observed.
  const b = await $.ui.mount({ ...row('row-b'), surface: 'terminal' })
  await b.press({ key: 'open' })
  await b.unmount()
  first.state = 'completed'
  status.revision = 2
  status.head = ['mB', 0]
  await clock.advance(2000)
  const pane = await $.ui.mount(PANE)
  expect((await pane.find({ type: 'Markdown' }))?.text).toBe('second')
  await pane.press({ key: 'mA' })
  await clock.settle()
  expect((await pane.find({ type: 'Markdown' }))?.text).toBe('first')
  expect((await pane.find({ type: 'Text', text: /reviewer · \d\d:\d\d · completed/ })) !== undefined).toBe(true)
  await pane.unmount()
})

test('rows the mod does not know, and expanded rows, stay as the engine draws them', async ($, on) => {
  mock.clock(on)
  daemon(on, STATUS, [])
  engine(on)
  on('ui.render', ($, e) => {
    const { Text } = $.ui.resolve(e)
    return <Text>engine</Text>
  })
  const unknown = await $.ui.mount({ ...row('row-9'), surface: 'terminal' })
  expect((await unknown.find({ key: 'open' }))?.text).toContain('✉ Goblins inbox')
  await unknown.unmount()
  const expanded = await $.ui.mount({ ...row('row-9', { isExpanded: true }), surface: 'terminal' })
  expect((await expanded.find({ type: 'Text' }))?.text).toBe('engine')
  await expanded.unmount()
  const other = await $.ui.mount({ ...row('row-9', { text: 'Monitor event: "builds"' }), surface: 'terminal' })
  expect((await other.find({ type: 'Text' }))?.text).toBe('engine')
  await other.unmount()
  const failure = await $.ui.mount({ ...row('row-9', {
    text: 'Monitor "Goblins daemon inbox messages" failed with exit code 1',
  }), surface: 'terminal' })
  expect((await failure.find({ type: 'Text' }))?.text).toBe('engine')
  await failure.unmount()
})

test('the footer counts waiting messages and names a paused integration', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on)
  const status = { ...STATUS, queued: 2 }
  daemon(on, status, [reply])
  engine(on)
  on('ui.render', ($, e) => {
    const { Text } = $.ui.resolve(e)
    return <Text>{(e.props as { modes: string[] }).modes.join(' & ')}</Text>
  })
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  const mode = {
    plugin: PLUGIN,
    surface: 'terminal',
    component: 'SessionMode',
    requestId: 'mode',
    props: { modes: ['focus'] },
  } as const
  let ui = await $.ui.mount(mode)
  expect((await ui.find({ type: 'Text' }))?.text).toBe('focus & ✉ 2')
  await ui.unmount()
  status.queued = 0
  status.integration = { health: 'paused' }
  await clock.advance(2000)
  ui = await $.ui.mount(mode)
  expect((await ui.find({ type: 'Text' }))?.text).toBe('focus & ✉ paused')
  await ui.unmount()
  status.integration = { health: 'healthy' }
  await clock.advance(2000)
  ui = await $.ui.mount(mode)
  expect((await ui.find({ type: 'Text' }))?.text).toBe('focus')
  await ui.unmount()
})

test('an interrupted turn ends only after its idle report', async ($, on) => {
  const clock = mock.clock(on)
  const order: string[] = []
  on('process.run', async (_, e) => {
    if (CLAUDE.test(e.argv[0] ?? '') && e.argv[1] === 'hook') {
      await clock.sleep(1000)
      order.push('idle report')
    }
    return ran('{}')
  })
  engine(on)
  const ended = $.turn.complete({ answer: '', durationMs: 1, turnId: 'x', isAborted: true, reason: 'aborted' }).then(() =>
    order.push('turn ended'),
  )
  await clock.settle()
  expect(order).toEqual([])
  await clock.advance(1000)
  await ended
  expect(order).toEqual(['idle report', 'turn ended'])
})

test('an interrupted main turn reports idle activity; others report nothing', async ($, on) => {
  const clock = mock.clock(on)
  const calls = daemon(on, STATUS, [])
  engine(on)
  const hooks = () => calls.filter(call => CLAUDE.test(call[0] ?? '') && call[1] === 'hook').map(call => call.slice(1))
  const base = { answer: '', durationMs: 1, turnId: 'x' }
  await $.turn.complete({ ...base, isAborted: false, reason: 'answer' })
  await $.turn.complete({ ...base, isAborted: true, reason: 'aborted', agentId: 'sub-1' })
  await clock.settle()
  expect(hooks()).toEqual([])
  await $.turn.complete({ ...base, isAborted: true, reason: 'aborted' })
  await clock.settle()
  expect(hooks()).toEqual([['hook', IDLE]])
})

test('/inbox opens the pane on the newest message', async ($, on) => {
  const clock = mock.clock(on)
  mock.store(on)
  daemon(on, STATUS, [reply])
  const opened = engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  await $.command.run({
    command: 'inbox',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: false, columns: 100 },
  } as never)
  expect(opened).toEqual([expect.objectContaining({ id: 'goblins-inbox' })])
})

test('a resume or branch inside the process switches the views to that conversation', async ($, on) => {
  const clock = mock.clock(on)
  const store = new Map<string, unknown>(Object.entries({
    'conversation:conv-b': {
      at: 1,
      wakes: { 'row-b': { id: 'mB', at: 1 } },
      messages: {
        mB: { id: 'mB', sender: 's3', senderName: 'scout', inReplyTo: null, body: 'B news', createdAt: 1, state: 'completed', operation: null, raid: null },
      },
    },
  }))
  on('store.get', (_, e) => ({ value: store.get(e.key) }))
  on('store.set', (_, e) => {
    store.set(e.key, JSON.parse(JSON.stringify(e.value)))
    return { value: undefined }
  })
  on('store.keys', () => ({ value: [...store.keys()] }))
  on('store.delete', (_, e) => {
    store.delete(e.key)
    return { value: undefined }
  })
  let conversation = 'conv-a'
  let slow = false
  on('process.run', async (_, e) => {
    if (HELPER.test(e.argv[0] ?? '')) {
      if (slow) await clock.sleep(1000)
      const found = [reply, { ...reply, id: 'mC', body: 'late' }].find(m => m.id === e.argv[1])
      return found ? ran(JSON.stringify(found)) : ran('', 1)
    }
    return ran(JSON.stringify({ ...STATUS, queued: 0, head: null }))
  })
  engine(on, () => conversation)
  const end = (reason: string) => $.session.end({ reason, sessionId: conversation, resume: { sessionId: conversation } } as never)
  const label = async (uuid: string) => {
    const ui = await $.ui.mount({ ...row(uuid), surface: 'terminal' })
    const text = (await ui.find({ key: 'open' }))?.text
    await ui.unmount()
    return text
  }
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.settle()
  await $.session.append(append('row-a', wakeText(TASK, 'm2')))
  await clock.settle()
  expect(await label('row-a')).toContain('reviewer replied')
  // A fetch for A is still running when B takes its place.
  slow = true
  void $.session.append(append('row-a2', wakeText(TASK, 'mC')))
  await clock.settle()
  await end('resume')
  conversation = 'conv-b'
  await clock.advance(2000)
  await clock.advance(1000)
  expect(await label('row-b')).toContain('scout')
  expect(await label('row-a')).toBe('✉ Goblins inbox')
  expect(await label('row-a2')).toBe('✉ Goblins inbox')
  const b = store.get('conversation:conv-b') as { messages: Record<string, unknown>; wakes: Record<string, unknown> }
  expect(Object.keys(b.messages)).toEqual(['mB'])
  expect(Object.keys(b.wakes)).toEqual(['row-b'])
  // And back: A's own history, B's not in it.
  slow = false
  await end('resume')
  conversation = 'conv-a'
  await clock.advance(2000)
  expect(await label('row-a')).toContain('reviewer replied')
  expect(await label('row-b')).toBe('✉ Goblins inbox')
})

test('a full store makes room by dropping the oldest conversations', async ($, on) => {
  const clock = mock.clock(on, { now: 10_000 })
  const LIMIT = 4 * 1024 * 1024
  const size = (value: unknown) => new TextEncoder().encode(JSON.stringify(value)).length
  // Four old conversations from other goblins, written without an index.
  const store = new Map<string, unknown>()
  for (const at of [1, 2, 3, 4]) store.set(`conversation:old-${at}`, { at, wakes: {}, messages: {}, pad: 'x'.repeat(950_000) })
  const total = () => [...store.values()].reduce((sum: number, value) => sum + size(value), 0)
  on('store.get', (_, e) => ({ value: store.get(e.key) }))
  on('store.set', (_, e) => {
    const before = store.get(e.key)
    store.set(e.key, e.value)
    if (total() > LIMIT) {
      before === undefined ? store.delete(e.key) : store.set(e.key, before)
      return { deny: 'the plugin store is full' }
    }
    return { value: undefined }
  })
  on('store.keys', () => ({ value: [...store.keys()] }))
  on('store.delete', (_, e) => {
    store.delete(e.key)
    return { value: undefined }
  })
  daemon(on, STATUS, [reply])
  engine(on, 'conv-new')
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.session.append(append('row-new', wakeText(TASK, 'm2')))
  await clock.settle()
  const saved = store.get('conversation:conv-new') as { messages: Record<string, { body: string }> }
  expect(saved.messages.m2?.body).toBe(reply.body)
  expect(total() <= 3 * 1024 * 1024 + 4096).toBe(true)
  expect(store.has('conversation:old-1')).toBe(false)
  expect(store.has('conversation:old-4')).toBe(true)
})

// A store in memory whose reads of one key can be held.
function heldStore(on: On, entries: Record<string, unknown>) {
  const store = new Map<string, unknown>(Object.entries(entries))
  const held = new Map<string, Promise<void>>()
  on('store.get', async (_, e) => {
    await held.get(e.key)
    return { value: store.get(e.key) }
  })
  on('store.set', (_, e) => {
    store.set(e.key, JSON.parse(JSON.stringify(e.value)))
    return { value: undefined }
  })
  on('store.keys', () => ({ value: [...store.keys()] }))
  on('store.delete', (_, e) => {
    store.delete(e.key)
    return { value: undefined }
  })
  const hold = (key: string) => {
    let release = () => {}
    held.set(key, new Promise<void>(resolve => (release = resolve)))
    return () => {
      held.delete(key)
      release()
    }
  }
  return { store, hold }
}

const scoutNews = {
  'conversation:conv-b': {
    at: 1,
    wakes: { 'row-b': { id: 'mB', at: 1 } },
    messages: {
      mB: { id: 'mB', sender: 's3', senderName: 'scout', inReplyTo: null, body: 'B news', createdAt: 1, state: 'completed', operation: null, raid: null },
    },
  },
}

test('work for a conversation held across a switch never lands in the next one', async ($, on) => {
  const { store } = heldStore(on, scoutNews)
  // The clock's first read (A's wake time) is held until the switch is done.
  let releaseNow = () => {}
  let reachNow = () => {}
  const nowHeld = new Promise<void>(resolve => (releaseNow = resolve))
  const nowReached = new Promise<void>(resolve => (reachNow = resolve))
  let reads = 0
  on('clock.now', async () => {
    if (reads++ === 0) {
      reachNow()
      await nowHeld
    }
    return { value: 5 }
  })
  on('clock.sleep', () => new Promise<{ value: undefined }>(() => {}))
  let conversation = 'conv-a'
  const calls = daemon(on, STATUS, [reply, { ...reply, id: 'mB', sender_name: 'scout' }])
  engine(on, () => conversation)
  const label = async (uuid: string) => {
    const ui = await $.ui.mount({ ...row(uuid), surface: 'terminal' })
    const text = (await ui.find({ key: 'open' }))?.text
    await ui.unmount()
    return text
  }
  const appended = $.session.append(append('row-a', wakeText(TASK, 'm2')))
  // A's wake has its conversation's token and waits on the clock.
  await nowReached
  await $.session.end({ reason: 'resume', sessionId: 'conv-a', resume: { sessionId: 'conv-a' } } as never)
  conversation = 'conv-b'
  // B loads (/inbox waits for that), then A's held work resumes.
  await $.command.run({ command: 'inbox', args: '', origin: { kind: 'composer' }, presentation: { isFullscreen: false, columns: 100 } } as never)
  releaseNow()
  await appended
  // Appends are kept in order: B's own wake follows A's.
  await $.session.append(append('row-b2', wakeText(TASK, 'mB')))
  expect(await label('row-b')).toContain('scout')
  expect(await label('row-b2')).toContain('scout')
  expect(await label('row-a')).toBe('✉ Goblins inbox')
  expect(calls.filter(call => HELPER.test(call[0] ?? '')).map(call => call[1])).not.toContain('m2')
  const saved = store.get('conversation:conv-b') as { messages: Record<string, unknown>; wakes: Record<string, unknown> }
  expect(Object.keys(saved.messages)).toEqual(['mB'])
  expect(Object.keys(saved.wakes).sort()).toEqual(['row-b', 'row-b2'])
  expect(store.has('conversation:conv-a')).toBe(false)
})

test('a wake that arrives while its conversation loads stays mapped', async ($, on) => {
  const clock = mock.clock(on)
  const { store, hold } = heldStore(on, scoutNews)
  daemon(on, { ...STATUS, queued: 0, head: null }, [{ ...reply, id: 'mB2', sender_name: 'scout', body: 'more B' }])
  engine(on, 'conv-b')
  const release = hold('conversation:conv-b')
  const appended = $.session.append(append('row-b2', wakeText(TASK, 'mB2')))
  await clock.settle()
  release()
  await appended
  await clock.settle()
  const ui = await $.ui.mount({ ...row('row-b2'), surface: 'terminal' })
  expect((await ui.find({ key: 'open' }))?.text).toContain('scout replied · more B')
  await ui.unmount()
  const old = await $.ui.mount({ ...row('row-b'), surface: 'terminal' })
  expect((await old.find({ key: 'open' }))?.text).toContain('Message from scout · B news')
  await old.unmount()
  const saved = store.get('conversation:conv-b') as { wakes: Record<string, unknown> }
  expect(Object.keys(saved.wakes).sort()).toEqual(['row-b', 'row-b2'])
})
