# ADR 0001: Daemon inboxes with notify-then-fetch agent communication

Date: 2026-09-19

Status: Accepted

## Context

Goblins already manages sandbox lifetimes, ownership trees, permission grants,
and native terminal attachment. Agents need to delegate work and exchange
multiple messages while the host can inspect what was actually communicated.
The first integration is Codex; Claude Code and other native coding agents
must be possible without rebuilding the messaging system around one provider.

Agents should inherit their parent's authentication and remain usable through
their native interfaces. Goblins integration must not require changes to the
user's agent configuration outside the sandbox.

## Decision

The host daemon owns agent inboxes, message identities, routing, processing
acknowledgements, retry records, and a host-readable communications log. Every
Goblins message and reply passes through the daemon. A sandbox's endpoint
determines its identity; message parameters cannot impersonate another sender.

Use notify-then-fetch. A notification tells an agent to check its inbox. The
agent fetches a message, processes it, and explicitly replies or acknowledges
it through the daemon. Notifications neither carry the message body nor consume
the queued item. Host users can initiate the same flow through `goblins send`.
File input is read by the sender and its contents are transmitted; recipient
access to the sender's filesystem is not required.

The baseline runs each agent's native CLI in its existing Goblins terminal.
Sandbox-mounted hooks check for work at lifecycle boundaries. An idle agent
receives a fixed terminal wakeup when its integration can establish readiness.
Skills describe the messaging workflow. Generated configuration is mounted
inside the sandbox without editing host settings. Children inherit the selected
integration and authentication mounts with their existing launch policy.

Keep notification delivery behind a small, versioned integration boundary.
An optional sandbox process may watch inbox state and coordinate notification.
The daemon remains authoritative if a hook or notification process disconnects.
This boundary should allow future native-protocol integrations while retaining
the inbox, agent-facing commands, logging, and native attachment behavior.

## Alternatives considered

- **Paste complete messages into terminals.** This avoids a fetch operation,
  but terminal writes do not establish message receipt or processing. A mailbox
  gives retries, correlation, inspection, and recovery explicit semantics.
- **Cooperative polling alone.** It works while the agent keeps checking, but
  cannot wake an agent that has returned to its input prompt. Hooks and idle
  notification complement the same explicit inbox commands.
- **Codex App Server or Claude Channels as the initial foundation.** These can
  improve delivery but tie the first implementation to provider-specific
  mechanisms. They are future options, not work authorized by this task.
- **A2A or another federation protocol.** External interoperability is not
  required for the local proof of concept and does not provide native CLI
  wakeups or Goblins sandbox ownership. It is out of scope.

## Consequences

The daemon can distinguish accepted, claimed, replied-to, and failed messages.
Lost or duplicate notifications do not lose or duplicate accepted messages.
The same flow supports host-to-agent and agent-to-agent requests.

Terminal readiness remains agent-specific and subject to UI races. Delivery
must defer for blocked or unknown interfaces and preserve user input during
attachment. Integration health must expose stalls rather than claim success
from a terminal write. Message processing is not exactly-once execution.

Shared configuration and workspaces can expose alternative communication
paths. The opt-in real-agent test must inspect the communications log and
additional evidence before claiming that its guessing game used only the
intended exchanges. This test is excluded from the default suite.

Inbox operations, notification behavior, configuration mounts, and audit output
need deterministic tests independent of model availability. Daemon restart
recovery remains out of scope.

## Scope and specification

This task delivers the Codex notify-then-fetch baseline only. It does not
implement, prototype, or test App Server, Channels, A2A, or other advanced
delivery integrations.

The evolving [agent integration specification](../specs/AGENT-INTEGRATION.org)
defines CLI syntax, wire contracts, limits, implementation sequencing, and
acceptance tests. The existing [daemon specification](../specs/DAEMON.org)
defines the underlying runtime contract. Routine changes to implementation
details belong in the specification; a change to this architectural decision
requires a superseding ADR.
