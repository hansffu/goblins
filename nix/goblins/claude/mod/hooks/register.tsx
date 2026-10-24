// The Goblins mod: what Claude Code's settings hooks cannot do. The managed
// hooks and the plugin monitor stay authoritative for the integration
// lifecycle and inbox delivery; this module only reports interrupted turns
// and draws the inbox: wake rows, a footer count and a message pane.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { GoblinsInbox, GoblinsMessage } from '../types'

const GOBLIN = '/run/goblins/bin/goblin'
// The monitor's description in plugin.json, as its notification rows name it.
const MONITOR = 'Goblins daemon inbox messages'
// The notification the monitor's line becomes, exactly: no other row matches.
const WAKE = new RegExp(
  `^<task-notification>\\s*<task-id>[^<]{1,128}</task-id>\\s*<summary>Monitor event: "${MONITOR}"</summary>\\s*` +
    '<event>Check your Goblins inbox and process the next item\\. \\[goblins-inbox-wake(?: ([A-Za-z0-9_-]{1,128}))?\\]</event>',
)
const PANE = 'goblins-inbox'
const KEPT = 100

const inbox = atom({ plugin: 'goblins-daemon', key: 'inbox' } as const, null, { shape: 'inbox-1' })
const messages = atom({ plugin: 'goblins-daemon', key: 'messages' } as const, {}, { shape: 'messages-1' })
const wakes = atom({ plugin: 'goblins-daemon', key: 'wakes' } as const, {}, { shape: 'wakes-2' })
const selected = atom({ plugin: 'goblins-daemon', key: 'selected' } as const, null)

// Module state: a reload starts these over.
let isPolling = false
// The pane's message as last read, at the inbox revision of that read.
let refreshed: { id: string; revision: number } | undefined
const fetching = new Set<string>()
// The conversation the views hold (its store key) and a generation that
// changes with it: work started for one conversation never lands in another.
let active: string | undefined
let generation = 0
let loading: Promise<void> = Promise.resolve()
let isIndexed = false

async function goblin($: EngineInterface, args: string[], timeoutMs = 5000): Promise<any> {
  try {
    const run = await $.process.run([GOBLIN, ...args], { timeoutMs })
    return run.exitCode === 0 ? JSON.parse(run.stdout) : undefined
  } catch {
    return undefined
  }
}

function toMessage(raw: any): GoblinsMessage | undefined {
  if (typeof raw?.id !== 'string') return undefined
  const operation = raw.operation
  return {
    id: raw.id,
    sender: String(raw.sender ?? ''),
    senderName: String(raw.sender_name ?? raw.sender ?? 'unknown'),
    inReplyTo: typeof raw.in_reply_to === 'string' ? raw.in_reply_to : null,
    body: String(raw.body ?? ''),
    createdAt: Number(raw.created_at ?? 0),
    state: String(raw.state ?? ''),
    operation: operation
      ? {
          kind: String(operation.kind ?? ''),
          subject: String(operation.subject ?? ''),
          status: String(operation.status ?? ''),
          message: typeof operation.message === 'string' ? operation.message : null,
        }
      : null,
    raid: raw.raid ? { name: String(raw.raid.name ?? ''), event: String(raw.raid.event ?? '') } : null,
  }
}

// Reads a message through the read-only helper into the conversation that
// asked (its token), keeping the newest KEPT.
async function fetchMessage($: EngineInterface, id: string, token: number): Promise<GoblinsMessage | undefined> {
  if (fetching.has(id) || token !== generation) return undefined
  fetching.add(id)
  try {
    const run = await $.process.run([`${$.plugin.root}/bin/goblins-claude-message`, id], {
      timeoutMs: 10000,
    })
    const message = run.exitCode === 0 ? toMessage(JSON.parse(run.stdout)) : undefined
    if (!message) return undefined
    if (!(await changeMessages($, token, known => newest({ ...known, [message.id]: message }, kept => kept.createdAt)))) {
      return undefined
    }
    void save($, token)
    return message
  } catch {
    return undefined
  } finally {
    fetching.delete(id)
  }
}

export function describe(message: GoblinsMessage | undefined): { title: string; preview: string } {
  if (!message) return { title: 'Goblins inbox', preview: '' }
  const preview = message.body.replace(/\s+/g, ' ').trim()
  if (message.operation) {
    const { kind, subject, status, message: outcome } = message.operation
    return { title: `Goblins: ${kind} ${subject} ${status}`.replace(/\s+/g, ' '), preview: outcome ?? '' }
  }
  if (message.raid) return { title: `Raid ${message.raid.name} ${message.raid.event}`, preview: '' }
  if (message.inReplyTo) return { title: `${message.senderName} replied`, preview }
  return { title: `Message from ${message.senderName}`, preview }
}

