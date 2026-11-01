# ADR 0018: Per-goblin supervisors

Date: 2026-10-29

Status: Accepted. Implements decisions Q1–Q5 in [SUPERVISOR-REFACTOR.org](../specs/SUPERVISOR-REFACTOR.org). The process cutover completed in commit `19ce5c4743240ce34045c01a933d8d9751406aca` on 2026-10-31. [The architecture guide](../architecture/README.org) describes the implemented system; [API.org](../API.org) specifies the private control protocol alongside the public API.

## Context

The daemon's controller managed approvals and records while also holding sandbox request listeners, PTY masters, init control sockets and helper exit handles. Its session workers already exchanged commands and events, but runtime capabilities and shared Rust values crossed that boundary. Scopes, network plumbing and Docker also lived under daemon-owned runtime code. Moving only the blocking worker into a child would leave sandbox transport and terminal failure handling in the controller.

A raid member can outlive the owner that launched it. Shared namespaces, workspace resources and Docker grants therefore need independent leases and a lifetime that does not depend on the first member's supervisor. Public host authority, raid control, messaging, history and attachment decisions still belong in the daemon.

## Decision

1. **Relay sandbox requests through their own supervisor (Q1).** Each supervisor owns its goblin's `request.sock`, bounded decoder and connections. It checks framing, typed parameters, initialization, fixed configuration `Policy` and backend `Support`, then emits `SandboxOpened`, `SandboxCall` and `SandboxClosed` events. The daemon derives the session actor from that runtime and applies live state, raid authorization and human decisions. Replies and connection closure return as commands. A sandbox cannot select another actor or bypass this gate.
2. **Keep terminals in the supervisor (Q2).** PTY masters, output buffers, terminal brokers and the server log belong to the runtime engine. The daemon keeps the modern attach handshake, readiness and client lifecycle decisions, then transfers the accepted stream to the supervisor. Legacy `terminal.sock` acceptance is also in the supervisor; the daemon only prebinds that listener for synchronous launch acceptance. Terminal bytes travel directly between frontend and supervisor.
3. **Use a separate pinned executable (Q3).** `packages/supervisor-bwrap` builds `goblins-supervisor-bwrap`. The Nix host wrapper pins its absolute store path through `GOBLINS_SUPERVISOR_RUNTIME`; the daemon captures that selection rather than searching PATH. One inherited control socket selects supervisor, scope, preflight or maintenance mode. The strict same-build protocol carries `supervisor_api`; a mismatched or incomplete startup fails before an engine starts.
4. **Give every scope its own daemon-started process (Q4).** The same executable runs a separate scope actor with membership and Docker leases, serialized activation and teardown, compatibility checks and cleanup of a departed member's mount namespace. Scope lifetime is independent of raid ownership and any single goblin. Resource leases retain ancestor resources while another member or the Docker engine needs them.
5. **Separate runnable packages, contracts and implementation (Q5).** `packages/` contains executables and adapters; `libs/` contains public protocol and internal contracts, Unix helpers and runtime implementation. `goblins-runtime` carries commands, events, policy and typed lease/handoff data. `goblins-supervisor` owns the Linux engine, worker, relay and terminals. Pure init wire data lives in `libs/api`; `libs/init` supplies its descriptor transport. The daemon links neither supervisor nor init implementation, including through test dependency routes. Schema-first public API work remains a separate, parked [API-SCHEMA.org](../specs/API-SCHEMA.org) plan.

The controller uses only the `Supervised` process backend for real sessions; `Fake` remains a controller test fixture. There is no in-process Local adapter or debug environment escape hatch. Actor tests execute a separately built production supervisor, selected by its exact build artifact, without adding that check-only fixture to the installed controller closure.

## Alternatives considered

- **Transfer accepted sandbox sockets to the daemon.** This keeps a single connection implementation, but lets the daemon speak directly to the sandbox and bypasses the intended supervisor request boundary.
- **Keep PTYs in the daemon and transfer only worker results.** This preserves the old broker placement but leaves terminal I/O, buffering and transport failure coupled to the controller.
- **Use a hidden daemon subcommand or mount-helper mode.** Both reuse an executable, but mix the runtime actor with an authority or namespace setup program whose responsibilities differ. A separate pinned binary makes its dependency and startup boundary explicit.
- **Keep scopes in the daemon.** This retains namespace, network and Docker implementation there. **Let the first member own the scope** instead ties surviving members to a supervisor that may stop or crash.
- **Keep an in-process fallback.** This would leave two production execution paths and implementation dependencies in the daemon. The final cutover removes it and tests that the former environment variable cannot select it.

## Consequences

Every goblin adds a host process and a framed control channel. Descriptor handoff, partial messages, backpressure and ownership acknowledgements must be handled explicitly. Typed senders keep transfer guards through adoption or confirmed child death; receivers retain real cleanup guards during multipart handoff. Member processes start only after the owner's complete export is available, rather than sharing an `Arc` or a worker's Rust state.

The supervisor applies fixed policy and backend support before the daemon checks live state; a call failing several checks can therefore receive a different first refusal. A requesting socket hangup becomes a relay event, so the daemon's decision check uses the relayed connection lifetime rather than reading that socket itself. Queue saturation must return capacity or apply backpressure without blocking the controller or losing accepted events.

`Stopped`, resource release and process end are separate. A stopped goblin can retain terminal output; name and history availability follow authoritative runtime release, and complete end also requires event delivery, terminal drain and child reap. Resource-directory disappearance alone does not establish name availability. A supervisor failure affects its own goblin, closes its relayed calls and streams, and releases its capabilities; other members keep running. Daemon control EOF stops all its children, with parent-death signaling as a backstop. There is no live-session restart recovery or promise of directory cleanup after abrupt host failure.

The foreground daemon can raise its descriptor capacity while passing its original soft limit as checked startup metadata for sandbox payloads; the supervisor inherits the raised soft limit and unchanged hard limit. The preexisting background `server start` self-exec path loses that process-local original-limit metadata and remains outside this propagation mechanism.

These boundaries remove Linux runtime implementation from the daemon dependency graph; they do not provide a second operating-system backend. Scope isolation, payload policy, public resource limits and the user's approval authority remain governed by the existing contracts.
