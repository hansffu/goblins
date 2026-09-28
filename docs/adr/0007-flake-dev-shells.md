# ADR 0007: Host-trusted flake dev shells with approved refreshes

Date: 2026-09-28

Status: Accepted

This decision lifts part of the "arbitrary project flakes" deferral in
[the project plan](../specs/PLAN.org): a sandbox can use a project flake's dev
shell. `nix run` of flake apps follows on the same mechanism. Other flake
operations stay deferred.

## Context

Agents working in a Nix project expect `nix develop`. Package grants cover
single nixpkgs attributes, but a project's dev shell is a whole environment:
compilers, libraries, variables and a `shellHook`. Approving it package by
package would be tedious and would not match what the project declares.

Flakes cannot simply be evaluated on the host at an agent's request. Evaluation
runs as the host user, with the host's files, credentials and Nix daemon, and
the agent controls the flake:

- inputs can point at host paths (`path:/home/...`, `git+file:`), which Nix
  copies into the world-readable store;
- `git+ssh` inputs, private forges and `access-tokens` use the host's
  credentials;
- `builtins.fetchGit` and `fetchTarball` in `flake.nix` fetch at evaluation
  time and bypass `flake.lock` entirely;
- `nixConfig` can request substituters, keys and daemon settings;
- evaluation can trigger builds (import-from-derivation) and can exhaust
  resources.

The agent may edit `flake.nix` freely; it is ordinary project code. `flake.lock`
is different: it decides which code is fetched and run, so its changes must be
visible to and approved by the user.

## Decision

### No Nix in the sandbox

Sandboxes get no `nix` binary, no Nix daemon socket and no shim. The whole
interface is the Goblins CLI. Agent skills say so explicitly and name the
commands below, so agents do not try `nix develop` and work around the failure.

### Launch

```sh
goblins run CONFIG --dev-shell .#rust
```

`--dev-shell PATH[#ATTR]` is a CLI option only; `mkGoblin` has no dev shell
setting. It accepts local flake directories only. A relative path resolves
against the directory where `goblins run` is invoked, including in
snapshot-workspace mode.

The host user starting a dev shell is choosing to run that code, so launch
needs no approval. The controller:

1. copies the flake source into the store and records its store path,
   `narHash` and parsed `flake.lock` as **generation 1**, with GC roots;
2. evaluates `nix print-dev-env --json` on that store copy, with
   `--no-update-lock-file` and `accept-flake-config = false`, in the evaluator
   sandbox (below). Only the flake's source tree (its Git work tree, or the
   flake directory) is visible. Launch uses the host network, because the
   sandbox's network does not exist yet;
3. builds the environment, mounts its closure read-only like a package grant,
   and writes the variables to `/run/goblins/devshell/env.sh`.

The launch lock must be complete; Goblins never creates or updates a lock at
launch. The sandbox reads the trusted snapshot, never the workspace flake,
until a refresh.

### Environment in the sandbox

Entering a generation behaves like `nix develop -c PROGRAM`, inside the
sandbox:

- `/run/goblins/devshell/env.sh` reproduces the environment in Bash:
  exported and non-exported variables, arrays, functions and, last, the
  `shellHook`. Variables `nix develop` ignores (`HOME`, `TMPDIR`, `TERM` and
  similar) and sandbox plumbing (`USER`, `DOCKER_HOST`, `BASH_ENV`,
  `GOBLINS_*`) are left out. `PATH`, `PKG_CONFIG_PATH` and `XDG_DATA_DIRS`
  put the dev shell's entries before the sandbox's own; other variables
  override configured `env` values, as with `nix develop`.
- The sandbox's entry program (shell or agent) is started by a Bash that
  sources `env.sh` and then `exec`s it, so the `shellHook` runs on entry and
  its exports reach the entry program and everything it starts. Children enter
  the same way when they start.
- The `shellHook` runs only inside the sandbox, never on the host. Everything it
  can use is the generation's closure, which is already mounted, so it can run
  whenever a generation is entered without further approval.
- A refresh replaces `env.sh` with the new generation's and runs it once
  inside the sandbox, so the new `shellHook` runs as it would on entering the
  shell. Goblins does not push the new environment into running processes: no
  `BASH_ENV`, no live `PATH` profile. The refresh command tells the agent to
  `source /run/goblins/devshell/env.sh` (in Bash) where it needs the new
  environment, and the agent decides when to do so. New children enter the
  new generation normally.

