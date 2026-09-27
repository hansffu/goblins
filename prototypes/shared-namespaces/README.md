# Shared scope namespaces experiment

Tests an alternative to the execution service in
[ADR 0005](../../docs/adr/0005-scoped-services.md): Gradle runs **inside each
client sandbox** with a shared Gradle home, and the scope shares only kernel
namespaces between its clients. The question is whether this avoids the
cross-sandbox lock problem that motivated execution services: a lock held by a
process in another sandbox that the waiting build can neither reach nor see.

## Run

```sh
python3 prototypes/shared-namespaces/experiment.py --output /tmp/shared-ns
# optionally: --variant shared-net --scenario shared-home
```

Requires bwrap, util-linux `unshare`/`nsenter`, iproute2, Java and Gradle from
the Nix store. There's no internet access; each client serves a generated
Maven repository on its own loopback port, so each client's first build must
write new dependency metadata into the shared cache.

## Setup

A scope holder (`unshare --user --net --pid`) owns the scope namespaces. Each
client is a long-lived bwrap sandbox, kept open like an agent's shell so its
Gradle daemon keeps running while idle. The client enters the holder's user
namespace, then joins or unshares its network and PID namespaces:

| Variant | Network ns | PID ns |
|---|---|---|
| `isolated` | per client | per client |
| `shared-net` | scope | per client |
| `shared-net-pid` | scope | scope |

Every client mounts its Gradle home at the same path, `/gradle-home`, and uses
a private daemon registry
(`-Dorg.gradle.daemon.registry.base=/tmp/daemon-registry`), so a daemon never
serves a client with a different mount view.

Scenarios (A and B are two clients):

- `shared-home`: shared Gradle home, separate checkouts. A builds and its daemon
  stays idle; then B builds; then A builds again.
- `same-checkout`: private Gradle homes, one checkout. B builds while A runs a
  20s task, then again while A's daemon is idle.
- `shared-home-same-checkout`: both shared, same steps as `same-checkout`.

## Results (2026-09-27, Gradle 9.7.1, OpenJDK 25.0.4.1)

| Variant / scenario | B while A's daemon idle | B during A's build | B after A's build |
|---|---|---|---|
| isolated / shared-home | **FAIL, 61.8s lock timeout** | – | – |
| isolated / same-checkout | – | ok, 22.9s (waited for A) | ok, 0.4s |
| isolated / shared-home-same-checkout | – | **FAIL, 61.7s lock timeout** | **FAIL, 121.3s** |
| shared-net / all three | ok, 3.0s | ok, 3.3–3.6s | ok, 0.5s |
| shared-net-pid / all three | ok, 3.9s | ok, 3.2–3.9s | ok, 0.4s |

Every failure was `Timeout waiting to lock journal cache
(/gradle-home/caches/journal-1). It is currently in use by another process.`
The owner PID in the error (52 or 53) was A's daemon **in A's PID namespace**.
Inside B, that PID belonged to a different Java process, apparently B's own
daemon: the daemon PIDs in both isolated clients were 52. An agent following
the error's advice would inspect or kill the wrong process.

Findings:

1. The isolated baseline reproduces the original problem. Gradle asks the lock
   holder to release over loopback UDP; across network namespaces that request
   never arrives, so an idle daemon in another sandbox blocks the shared cache
   until the waiting build times out.
2. **Sharing only the network namespace fixes it**, including concurrent builds
   of one checkout. With contention requests delivered, B no longer even waits
   for A's running build (3.6s instead of 22.9s).
3. Project `.gradle/` locks in a shared checkout are released when a build ends
   even without namespace sharing. Only the user-home cache was blocked by an
   idle daemon.
4. Sharing the PID namespace isn't needed for correctness. Its value is
   truthful diagnostics: daemon PIDs are unique across the scope (58 and 207)
   and visible to every member. No lock errors occurred in that variant, so
   owner-PID reporting wasn't exercised directly.

## Limitations

One run per cell with a tiny project and one Gradle/JDK version. Not covered:
the configuration cache, the build cache, toolchain provisioning, wrapper
downloads, different JDKs per client, Docker or Testcontainers, or larger
concurrent workloads. Clients run as root inside the scope's user namespace,
not with Goblins' identity mapping. Clients join the scope's namespaces at
sandbox creation; late attachment of an already-running shell wasn't tested.
