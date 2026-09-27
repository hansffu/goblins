# ADR 0006: Scopes share namespaces and storage; tools run in the goblin

Date: 2026-09-27

Status: Accepted

This decision generalizes [ADR 0004](0004-shared-docker-scopes.md) and replaces
the execution-service model explored on the `design/scoped-services` branch
(ADR 0005 there, never merged). The evidence is in
[the shared-namespace experiment](../../prototypes/shared-namespaces/README.md).

## Context

Sandboxes that share a tool's state (for example a Gradle user home) must be able
to coordinate with each other's processes. Gradle asks a lock holder to release
a cache lock by sending a UDP message to the holder's loopback port. Across
network namespaces that message is lost. An idle daemon in one sandbox then
blocks every other sandbox's build until Gradle's lock timeout, and the owner PID
in the error refers to another PID namespace. In the experiment it even named an
unrelated process in the waiting sandbox.

The execution-service prototype avoided this by running every tool process in
one service sandbox. That separated the command from the caller's environment:
granted packages, `JAVA_HOME`, Docker access and every other capability had to
be passed on to the service again. Toolchain selection and Docker access from
builds were the first two failures of that approach.

The experiment showed that sharing only the **network namespace** between
sandboxes makes Gradle's cross-process locking work, including concurrent
builds of one checkout. Sharing the **PID namespace** makes lock-owner PIDs
truthful and visible. Mounts, environment and packages can remain per sandbox.

## Decision

A **scope** is a named trust group whose members share:

- the scope's network namespace (always) and PID namespace (by default);
- managed storage directories, created and owned by Goblins;
- optionally one rootless Docker engine.

Tools run inside each member goblin, in that goblin's own sandbox, with its own
packages, environment, files and Docker socket attachment. There is no command
forwarding and no execution service.

### Ownership

`mkScope` owns only what must be shared: identity, lifetime, namespaces, managed
storage and the Docker engine. Everything describing the sandbox that runs
processes belongs to `mkGoblin`. A scope may supply **defaults** for those
options:

- lists (`allowedPackages`, `rwDirs`, `rwFiles`, `roDirs`, `roFiles`) are
  combined, scope entries first;
- attribute sets (`env`, `injectedFiles`, `scopeStorage`) are merged, and the
  goblin's value wins on a conflict.

A member can add to or override defaults but not remove them. Network policy is
the exception: members share one network namespace, so every goblin that may join
a scope must have the same `allowedDomains` online/offline policy. `mkGoblins`
rejects conflicting configurations.

```nix
mkGoblins {
  scopes.dev = mkScope {
    persistent = true;
    namespaces.pid.enable = true;
    storage.gradle = { };
    docker.enable = true;
    defaults = {
      allowedPackages = [ pkgs.gradle pkgs.jdk21 ];
      env.GRADLE_USER_HOME = "/var/cache/gradle";
      scopeStorage.gradle = "/var/cache/gradle";
    };
  };
  goblins.shell = mkGoblin {
    pkg = pkgs.bashInteractive;
    binName = "bash";
    scope = "dev";
    allowedScopes = [ ];
    allowedPackages = [ pkgs.jdk25 ];
  };
}
```

Scope defaults are resolved when Nix is evaluated. `mkGoblins` renders one
complete goblin configuration for each scope a goblin may join, so the
controller selects a prepared configuration instead of merging options.

### Membership

- A goblin belongs to at most one scope.
- The scope is chosen when a root sandbox starts: the goblin's `scope` by
  default, or `goblins run CONFIG --scope NAME` for a scope in `allowedScopes`.
  A goblin with no default scope starts without one unless `--scope` is given.
- Children always inherit their parent's scope. A running sandbox cannot change
  scope, because a process cannot move into another PID namespace.

### Lifetime and storage

A scope instance starts with its first member and stops after its last member
exits. Persistent scopes keep their managed storage under
`$XDG_CACHE_HOME/goblins/scopes/NAME/`. Temporary scopes share storage while
they have members, and delete it after the last member leaves. A later member
of the same temporary scope starts with empty storage. A host lock prevents two
controllers from using one persistent scope's storage at the same time.
Managed storage is protected against ordinary filesystem grants.

`scopeStorage.NAME = "/path"` mounts a storage directory read-write at that
path in the goblin. Only members of the scope that declares the storage can use
it.

### Namespaces

A trusted scope keeper owns the scope's user, network and PID namespaces. It is
the PID namespace's init process and reaps orphans. Stopping the keeper ends
every process in the PID namespace. Members join the network namespace and,
when `namespaces.pid.enable` is true, the PID namespace. Because a member's
sandbox has its own user namespace, it cannot mount `/proc` for the scope's PID
namespace. The per-session helper therefore mounts that `/proc` before creating
the member's user namespace, and Bubblewrap binds it into the sandbox.

Each member still has its own mount namespace, which payload code cannot leave
(seccomp prohibits `unshare`, `setns` and `mount`). When a member stops, the
controller kills every process in the scope's PID namespace that belongs to that
member's mount namespace. A daemon started by a departed goblin therefore never
keeps that goblin's mounts, including its writable workspace.

Tools must keep per-sandbox runtime state private. For Gradle, set
`org.gradle.daemon.registry.base` to a per-sandbox path such as
`/tmp/gradle-daemons`, so a daemon never serves a member with a different mount
view.

### Docker

`mkScope { docker.enable = true; }` allows the scope's members to use one shared
rootless engine. `goblins enable-docker` (after approval) or
`mkGoblin { docker.enable = true; }` attaches the goblin to that engine.
Containers publish ports on the scope network, which every member reaches over
loopback, so Testcontainers works from any member. A persistent scope keeps
engine data under the existing `$XDG_CACHE_HOME/goblins/docker/scope-NAME`
directory, so data from ADR 0004 named scopes carries over. A temporary scope's
engine data is deleted when the engine stops.

Goblins without a scope have no Docker. `mkGoblins.docker.scopes`,
`docker.defaultScope`, `docker.allowedScopes`, `enable-docker --scope` and
`enable-docker --anonymous` are removed. Temporary scopes replace anonymous
Docker scopes.

### Trust

A scope remains a trust group, and it now includes more than Docker. Members can
read and modify shared storage, affect the scope network and use the shared
engine. With a shared PID namespace, members can also see each other's
processes, read their command lines and environment through `/proc`, and signal
them. Disable `namespaces.pid.enable` when members must not see each other's
processes. Lock-owner PIDs then become meaningless again, but locking still
works over the shared network.

## Consequences

- Toolchain selection and Docker access from builds need no special mechanism:
  the goblin's own packages, environment and Docker socket apply.
- Warm daemons are reused within a goblin, not across goblins.
- Scope keepers use the same subordinate-ID mapping as Docker scopes, so every
  scope requires `/etc/subuid`, `/etc/subgid` and `newuidmap`/`newgidmap`.
- Existing named Docker scope configurations must move to `mkScope`.

## Deferred

- A cleanup hook that runs after the last member leaves.
- Scopes without a shared network namespace (storage-only scopes).
- Joining several scopes, or changing scope while running.