export function modeLabel(box: GoblinsInbox | null): string | undefined {
  if (!box) return undefined
  if (box.health === 'paused' || box.health === 'stalled') return `✉ ${box.health}`
  return box.queued > 0 ? `✉ ${box.queued}` : undefined
}

function time(ms: number): string {
  const at = new Date(ms)
  return at.toTimeString().slice(0, 5)
}

async function poll($: EngineInterface): Promise<void> {
  if (isPolling) return
  isPolling = true
  try {
    const token = await current($)
    const status = await goblin($, ['inbox', 'status'])
    if (!status) return
    const next: GoblinsInbox = {
      queued: Number(status.queued ?? 0),
      claim: typeof status.claim === 'string' ? status.claim : null,
      head: Array.isArray(status.head) && typeof status.head[0] === 'string' ? status.head[0] : null,
      revision: Number(status.revision ?? 0),
      health: String(status.integration?.health ?? 'unregistered'),
    }
    const previous = await read($, inbox)
    if (JSON.stringify(previous) !== JSON.stringify(next)) await update($, inbox, () => next)
    const known = await read($, messages)
    for (const id of [next.head, next.claim]) {
      if (id && token !== undefined && !known[id]) await fetchMessage($, id, token)
    }
    // A message's state changes on claim, completion and requeue: the pane
    // shows its message as read at the current revision, retrying a failed read.
    const shown = await read($, selected)
    if (token !== undefined && shown && (refreshed?.id !== shown || refreshed.revision !== next.revision)) {
      await refresh($, shown, token)
    }
  } finally {
    isPolling = false
  }
}

async function refresh($: EngineInterface, id: string, token: number): Promise<void> {
  const revision = (await read($, inbox))?.revision ?? -1
  if ((await fetchMessage($, id, token)) && token === generation) refreshed = { id, revision }
}

// Every selection is explicit and read afresh, so the pane never shows a
// state from before the selection.
async function select($: EngineInterface, id: string): Promise<void> {
  const token = await current($)
  if (token === undefined || !(await changeSelected($, token, id))) return
  await refresh($, id, token)
}

async function openPane($: EngineInterface, id: string | null): Promise<void> {
  await current($)
  const all = await read($, messages)
  const newest = Object.values(all).sort((a, b) => b.createdAt - a.createdAt)[0]?.id
  const shown = await read($, selected)
  const target = id ?? (shown && all[shown] ? shown : newest)
  if (target) void select($, target)
  await $.ui.open({ id: PANE, title: '✉ Goblins inbox', focus: true, closeOnEscape: true })
}

async function reportInterrupt($: EngineInterface): Promise<void> {
  // Activity only: the daemon maps Claude's idle reminder to idle. The
  // integration lifecycle waits for the next turn, as for any interruption.
  // Bounded and awaited, so it cannot land after the next turn's report.
  await goblin($, ['integration', 'hook', '--event', 'Notification', '--notification', 'idle_prompt'], 2000)
}

function newest<T>(all: Record<string, T>, order: (value: T) => number): Record<string, T> {
  const kept = Object.entries(all).sort(([, a], [, b]) => order(b) - order(a))
  return Object.fromEntries(kept.slice(0, KEPT))
}

function record(value: unknown): Record<string, any> {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, any>) : {}
}

// A conversation's rows and messages outlive the process in the plugin's
// store, so a resumed conversation still names its wake rows. The store is
// shared by every goblin using this Claude configuration directory, so each
// conversation (Claude's session ID, kept on --resume) reads only its own.
// The store refuses writes past 4 MiB in all: records keep previews, not
// bodies (selecting a message reads it whole), and the oldest conversations
// make room before a write.
const CONVERSATION = 'conversation:'
const INDEX = 'index'
const BUDGET = 3 * 1024 * 1024
const RECORD = 256 * 1024
const PREVIEW = 512

type Index = Record<string, { at: number; bytes: number }>

function bytes(value: unknown): number {
  return new TextEncoder().encode(JSON.stringify(value)).length
}

async function conversation($: EngineInterface): Promise<string | undefined> {
  try {
    return CONVERSATION + (await $.session.id())
  } catch {
    return undefined
  }
}

// Writes conversation state for the conversation `token` names, and only
// while it is still in force: checked again inside each compare-and-set try,
// so a switch between the read and the write can never let it through.
// (The engine lists a module's state by literal references: one per atom.)
type Rows = Record<string, { id: string | null; at: number }>
type Messages = Record<string, GoblinsMessage>

async function changeWakes($: EngineInterface, token: number, fn: (value: Rows) => Rows): Promise<boolean> {
  if (token !== generation) return false
  await update($, wakes, value => (token === generation ? fn(value) : value))
  return token === generation
}