### Trusted and foreign locks

A lock is **trusted** only when the host launched the sandbox with it or the
host approved a generation containing it. Nothing an agent or Goblins command
produces is trusted without user intervention.

Before any evaluation, every dev-shell command compares the workspace
`flake.lock` (parsed JSON, so formatting does not matter) with the sandbox's
trusted lock. Any other lock, including a missing one, a hand edit, a
`git checkout` or a committed change, is **foreign** and the command fails:

```
error: flake.lock differs from the trusted lock for this sandbox
  changed: nodes.nixpkgs.locked.rev
  revert:  goblins devshell restore-lock
  then:    goblins devshell refresh --update nixpkgs
```

`goblins devshell restore-lock` writes the sandbox's current trusted lock back
into the workspace.

There is no standalone `lock` or `update` command. Lock changes happen only as
part of a refresh, so a new lock is always evaluated, shown with its effect on
the environment and approved together with it, rather than accumulating
untested.

### Refresh

| Command | Where | Approval |
|---|---|---|
| `goblins devshell diff [--update INPUT...] [--lock]` | sandbox | none; a preview with no side effects |
| `goblins devshell refresh [--update INPUT...] [--lock] --reason TEXT` | sandbox | host approval |
| `goblins devshell restore-lock` | sandbox | none; writes the trusted lock back |

There is no host-initiated refresh: the host restarts a sandbox to choose a new
dev shell.

`--update` bumps the named inputs (all inputs when none are named). `--lock`
adds entries for inputs that `flake.nix` declares but the lock lacks. Without
either, a missing lock entry is an error. A refresh runs in stages:

1. **Lock check** (above). No network.
2. **Evaluation** in the evaluator sandbox, from a new store snapshot of the
   workspace flake source. With `--update` or `--lock`, the candidate lock is
   computed here, in the snapshot, not in the workspace. A dry run gives the
   closure and the paths to fetch and build without building.
3. **Approval** shows one generation diff, in the style of `nh`, against the
   last trusted generation. It is the approver's whole basis for the decision.
   The daemon sends facts only, as a structured preview: attention items
   (changed input sources, new inputs, `nixConfig`, local builds, a changed
   `shellHook`), changed files with line counts, input, package and
   environment changes (each `added`, `removed` or `changed` with old and new
   values), fetch and build counts, every package the dry run fetches or
   builds, and the source patch without `flake.lock`. Each frontend assembles
   its own sections, wording and colors from it:

   - **Emacs** shows a collapsible summary with highlighted keys, additions
     and new values green, removals and old values red, changes and
     attention items in the warning color; then **Nix changes** (expanded
     magit-style file sections) and **Other changes** (all non-Nix files,
     collapsed with its file list). TAB folds every section; expanded
     sections stay open across updates.
   - **TUI** shows the same colored summary in the approval popup; `d` opens a
     full-screen colored diff with Nix files first and other files hidden
     until `a`; `y`/`n` decide from the diff.

   Package changes compare the dev shell's direct inputs; the incoming list
   makes transitive changes (a new glibc under a nixpkgs bump) visible. A full
   runtime-closure comparison is only possible after the build. All
   displayed text is escaped for control and bidi characters.
4. **Apply** after approval: build, mount the new closure, write the new
   `env.sh`, run it once inside the sandbox, and record the generation and
   its lock as trusted. A candidate lock is written into the workspace only now.

Rejection leaves the workspace lock and the active generation unchanged.

A refresh is approved automatically when its source hash and lock equal a
generation an ancestor already has. That generation is trusted only because the
host launched or approved it, so no new trust is granted. This rule lives beside
`inherits_package`.

### Lock validation

Stage 2 rejects, before fetching:

- `path:`, `git+file:` and `file:` inputs outside the flake source;
- `git+ssh` and other credentialed transports;
- unlocked inputs and inputs without `narHash`;
- a `nixConfig` attribute (reported, never applied).

### Evaluator sandbox

Lock validation cannot cover code in `flake.nix`. Every evaluation, lock
computation and dry run therefore runs in a host-side Bubblewrap sandbox that
sees only the store, the source snapshot and the Nix daemon socket, with:

- an empty `HOME`, no `SSH_AUTH_SOCK`, and `NIX_USER_CONF_FILES` pointing at a
  Goblins-owned configuration with no `access-tokens` or netrc;
