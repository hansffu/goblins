# ADR 0016: Server and client goblins; Codex without the relay

Date: 2026-10-28

Status: Accepted. Supersedes the parts of [ADR 0009](0009-sandbox-owned-codex-delivery.md) that relay the native Codex terminal through the adapter, detect the foreground thread from native request IDs and bind it from the SessionStart hook. Its inbox delivery, retry, activity and control rules stay. The plan is [SERVER-CLIENT-GOBLINS.org](../specs/SERVER-CLIENT-GOBLINS.org).

## Context

A Codex goblin runs `codex app-server`, the `goblins-codex` adapter and the native terminal `codex --remote`, which reaches the server only through the adapter. The adapter relays every WebSocket frame because two jobs need that position: learning which conversation the terminal shows, and rewriting the approval and sandbox policy on the terminal's requests. Its inbox reminders and activity already use its own server connection. The foreground detection guesses from native request ID formats, and is the most version-fragile part of the integration.

The native terminal also runs on one long-lived daemon PTY. A goblin started detached, such as a raid member, starts its terminal with nobody attached, so startup terminal queries (background color, cursor position, keyboard support) are answered late or not at all. Reattaching replays at most 64 KiB of raw output without a repaint.

Codex already separates server and client. Probes against the App Server (0.164.0) and its 0.163.0 source found:

- `thread/started` reaches every connection, but nothing says which connection started a thread or which thread a client shows.
- A fresh thread cannot be resumed by another connection until it is saved. `thread/name/set` saves it without a model turn.
- Resuming raises no `thread/started`, and SessionStart runs once per in-memory session, so rejoining a still-loaded thread produces no signal at all.
- Listed thread items carry `startedAtMs` and item IDs.

## Decision

Goblins gets generic attach handlers. `mkGoblin` takes `onAttach` (`command`, `timeout`, `waitFor`) and an optional `onDetach`, with `{key}` placeholders filled from the goblin's variables as `resumeCommand` does. A goblin with `onAttach` runs its main command in the background without a terminal, for its whole life. Each attach starts `onAttach` as a fresh `goblins-init` exec on a new PTY with the attaching terminal's size. Goblins without `onAttach` are unchanged.

- Readiness: the daemon waits until every placeholder and `waitFor` name has been set to a value during the current run, then starts the client. A value seeded from the history record does not count. On timeout the attach fails; the user restarts the goblin. There is no fallback.
- Detach stops the client, and the client exiting detaches. Detach runs `onDetach` if set, then cancels the client's whole process subtree through init's keeper. One attachment at a time.
- New frontends attach through `attach.sock` and send a hello with their version and size first. `terminal.sock` keeps today's protocol for single-PTY goblins and returns an upgrade error for server goblins.
- The goblin sees `attached` and `attached_since` in `sessions.status`.

Codex becomes a server goblin. The main command starts the App Server and `goblins-codex` on its own connection; nothing is relayed. `goblins-codex` creates (and names, so it is saved) or loads the goblin's conversation with the goblin's policy, and records `session_id` on every run. `onAttach` is `codex --remote … resume {session_id}`. A missing recorded conversation on recreate stops the goblin. `codex` on `PATH` stays a wrapper that runs its own engine with the goblin's sandbox and approval settings.

Reminders follow the attached terminal from the person's first message in a conversation. `goblins-codex` joins every user thread and rebinds to the thread of the newest user message that started during the current attachment and is not one of its own reminders. It catches up on messages it missed before it could join, so a first turn that finished early still counts. Switching to a conversation without sending a message does not move reminders. The managed SessionStart hook is removed.

Only the newest installed Codex is supported; the version floor rises to it.

## Consequences

The relay, its request-ID rules, picker handling and the SessionStart binding disappear. Every attach gets a client that talks to the real terminal, which removes the late-query and reattach problems for Codex. Quitting Codex detaches instead of stopping the goblin, so stopping goes through `goblins stop` and the frontends' stop actions. An unsent composer draft may be lost on detach.

Detached goblins have no terminal output; the server log replaces it. A conversation resumed in the terminal receives reminders only from the person's first message in it. A client connected by hand to the internal server socket can still move the binding; `goblins-codex bind THREAD` corrects it.

The attach handlers are generic and also suit other agents with a server/client split. The Codex integration still depends on the experimental App Server protocol, now on fewer parts of it.