async function changeMessages($: EngineInterface, token: number, fn: (value: Messages) => Messages): Promise<boolean> {
  if (token !== generation) return false
  await update($, messages, value => (token === generation ? fn(value) : value))
  return token === generation
}

async function changeSelected($: EngineInterface, token: number, id: string | null): Promise<boolean> {
  if (token !== generation) return false
  await update($, selected, value => (token === generation ? id : value))
  return token === generation
}

// The conversation in force, with its views loaded: returns its token, or
// undefined when another conversation took its place meanwhile. Loading
// (the first call, or a resume or branch inside the process) clears the
// views and restores that conversation's record; every caller waits for it.
async function current($: EngineInterface): Promise<number | undefined> {
  const key = await conversation($)
  if (!key) return undefined
  if (key !== active) {
    forget(key)
    loading = load($, key, generation)
  }
  const token = generation
  await loading
  return token === generation ? token : undefined
}

async function load($: EngineInterface, key: string, token: number): Promise<void> {
  try {
    await changeWakes($, token, () => ({}))
    await changeMessages($, token, () => ({}))
    await changeSelected($, token, null)
    const stored = record(await $.store.get(key))
    const rows: Record<string, { id: string | null; at: number }> = {}
    for (const [row, entry] of Object.entries(record(stored.wakes))) {
      const id = record(entry).id
      rows[row] = { id: typeof id === 'string' ? id : null, at: Number(record(entry).at ?? 0) }
    }
    const kept: Record<string, GoblinsMessage> = {}
    for (const raw of Object.values(record(stored.messages))) {
      const message = record(raw)
      if (typeof message.id === 'string' && typeof message.senderName === 'string') kept[message.id] = message as GoblinsMessage
    }
    await changeWakes($, token, () => rows)
    await changeMessages($, token, () => kept)
  } catch {
    // Nothing stored yet, or a store this build cannot read.
  }
}

function forget(key: string | undefined): void {
  active = key
  generation += 1
  refreshed = undefined
}

async function scan($: EngineInterface): Promise<Index> {
  const index: Index = {}
  for (const key of await $.store.keys()) {
    if (!key.startsWith(CONVERSATION)) continue
    const stored = await $.store.get(key)
    index[key] = { at: Number(record(stored).at ?? 0), bytes: bytes(stored) }
  }
  return index
}

// Deletes the oldest conversations but `keep` until the rest fit in `budget`.
async function evict($: EngineInterface, index: Index, keep: string, budget: number): Promise<void> {
  const others = Object.entries(index).filter(([key]) => key !== keep).sort(([, a], [, b]) => a.at - b.at)
  let total = Object.values(index).reduce((sum, entry) => sum + entry.bytes, 0)
  for (const [key, entry] of others) {
    if (total <= budget) break
    await $.store.delete(key)
    delete index[key]
    total -= entry.bytes
  }
}

async function save($: EngineInterface, started: number): Promise<void> {
  const key = active
  if (!key || started !== generation) return
  try {
    const kept = Object.values(await read($, messages)).sort((a, b) => b.createdAt - a.createdAt)
    const value = {
      at: await $.clock.now(),
      wakes: await read($, wakes),
      messages: Object.fromEntries(
        kept.map(message => [message.id, { ...message, body: message.body.slice(0, PREVIEW) }]),
      ),
    }
    // One conversation's record stays small, oldest messages first out.
    while (bytes(value) > RECORD && Object.keys(value.messages).length > 0) {
      delete value.messages[kept.pop()!.id]
    }
    if (started !== generation) return
    // Other goblins write this store too: rebuild the index from it once.
    let index: Index = isIndexed ? record(await $.store.get(INDEX)) : await scan($)
    isIndexed = true
    index[key] = { at: value.at, bytes: bytes(value) }
    await evict($, index, key, BUDGET)
    try {
      await $.store.set(key, value)
    } catch {
      // Full after all (writes this index never saw): make room and retry once.
      index = { ...(await scan($)), [key]: index[key]! }
      await evict($, index, key, BUDGET / 2)
      await $.store.set(key, value)
    }
    await $.store.set(INDEX, index)
  } catch {
    // The views work from this process's state alone.
  }
}

async function noteWake($: EngineInterface, row: string, id: string | null): Promise<void> {
  const token = await current($)
  if (token === undefined) return
  const at = await $.clock.now()
  if (!(await changeWakes($, token, all => newest({ ...all, [row]: { id, at } }, entry => entry.at)))) return
  if (id && !(await read($, messages))[id]) await fetchMessage($, id, token)
  await save($, token)
}