- `accept-flake-config = false`, `experimental-features = nix-command flakes`,
  no `--impure`, and `--no-update-lock-file` except when computing a candidate
  lock;
- for a refresh, the requesting goblin's network namespace, so fetches leave
  the host the same way the agent's own traffic does;
- the host `nix` and `git` resolved to their store paths, the host's
  `/etc/resolv.conf` and `/etc/hosts`, and nothing else from `/etc`;
- no flake registry (`flake-registry =`), so an indirect reference must
  already be locked; a Goblins-pinned registry can be added later;
- a time limit, and cancellation from the approval interface.

Builds still go through the host Nix daemon and its build sandbox.

### Children and generations

- A child starts on its parent's **current** generation, with the same mounts.
- Generations are per sandbox. A refresh changes only the sandbox that asked
  for it. Neither parent nor children switch; each must refresh itself.
- Sandboxes in one branch share the workspace and therefore the workspace
  `flake.lock`. If a child's approved refresh changes it, the parent's next
  dev-shell command reports a foreign lock. The parent either restores its own
  lock or refreshes to the child's generation. That refresh needs approval,
  because automatic approval covers only ancestors' generations.
- In snapshot-workspace mode the lock is written only into that branch's copy.

### `nix run` (next)

`goblins flake run FLAKEREF [-- ARGS]` reuses the trusted snapshot, lock
check, evaluator sandbox and approval. It requires the app's program to be a
store path and mounts its closure. Details are decided when it is built.

## Consequences

- Agents get a project's full environment without per-package approvals, and
  every environment change is reviewed as one diff.
- Lock changes are visible and cannot be introduced by editing a file.
- A sandbox's environment changes only on its own refresh, and running
  processes keep their old environment until the agent sources the new
  `env.sh`. Agents must be told this; the skill and the refresh output say so.
- Dev shells mount build-time closures, which are much larger than runtime
  closures.
- The approval interface gains a generation diff view.

## Implementation notes

- Nix fetches and uses inputs that `flake.lock` does not mention, even with
  `--no-update-lock-file`, and only warns. Launch and refresh therefore require
  the lock Nix computes (`nix flake metadata --json`'s `locks`) to equal the
  file; otherwise the flake is rejected, or a refresh must use `--lock`.
- The package diff lists the dev shell derivation's direct inputs by name and
  version; the environment diff ignores store-hash-only changes.
- A refresh is also approved automatically when it equals the sandbox's own
  current generation.
- Launch and refresh evaluate only a flake whose source tree lies inside the
  sandbox's working directory, so evaluation never sees files the sandbox
  cannot. Without this, launching `repo/sub/flake.nix` from `repo/sub` would
  let a flake an agent edited earlier copy tracked `repo/` files into the dev
  shell (`builtins.readFile ../secret`, `${../.}`). A launch without a host
  working directory (an RPC start without `cwd`, or a daemon `--workspace`
  snapshot) cannot use a dev shell.

- Refresh evaluation disables import-from-derivation
  (`allow-import-from-derivation = false`), before and after approval, so
  a preview can never build; a flake that needs IFD cannot be refreshed.
  Launch allows it, because the host chose the flake.
- A flake in a subdirectory keeps its whole source tree (`path:ROOT?dir=SUB`),
  so it can use files outside its directory as with ordinary Nix; the source
  diff compares whole trees.
- Refresh evaluation before approval (snapshot, lock, evaluation, dry run)
  fails after 10 minutes (`GOBLINS_EVALUATION_TIMEOUT` seconds in the daemon's
  environment), so a flake that never finishes evaluating fails the request
  instead of keeping it pending. Building after approval and launch have no
  limit: launch allows import-from-derivation, whose builds can take long, and
  the host user starting it can stop it.
- A candidate's source snapshot is GC-rooted as soon as its preview is ready,
  so garbage collection during approval cannot remove it.

## Deferred

- `goblins devshell diff` (preview without a request).
- Refresh with a daemon `--workspace` snapshot.

- `nix build`, `nix flake check`, `nix fmt`, templates and other flake outputs.
- Dev shell defaults in `mkGoblin`.
- Automatic approval policies beyond ancestor generations (for example
  allowlisted input updates with nothing to build).
- Showing on launch that the workspace flake differs from git `HEAD` or from
  the last trusted snapshot for that path.
