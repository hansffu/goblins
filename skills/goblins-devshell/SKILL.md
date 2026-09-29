---
name: goblins-devshell
description: Use and refresh this Goblins sandbox's host-trusted flake dev shell. Use when changing development dependencies, flake.nix or flake inputs, when a command reports that flake.lock differs from the trusted lock, when the environment seems stale, or instead of nix develop, nix run or nix shell (there is no nix command). For a one-off tool the project flake shouldn't carry, use goblins-packages.
---

This sandbox runs in the project flake's dev shell, like `nix develop`: its
tools, variables and `shellHook` exports are already in your environment.
`goblins status` shows the flake. Editing project files does not change the
active generation.

There is no Nix executable in the sandbox. Use the Goblins commands below.
Other Nix operations, such as build or flake check, are not available.

## Change the environment

Edit `flake.nix` and related source files as needed. Never edit `flake.lock`
by hand or with other tools; lock changes happen only through a refresh.

Choose the flags for the task:

- No flags: refresh from changed source with the existing lock.
- `--lock`: lock inputs newly declared in `flake.nix`.
- `--update INPUT...`: update the named inputs.
- Bare `--update`: update all inputs; use only when that scope is intended.

`--lock` and `--update` are mutually exclusive.

1. Run `goblins devshell diff` with those flags. It shows the changes a
   refresh with the same flags would propose: source, inputs, packages,
   environment, `shellHook`, and what will be fetched or built. It asks
   nothing and changes nothing. Fix unexpected changes before requesting.
2. Run `goblins devshell refresh --reason "WHY THE TASK NEEDS THIS"` with the
   same flags. A preview is not approval; this asks the host and waits.
   - Exit 0: approved. The new `shellHook` has already run once.
   - Exit 1: denied or a known failure. Report it and continue with the
     existing environment where possible; do not retry unchanged.
   - Exit 2: unknown outcome. Do not resubmit on your own; the original
     request may still complete.
3. Existing processes, including your current shell and any shell it starts,
   keep the old environment. Separate command executions may not keep
   environment changes, so source `env.sh` in the same Bash command that
   needs it:

       bash -c 'source /run/goblins/devshell/env.sh && YOUR_COMMAND'

   Sourcing also reruns `shellHook` and provides the dev shell's functions.

## Handle a foreign lock

Diff, refresh and flake run require the workspace `flake.lock` to match this
sandbox's trusted lock; formatting differences do not matter. An edit, a
checkout, or another sandbox's approved refresh in the shared workspace can
make it foreign. Do not try to bypass the check.

`goblins devshell restore-lock` overwrites the workspace lock with this
sandbox's trusted one. Before running it, inspect and preserve any lock
changes that still matter. If another goblin's refresh wrote the lock,
restoring breaks its check in turn, so coordinate with that goblin first,
through your parent when it is not your parent or child. Then make the
intended change through a refresh with `--lock` or `--update`.

Generations belong to individual sandboxes even though they share files: a
refresh changes only the sandbox that asked for it. Existing parent and child
goblins keep their own generation. New children start in their parent's
current generation.

## Run a flake app

Use `goblins flake run '#APP' -- ARGS` instead of `nix run`, and
`goblins flake run '#default'` for the default app. Quote the selector: an
unquoted `#` starts a shell comment. Only this dev shell's own flake is served,
from the current trusted generation, with no approval. If any tracked file in
the flake's repository changed, it fails until a refresh is approved.

The app inherits the caller's environment; it does not source `env.sh`
automatically. Source it first, as above, if the app needs the refreshed
environment.

## Choose a mechanism

- Tools the project needs long-term belong in the flake: edit, diff, refresh.
- A one-off tool for the current task: `goblins request-package`
  (goblins-packages).
- Containers: goblins-docker, not a flake package.
