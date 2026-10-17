# ADR 0011: Let a goblin use the host's Nix with allowNix

Date: 2026-10-17

Status: Accepted

## Context

A goblin starts with a fixed closure and asks for anything else through package grants, or through an approved dev shell refresh. That suits most work, but not developing Goblins itself, which needs `nix build`, `nix develop` and `nix flake check` on the project's own flake. agent-sandbox.nix already has `allowNix`, and `mkGoblin` forwarded it, but Goblins rejected `true` twice: in Nix validation and in the Rust launcher. Goblins does not run agent-sandbox's Python launcher; it reads the build spec and launches with its own Bubblewrap arguments, so agent-sandbox's store and socket binds never applied.

## Decision

`allowNix = true` is accepted and implemented in the Rust launcher, reusing what agent-sandbox provides: `nix` on `PATH` through its implicit packages, the `allow_nix` flag in the build spec, and its conventions for the socket path and refusing a launch without a daemon.

- The sandbox binds the host's `/nix/store` read-only instead of its private staging store, so outputs the daemon builds later are visible at once. Package grants and Docker engine extensions have nothing to mount.
- It binds the daemon socket: the default socket's directory, which holds nothing else and survives the socket being recreated, or a socket elsewhere alone, without its neighbours. `NIX_DAEMON_SOCKET_PATH` from the server's environment selects another socket, and is set inside the sandbox together with `NIX_REMOTE=daemon`, because the launch clears the environment. A launch without a socket fails. Docker engines do not get the socket.
- `NIX_CONFIG` defaults to the flake CLI with `accept-flake-config = false`, since there is no `/etc/nix`; a goblin's own `env.NIX_CONFIG` wins.
- `allowNix` requires `allowedDomains = null`: the daemon fetches over the host network, so an offline goblin with Nix would not be offline. Members of a scope with Docker must agree on `allowNix`, in `mkGoblins` and in the daemon, because a shared engine's store view comes from its members. Other scopes may mix them, so a goblin with Nix can keep caches in scope storage and still start children without Nix.
- A goblin with Nix uses Nix workflows only. Package requests, dev shell refreshes and diffs and `flake run` are refused with a hint, recreate skips recorded grants, and its agent gets a `goblins-nix` skill instead of `goblins-packages` and `goblins-devshell`, so its instructions present one workflow. Starting it in a host-evaluated dev shell still works. Skills are selected per feature by `nix/goblins/skills.nix` for both Claude and Codex.

## Consequences

The goblin can read and run anything in the host store, and acts as the host user towards the daemon, with that user's trust. Builds go through the daemon's own sandbox. `result` links in the shared working directory are host GC roots; links in the sandbox's private `/tmp` or home are not. The full plan, including what remains to verify, is `docs/specs/NIX-AND-HOST-COMMANDS.org`.
