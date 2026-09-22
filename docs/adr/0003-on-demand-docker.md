# ADR 0003: Private Docker scopes and on-demand activation

Date: 2026-09-22

Status: Accepted

Scope selection, sharing and child inheritance are extended by
[ADR 0004](0004-shared-docker-scopes.md). The private-only behavior below records
the original decision, not the current scope API.

## Context

Users need to enable containers inside a running sandbox without restarting its
shell or provisioning Docker for every session. ADR 0002 coupled namespace,
network and engine startup. Simply starting a second rootless daemon later
would not let it join the shell's existing network with the required authority.

Shared daemons, shared caches, their trust boundaries and configuration are
separate future decisions. This change implements only private scope ownership
and on-demand activation.

## Decision

Separate the per-session Docker manager from its optional engine. At session
startup, a trusted namespace keeper establishes the mapped ancestor and nested
user namespaces and the session network namespace. The shell and optional Pasta
connection use that network independently of Docker. No Docker daemon or data
directory exists until activation.

The engine launcher enters the ancestor namespace and session network, builds
its filesystem from the initial pinned workspace/grant plan, and only then
enters the nested user namespace and clones its mount namespace. This preserves
the kernel-locked inherited mounts and read-only flags from ADR 0002. The trusted
keeper's filesystem and namespace descriptors are not exposed to sandbox code.
The shell retains its stricter syscall filter; containers never resolve bind
sources against the host root filesystem.

`goblins enable-docker [--reason TEXT]` submits a distinct `kind: "docker"`
permission request through the existing per-session approval channel. It is not
an alias for a package request. The host approves authority to run the private
engine with that session's filesystem and network policy. No caller-supplied
daemon, path, namespace, scope or configuration is accepted.

Approval installs the configured Docker client through the existing package
grant mechanism and starts the engine and disk store. The command returns
`ready` only after the engine responds. Denial or pre-approval disconnect does
not activate Docker. A post-approval disconnect can have an unknown outcome;
the operation may finish. Repeating a completed activation is idempotent.
An activation failure may leave the client installed; it does not report Docker
enabled. Docker status is recorded separately from approved package names.

The shell receives `DOCKER_HOST` and a read-only socket-directory bind at launch;
the socket appears on activation. The Testcontainers non-privileged Ryuk default
is also present before activation. `docker.enable = true` still means automatic
startup; false means not started, not a prohibition on requesting approval.
Installing a package named `docker` or `docker-client` does not activate Docker.

Every scope is private to its session, including child sessions. Docker approval
is not inherited from an ancestor. Shutdown stops the engine and containers and
removes their ephemeral disk store, then releases the namespace keeper. Private
lifetime pipes also trigger teardown on controller death. Existing caveats
about power loss, guardian termination and orphaned data remain unchanged.

If namespace preparation fails for a Docker-disabled configuration, ordinary
sandbox startup remains available with its previous network setup. A subsequent
Docker request reports the preparation failure and requires a new sandbox after
the host prerequisites are repaired. Startup-enabled Docker instead fails the
launch. Helper compatibility is versioned as API 3; old running sessions are not
upgraded in place.

## Consequences

Activation preserves the shell's network namespace and running services. It
requires a small keeper and mapped namespaces even before Docker is enabled on
capable hosts. Runtime metadata references the Docker packages on the host, but
their engine closure is not mounted into the shell. Disk usage, repeated pulls,
cleanup, host prerequisites and container limitations remain as in ADR 0002.

There is no shared-daemon, cache, durable-volume or scope-selection configuration
in this change. Independent engines must not share a mutable Docker data root.

## Verification

The Docker integration suite tests activation, denial, withdrawal and stale
approval, malformed/authority-bearing requests, duplicate requests, idempotency,
unchanged networking, private stores, filesystem confinement and cleanup.
Existing build, Compose, user-switching, cgroup and controller-death checks also
exercise the refactored startup path.

See [usage](../../USAGE.org), [scope keeper](../../src/scope.rs),
[scope lifecycle](../../crates/controller/src/docker_scope.rs),
[engine manager](../../crates/controller/src/docker.rs) and
[integration tests](../../tests/test_docker.py).
