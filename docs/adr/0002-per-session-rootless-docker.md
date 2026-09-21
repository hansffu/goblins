# ADR 0002: Per-session rootless Docker within the sandbox filesystem boundary

Date: 2026-09-21

Status: Accepted

## Context

Development workflows need containers for services, image builds, Docker Compose
and Testcontainers. These operations must respect the filesystem grants of the
Goblins session that requests them. Running the entire sandbox in a VM is an
explicit non-goal.

The ordinary shell sandbox intentionally denies mount and namespace operations
that a container runtime requires. Giving it a host Docker socket would move
container creation and bind-source resolution outside its filesystem boundary.
Rootless Docker alone does not solve that problem: an unconstrained daemon can
still access files available to its host user.

A separately provisioned host user with a rootless daemon and directory
permissions can approximate the desired access policy, but requires host-specific
administration and does not compose naturally with per-session grants and
lifetimes. We need a reusable runtime feature rather than a separately managed
Docker service for each workflow.

## Decision

Provide `mkGoblin { docker.enable = true; ... }`, defaulting to `false`. Each
enabled session owns one rootless Docker daemon, a private socket and a private
data store. The host controller launches the engine within a filesystem sandbox
assembled from the session's initial workspace and explicit grants. Container
bind sources are resolved inside that confined engine.

The initial integration enables the shell through the `docker-shell` package and
`nix develop`. The ordinary default package remains opt-in; Codex and Claude
presets are unchanged. Enabling those integrations is separate work.

### Filesystem and privilege boundary

The engine reuses the shell's initial workspace and grant plan, including pinned
source descriptors and read-only mounts. It also receives its runtime closure
and engine-private state. Implicit temporary home and `/tmp` mounts are separate
from the shell's; build inputs intended for bind mounts belong in the workspace
or an explicit grant. Live Nix package grants currently update only the shell.

The launcher uses Bubblewrap and the host's `newuidmap`/`newgidmap` helpers to
establish an outer user namespace with the host user's UID/GID and authorized
subordinate IDs. A trusted native bootstrap creates a nested user namespace and
a less-privileged mount namespace. The kernel locks inherited mounts and their
read-only flags before Docker or container code runs. The engine receives the
namespace and mount capabilities needed by the OCI runtime; the shell retains
its existing stricter syscall filter.

Container root maps to the launching host user. Other container users map to
subordinate host IDs. The mapped parent installs the nested GID mapping without
disabling `setgroups`, allowing entrypoints such as Postgres's `gosu` to switch
users and supplementary groups. No host-root daemon, separate host account or
host Docker socket is exposed. `no_new_privs` remains in effect.

Docker API access grants control over the engine's permitted resources; API
validation is not the confinement boundary. A caller may create privileged containers or change the
engine's network configuration, but those requests remain subject to the outer
namespace and mount restrictions. Containers are not separate trust domains
from their own shell or other containers using the same engine.

Keep `/sys` and inherited cgroups read-only. An engine-specific inherited seccomp
filter denies creation of new cgroup namespaces, including through `clone3`
fallback handling. Without this restriction, a privileged container could mount
a fresh writable cgroup filesystem despite the inherited read-only bind. Docker
and BuildKit use the outer sandbox's cgroup namespace, owned by the ancestor user
namespace. There is no writable cgroup delegation or container resource quota.

### Session networking

The engine owns a private network namespace that the shell joins. Docker manages
its normal default bridge, user-defined networks, forwarding and NAT inside this
namespace. This supports ordinary Dockerfile `RUN` steps and Compose service
networks, including service-name DNS.

Pasta provides upstream connectivity only when the session's network policy
allows it. Offline sessions can have local container networks but no upstream
connection. `--network host` means the session network namespace. Published
container ports are reachable from the shell in that namespace; they are not
published onto the real host. Docker's network controls do not replace Goblins'
outer network boundary.

### Storage and lifetime

Use `overlay2` with a unique disk-backed directory under
`$XDG_CACHE_HOME/goblins/docker/session-*`, falling back to
`$HOME/.cache/goblins/docker/session-*`. Only runtime state remains on tmpfs.
The initial tmpfs/VFS implementation exhausted space on ordinary service images:
tmpfs competes with memory, and VFS copies complete layers. Disk-backed
copy-on-write storage avoids those costs.

Each daemon receives only its own data directory. The cache parent is protected
from startup grants and workspace snapshots, including in Docker-disabled
sessions, so another session cannot obtain it through a broader directory grant.
There is no shared image store or cross-session build cache.

