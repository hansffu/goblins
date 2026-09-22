# ADR 0004: Shared Docker scopes are trust groups

Date: 2026-09-22

Status: Accepted

## Decision

`mkGoblins.docker.scopes` declares a unique list of scope names. Each goblin can
specify `docker.defaultScope` and `docker.allowedScopes`, referencing those
names. The default is implicitly allowed. Anonymous scopes remain available
without any scope configuration. `docker.enable` continues to control eager
activation, not whether a user may request Docker.

`goblins enable-docker --scope NAME` requests host approval to attach to a named
scope; `--anonymous` requests a fresh anonymous scope. No options activate the
current/default selection. Children inherit the parent's selected scope and
activation, including anonymous and non-default named scopes. Children continue
to use their parent's configuration and pinned filesystem grants. A later parent
switch does not move existing children.

One active scope owns one daemon, socket and overlay2 data root. Session socket
paths hard-link the same Unix socket; there is no Docker API authorization proxy.
Independent root sessions attach only after approval or startup enablement.
Inactive shells do not acquire the socket merely because another member starts
Docker. Explicit leases stop the daemon and containers after the last attached
sandbox exits or switches away. Inactive shells can retain the small namespace
keeper and network attachment without retaining a daemon. Dedicated creator
threads keep namespace helpers alive independently of the initiating worker.

Anonymous data is removed on last-user teardown. Named data persists under
`$XDG_CACHE_HOME/goblins/docker/scope-NAME` (default `$HOME/.cache`). A separate
per-name lock is held by the trusted guardian throughout engine teardown.
Sharing is controller-local; another controller fails rather than concurrently
opening the same mutable data root. A later controller can reuse idle storage.
Guardian shutdown also handles controller death. Power loss or guardian death
can leave orphaned anonymous data; named data is intentionally retained.

## Filesystem and trust boundary

Named scopes are trust groups, not sandbox isolation boundaries. Members can
inspect, modify or delete one another's containers, images, caches and volumes,
and use the union of filesystem grants introduced by attached members. Read-only
grants remain read-only, but secrets in them are readable by peers. Grants are
not revoked when a member exits: they remain until the engine stops. Joining a
scope is unsuitable for mutually untrusted agents or projects. Persisted images,
logs, writable layers and volumes may retain secrets; there is no quota or
automatic expiry. Containers with restart policies may start on reactivation.

The daemon resolves bind sources inside its confined filesystem, never the host
root. Initial mounts retain ADR 0002's nested-user-namespace locks. Later grants
are pinned by descriptor and inserted by a host-only helper. The helper reopens
the source in its private mount namespace, verifies device/inode identity, pins
the new descriptor, applies access flags, crosses the nested user namespace and
clones the now kernel-locked mount. It publishes through a pinned engine root
using beneath/no-symlink destination traversal. Renamed/replaced sources may
fail closed. Conflicting destinations, access modes and overlapping new grants
are rejected rather than silently upgrading authority. Already-added approved
grants can remain after a partially failed attachment until the engine stops.

Session request sockets and package-control directories are excluded from the
engine filesystem. Retained filesystem owners keep pinned resources alive
even after their originating session leaves controller history. The engine's
own scratch root is writable to create later mount targets; host read-only
binds remain kernel-locked. This does not grant access to arbitrary host paths.

## Networking and compatibility

Default and inherited members start in the scope network. Selecting a different
scope from a running shell does not move existing processes or connections.
A trusted relay forwards published IPv4 TCP ports into that shell's existing
network (128 ports, 64 simultaneous connections per port). UDP, IPv6, direct
container addresses and arbitrary host-network listeners are not relayed.
Existing local port listeners are never replaced. Online/offline policies must
match; a conflicting default still permits anonymous activation. Scope members
can influence the shared network through the Docker API.

The native helper contract is API 4. Restart controllers and create fresh
sandboxes after upgrading; existing sessions are not upgraded in place. The RPC
feature `docker-scopes` advertises selection support. No VM, host Docker socket,
separate host account or shared mutable data root between daemons is introduced.

## Verification

Host integration tests cover lazy startup, a shared socket/image/volume store,
last-user shutdown, idle and full-scope restart, parent exit, anonymous and
non-default inheritance, switching, published-port access, filesystem union,
privileged read-only-remount rejection, scope authorization and network-policy
conflicts. Existing build, Compose, subordinate-user, cleanup and controller
death tests remain applicable. These tests are not an exhaustive security audit.
