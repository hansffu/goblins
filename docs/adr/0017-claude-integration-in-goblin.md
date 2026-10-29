# ADR 0017: Claude's integration lives in the goblin; the daemon has no agent driver

Date: 2026-10-28

Status: Accepted. Supersedes [ADR 0001](0001-daemon-inboxes-for-agent-communication.md)'s fixed terminal wakeup and its daemon-side integration boundary, [ADR 0009](0009-sandbox-owned-codex-delivery.md)'s exception that the daemon derives Claude's activity from its hooks, and the daemon's part in [ADR 0015](0015-claude-code-mod.md)'s delivery (settling `ready` and notifying through the monitor), which `goblins-claude` takes over. The plan is [CLAUDE-INTEGRATION-IN-GOBLIN.org](../specs/CLAUDE-INTEGRATION-IN-GOBLIN.org).

## Context

Codex goblins keep all Codex code in `mkCodexGoblin`: `goblins-codex` keeps its own wakeup ledger and uses only generic daemon calls (`inbox.status`, `sessions.set-state`, `sessions.set-variables`). Claude goblins do not. Their hooks and plugin monitor call `goblin integration hook|watch`, and the daemon interprets Claude's hook vocabulary, keeps the wakeup lifecycle and retries, and derives Claude's activity.

The daemon's terminal also holds code that only served Claude: pasting the inbox prompt into the composer, reading the screen to judge the composer empty, and tracking human input so that Esc or Ctrl-C pauses wakeups. Nothing Goblins ships uses terminal delivery: Claude's monitor uses notification delivery, and Codex never registers.

Claude Code's own background sessions (`claude --bg`, `claude attach`) were considered as a server/client split like ADR 0016's, and dropped: Claude works on the goblin's terminal, and the split would need a private configuration directory per goblin.

## Decision

Claude stays on the goblin's terminal. A new program in `mkClaudeGoblin`, `goblins-claude` (crate `crates/claude`), takes over the integration:

- `start`, run by the launcher, begins a fresh ledger for the run, replacing registration and its epoch.
- `hook`, run by every managed hook, keeps the lifecycle and activity in a ledger private to the goblin and decides `Stop` continuations.
- `watch`, the plugin monitor's process, decides wakeups and prints them as the monitor's notifications.
- `status`, `pause` and `resume` inspect and control wakeups locally, as `goblins-codex` does; the host uses them through `goblins exec`.

The wakeup rules are unchanged apart from terminal input: at most three reminders per inbox head, 30 seconds apart, reset by a new head or an empty inbox, and a `Stop` continuation when the monitor is not running. Activity is reported with `sessions.set-state` and repeated every ten seconds.

The daemon keeps no agent driver. It loses the `integration.*` methods, the `integration` field of `inbox.status`, the `agent-integration-v1` feature, the `integration.*` communications log events, terminal delivery, composer reading and input tracking, and the `goblin integration` and `goblins integration` commands. `mkGoblin`'s `integration` attribute stays only as the marker that a goblin is an agent, which allows `bypassPermissions` and enables operation notices.

## Consequences

The daemon and the generic `goblin` CLI no longer change with Claude Code's hook events; Claude and Codex integrations follow the same pattern. Esc and Ctrl-C no longer pause wakeups; an interrupted turn still gets no reminder until the next submitted prompt, because it fires no `Stop`. Claude's wakeup attempts move from the communications log to `goblins-claude status`. A Claude goblin built with this change works with a daemon that still has `integration.*`; one built before it starts against the new daemon without wakeups or activity.