A trusted supervisor retains a private cleanup-parent mount and the full ID
mapping. That mount is removed from the engine's view before the nested namespace
boundary is established. A private lifetime pipe ties the supervisor to the host
controller. Shell exit, session termination or controller death triggers process
teardown and reaping before removal of the session's data tree. Cleanup operates
with the mapped IDs so subordinate-owned and unreadable directories can be
removed, without following symlinks into other stores or host grants.

Images, build cache, writable layers and named volumes are ephemeral and are
deleted when the sandbox exits. Explicit host bind data is not deleted.
Power loss or killing the cleanup supervisor itself can leave an orphaned store;
stores are never automatically reused, and cleanup failures report their path.

### Docker and Testcontainers compatibility

Maintain a narrow patch against the pinned Moby source: skip the RootlessKit
port-mapping API when our native launcher supplies the namespaces, and make
BuildKit honor the outer-cgroup-namespace setting. Namespace assembly and cleanup
remain Goblins responsibilities rather than a persistent RootlessKit service.

Docker-enabled configurations default
`TESTCONTAINERS_RYUK_CONTAINER_PRIVILEGED=false`, overridable through `mkGoblin.env`.
Ryuk uses the session Docker socket and remains enabled to clean up after JVM
exit. Its privileged default requests writable sysfs and conflicts with the
mount boundary. General privileged-container compatibility is not promised;
requests that require writable sysfs or private cgroup namespaces remain denied.

## Alternatives considered

- **Expose a host Docker socket.** Bind sources and daemon operations would use
  the host daemon's authority, bypassing the session's filesystem grants.
- **One shared rootless daemon.** Rootless mode limits host privilege but does
  not separate callers' mounts, containers, volumes or Docker API authority.
  Sharing would require an additional authorization layer and lifecycle model.
- **Separate host user and service.** This requires provisioning identities and
  directory permissions outside the reusable session configuration. It also
  leaves service lifetime and cleanup separate from the sandbox.
- **Start Docker directly in the ordinary shell.** Its syscall policy prevents
  the required mount and namespace setup. A dedicated engine boundary supplies
  those capabilities without changing the shell's policy.
- **Place the sandbox or engine in a VM.** This is outside the intended native
  sandbox design and is explicitly excluded from this decision.
- **Use Podman instead.** Rootless or daemonless execution would still need an
  equivalent filesystem, namespace and cleanup boundary. It may be viable, but
  has not been validated here. Docker directly serves the existing Compose and
  Testcontainers workflows.
- **Disable bridges or Ryuk.** Disabling bridges breaks default builds. Ryuk
  works without privileged mode here and provides cleanup during a still-running
  sandbox, so session-exit cleanup is not a reason to disable it.
- **Share Docker data directories for caching.** Mutable engine stores carry
  state and authority across session boundaries. A future cache needs a separate
  design; independent daemons must not share a writable data root.

## Consequences and limits

Docker, Compose and Testcontainers can operate within the session's filesystem
authority without a VM. Engines, stores and local networks have session
lifetimes. The cost is per-session daemon overhead, repeated image pulls and
builds, and loss of named-volume data when the sandbox ends.

The host must support unprivileged user namespaces and rootless OverlayFS and
provide subordinate UID/GID ranges and privileged mapping helpers. The current
implementation and engine filter target x86_64 Linux. Non-root container users
may be unable to write host-user-owned grants and can leave subordinate-owned
files in writable grants.

This shares the host kernel and is not VM-equivalent isolation. There is no
independent security audit, disk quota, writable cgroup delegation, shared cache
or automatic orphan recovery. Native bootstrap and Moby changes must be reviewed
together with namespace, mount and cgroup regression tests when dependencies
change. Existing sessions retain their running engine and configuration.

## Verification and implementation references

The local-image suite covers default builds, Compose networking, online/offline
behavior, user switching, allowed and denied binds, privileged remount attempts,
cgroup restrictions, independent sessions, disk cleanup and abrupt server death.
Separate official-image checks cover Postgres initialization and restart, plus
Testcontainers Java 2.0.5 with Ryuk 0.14.0 and cleanup after JVM halt. These checks
demonstrate the tested workflows, not universal Docker compatibility.

The initial implementation is commit `366ec01`; disk storage, networking and
compatibility follow-ups are committed in `190ef55`. See
[usage](../../USAGE.org), [verification results](../../RESULTS.org),
[engine lifecycle](../../crates/controller/src/docker.rs),
[native bootstrap](../../src/docker.rs),
[engine filter](../../crates/controller/src/seccomp.rs) and
[integration tests](../../tests/test_docker.py).

Operational details and new compatibility evidence belong in usage and results.
A change to session ownership, filesystem authority, the no-VM requirement or
the disposition of Docker data requires revisiting this decision. Shared cache,
durable volumes and AI integration are not decided by this ADR.
