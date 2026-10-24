# ADR 0015: A Claude Code mod presents the inbox; managed hooks keep the lifecycle

Date: 2026-10-24

Status: Accepted, experimental.

## Context

Claude Code 2.1.287 added mods: plugin code that runs inside Claude Code, hooks events in process and draws in the interface. Goblins' Claude integration relies on managed settings hooks and a plugin monitor (ADR 0001, 0009). Their gaps: an interrupted turn fires no hook, so activity stays working until Claude's idle reminder a minute later; a Stop continuation shows as a "Stop hook error" row; and every inbox wake shows the fixed "check inbox" prompt instead of who wrote.

Probes against Claude Code 2.1.309 in a goblin found:

- Wherever managed settings exist, the built-in `sec-default` guard passes every classic hook event (`classic.SessionStart`, `classic.Stop`, ...) past a user's mod. A goblin always has managed settings. Seating the mod as the organization's needs a managed marketplace, which Claude Code also registers in the shared `CLAUDE_CONFIG_DIR`.
- A Stop block is drawn as a hook row no mod can restyle. A prompt a mod submits with `$.prompt.submit` is drawn with Claude Code's own plugin-prompt frame, and the submitting mod's render hook is not raised for it.
- A monitor line is a `UserMessage` row (origin `task-notification`) that a mod can redraw; `session.append` names its row ID and full text. One monitor is one task for all its lines, so the row ID, not the task ID, identifies a line. A line that arrives during a turn is delivered as its own turn afterwards.
- `turn.complete` reports an interruption (`isAborted`).

## Decision

The managed hooks stay authoritative for every lifecycle and activity report, and the plugin monitor for delivery. The plugin adds a mod for what hooks cannot do, and changes no daemon, client, protocol or Emacs code:

- The monitor names the message each wake is about (`[goblins-inbox-wake ID]`), and records its PID. While that process runs, the Stop hook reports `active`: the daemon settles `ready`, and its notifier delivers and retries through the monitor. Without the monitor, Stop blocks as before. Delivery never depends on the mod.
- On an interrupted main-loop turn the mod reports Claude's idle reminder at once (activity only), awaiting it before the turn's completion passes on so a late report cannot overwrite the next turn.
- The mod redraws each wake row as the message it names, opens a pane on it (a press, or `/inbox`), and counts waiting messages in the prompt footer. It reads messages with `messages.get` through a read-only helper, since the CLI has none, and never claims or completes them.
- Row labels and message previews persist in the plugin store per Claude conversation, so a resumed conversation keeps them and other conversations sharing the directory never read them. The views switch with the conversation in force (including a resume or branch inside the process), work started for one conversation never lands in another, and the oldest conversations are evicted to stay within the store's 4 MiB.

## Consequences

Without the mod (an older Claude Code, `--safe-mode`, a policy refusing users' mods) delivery is unchanged apart from the wake marker in the monitor's text, and continuations no longer use Stop-hook rows while the monitor runs. With it, the daemon sees `active` Stops from goblins whose monitor runs; those continuations appear as notifier wake attempts, bound by its retry policy. The mod depends on an early-access API, checked by its own tests and `claude plugin validate` in the `claude-mod` check for each pinned Claude Code; the Python tests drive the hooks and the packaged monitor with a fake Claude.
