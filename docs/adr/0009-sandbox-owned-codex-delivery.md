# ADR 0009: Sandbox-owned Codex delivery and generic agent activity

Date: 2026-10-13

Status: Accepted; supersedes ADR 0001's Codex hook/terminal wakeup driver. The native relay, the foreground detection from native request IDs and the SessionStart binding are superseded by [ADR 0016](0016-server-and-client-goblins.md).

## Context

Terminal readiness and pasted reminders depend on native UI details and compete
with human drafts. Goblins already has authoritative inboxes, claims, replies,
operation notices, and a communications log. These should survive a change in
provider delivery. Sandbox lifecycle and current agent activity are separate.

## Decision

Run Codex 0.162.1 or newer with a private sandbox-local App Server, a standalone
`goblins-codex` Rust adapter, and the native terminal connected to an explicit
Unix socket outside shared Codex configuration. The launcher supervises helper
lifetimes and preserves native exit status. Authentication, skills, user hooks,
MCP filtering, native arguments/images, attachment and outer policy stay native.
Apply CLI configuration and native profile values to the local server as well;
remote thread creation alone does not forward every configuration field.

A managed SessionStart hook reports the exact thread ID. Since 0.162.1 defers
this hook until the first turn, the adapter also forwards the native connection
unchanged and captures IDs from successful foreground start/resume/fork responses.
Hidden title/recap threads and background task requests never select the binding;
the pinned native request identity distinguishes these from terminal selection.
This binds an empty terminal and a switched thread without guessing by name or
recency. The native ID guards against stale compaction hooks. Resume, clear and
switching replace the binding; compaction repeats it. Observe loaded
threads without changing configuration. The native terminal answers approvals
and user-input requests. The controller has no Codex protocol dependencies.

Accept concurrent native connections because the in-app resume picker connects while the main terminal stays open. Picker connections do not select or clear the foreground thread. Serialize successful foreground selections, renames and disconnect cleanup across connections; an older response or a disconnected former owner must not overwrite a newer selection. Apply Goblins' sandbox and approval policy in the adapter for fresh threads as well as resumes, keeping injected permission flags off the terminal so a later in-app remote resume remains available.

Forward WebSocket traffic with nonblocking readiness polling, bounded write buffers and backpressure in each direction. A slow reader must not throttle reverse traffic, truncate a partial frame or disconnect solely because a write deadline elapsed. Fatal forwarding errors carry a WebSocket close reason to the native UI and remain visible in adapter status until a successful foreground selection recovers it.

Verify lifecycle response permissions before selecting the thread. An already-loaded thread can ignore resume overrides; apply supported changes through `thread/settings/update` and re-read the effective settings before accepting it. Reject an unverified policy explicitly while preserving the previous selection. If an unfamiliar native request identity makes foreground ownership ambiguous, leave the native operation usable, warn through `configWarning`, and suspend automatic inbox delivery rather than continuing to route to the previous thread.

Poll inbox metadata every 500 ms. Submit the fixed inbox reminder using
experimental `thread/queue/add`, starting an idle queued reminder where required.
Never steer a live turn or write reminder text into the terminal. Delivery occurs
after the current turn, including interruption/failure, without independently
retrying the human task. The agent alone fetches and acknowledges inbox items.

Persist submission and queue IDs before retrying; correlate with
`clientUserMessageId` and history `clientId`. Maintain one outstanding reminder
per thread, reconcile queue/history after disconnects, and suspend submissions
when ownership remains uncertain. Limit retries without inbox progress to three,
at least 30 seconds apart. Local pause/resume controls new delivery and its retry
budget, accessible through existing `goblins exec`. Resume preserves uncertainty.
Codex no longer registers or ticks the daemon integration driver. Retain its
trusted configuration marker for operation notices. Claude remains unchanged.

Add sandbox-only `sessions.set-state {state}` and `goblins set-state STATE`.
Endpoint identity supplies the sandbox; no targets or host reports are allowed.
Accept unknown, idle, working, waiting-input, waiting-approval for running
sessions. Add `agent_state`, leaving lifecycle `state` unchanged. Daemon-owned
monotonic freshness expires activity after 30 seconds and clears it on stop.
Publish only visible changes. Codex reports each transition and ten-second
heartbeats, giving waiting-input precedence. No last-turn outcome is exposed.

Claude has no App Server, so its activity is the deliberate exception: the daemon derives it from the integration hooks Claude already sends, beside the unchanged wakeup lifecycle, and treats the plugin monitor's ticks as the heartbeat. Interrupted turns fire no hook and settle idle only at Claude's idle reminder, about 60 seconds later.

## Consequences

Mailbox authority and audit semantics are unchanged. Delivery bookkeeping is
local to the sandbox and survives adapter reconnects without guessing whether a
submission succeeded. Native drafts and images are independent of reminders.
Activity remains informational and separate from Goblins permission badges.

The adapter depends on an experimental provider protocol. Verify each baseline
with isolated native smoke tests and synthetic credentials/local Responses
scripts, alongside deterministic adapter tests, sandbox regressions, real daemon
subscriptions, Rust/Nix checks, and Emacs tests. A broken adapter leaves manual
inbox commands usable and activity eventually unknown.