// Every hook passes on what it cannot do: a failure here must never cost the
// session a row, a turn or a drawing.
export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    // The first count, without holding the session's start on the daemon.
    await Promise.race([poll($), $.clock.sleep(500)])
    $.clock.every(2000, () => void poll($))
    try {
      await $.command.register({
        name: 'inbox',
        description: 'Show this goblin’s daemon messages',
        immediate: true,
      })
    } catch (error) {
      $.ui.log(`/inbox is unavailable: ${String(error)}`, { to: 'debug' })
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  on('command.run', { command: 'inbox' }, async $ => {
    await openPane($, null)
    return {}
  }).catch(() => ({}))

  // /clear, a resume or a branch puts another conversation in the process:
  // the views drop this one's history, and load the next one's when it is in force.
  on('session.end', async ($, e, next) => {
    forget(undefined)
    const token = generation
    await changeWakes($, token, () => ({}))
    await changeMessages($, token, () => ({}))
    await changeSelected($, token, null)
    return next(e)
  }).catch(($, e, next) => next(e))

  // Each wake row names its own message: a monitor is one task for all its
  // lines, so the row's ID (the UserMessage requestId) is the key.
  on('session.append', { door: 'prompt', origin: { kind: 'task-notification' } }, async ($, e, next) => {
    if (e.agentId === undefined) {
      const text = e.message.content.map(block => (block.type === 'text' ? block.text : '')).join('')
      const wake = WAKE.exec(text)
      // The row draws next: name its message first, if that is quick.
      if (wake) await Promise.race([noteWake($, e.uuid, wake[1] ?? null), $.clock.sleep(500)])
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  on('turn.complete', async ($, e, next) => {
    // Claude runs no settings hook when a person interrupts a turn.
    if (e.agentId === undefined && e.isAborted) await reportInterrupt($)
    return next(e)
  }).catch(($, e, next) => next(e))

  // The monitor's wake row: a compact line naming the message instead of the
  // notification. ctrl+o (expanded) still shows the row as the model read it.
  on('ui.render', { component: 'UserMessage', props: { origin: { kind: 'task-notification' } } }, async ($, e, next) => {
    // The native compact row contains the event summary, not the full wake
    // envelope. A monitor exit/failure can name it too; keep that notice visible.
    if (e.props.isExpanded || e.props.text !== `Monitor event: "${MONITOR}"`) return next(e)
    const { Box, Button, Text } = $.ui.resolve(e)
    const id = (await read($, wakes))[e.requestId]?.id ?? null
    const message = id ? (await read($, messages))[id] : undefined
    const { title, preview } = describe(message)
    // One line: the preview gets what the row has left, cut with an ellipsis.
    const room = Math.max(16, (e.viewport?.columns ?? 100) - title.length - 12)
    const shown = preview.length > room ? `${preview.slice(0, room - 1)}…` : preview
    return (
      <Box>
        <Button key="open" plain onPress={() => openPane($, id)}>
          ✉ {title}
          {shown ? <Text dimColor> · {shown}</Text> : ''}
        </Button>
      </Box>
    )
  }).catch(($, e, next) => next(e))

  on('ui.render', { component: 'SessionMode' }, async ($, e, next) => {
    const label = modeLabel(await read($, inbox))
    return label ? next({ ...e, props: { ...e.props, modes: [...e.props.modes, label] } }) : next(e)
  }).catch(($, e, next) => next(e))

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Button, Markdown, Text } = $.ui.resolve(e)
    const all = await read($, messages)
    const box = await read($, inbox)
    const shown = await read($, selected)
    const list = Object.values(all).sort((a, b) => b.createdAt - a.createdAt)
    const message = (shown && all[shown]) || list[0]
    const others = list.filter(other => other.id !== message?.id).slice(0, 8)
    const waiting = box ? `${box.queued} waiting` : 'inbox status unknown'
    return (
      <Box flexDirection="column" gap={1}>
        {message ? (
          <Box flexDirection="column">
            <Text bold>✉ {describe(message).title}</Text>
            <Text dimColor>
              {message.senderName} · {time(message.createdAt)} · {message.state}
              {message.inReplyTo ? ' · reply' : ''}
            </Text>
          </Box>
        ) : (
          <Text dimColor>No Goblins messages in this conversation yet.</Text>
        )}
        {message ? <Markdown text={message.operation?.message ?? message.body} /> : ''}
        {others.length > 0 ? (
          <Box flexDirection="column">
            <Text dimColor>Earlier</Text>
            {others.map(other => (
              <Button key={other.id} plain dimColor onPress={() => select($, other.id)}>
                ✉ {describe(other).title} <Text dimColor>{time(other.createdAt)}</Text>
              </Button>
            ))}
          </Box>
        ) : (
          ''
        )}
        <Text dimColor>{waiting} · the agent reads and answers messages itself · Esc closes</Text>
      </Box>
    )
  }).catch(($, e, next) => next(e))
}
