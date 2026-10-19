# Testing and review workflow

These instructions apply to both Codex and Claude Code (`CLAUDE.md` imports
this file). The feature definitions and source mappings live in
`tests/features.toml`; keep that file current when adding tests or features.

## Review and merge are separate checkpoints

- Before presenting work for review, run the relevant feature tasks and any
  focused regression checks. You may present a result before the full suite
  has run. State exactly what passed, what failed or skipped, and that the
  full merge gate is pending when it has not passed.
- **A full test-suite pass is required before merge.** Run `just test-merge`
  on the final candidate. It checks feature coverage, the complete Rust and
  Python suites, all Emacs ERT tests, Nix checks and packaged smoke tests.
  Targeted passes do not satisfy this requirement. Changes after validation
  must be covered before merging; do not claim an earlier pass covers changed
  executable code, tests or configuration.
- Codex configuration tests are enabled automatically by the `codex` and
  `full` tasks. Host-Docker and external Testcontainers checks retain their
  existing opt-ins. Report skips; when changing those integrations, arrange
  the required environment and run them before merge, or report the blocker.
- Do not remove assertions, disable cases or add retries to obtain a pass.
  Preserve isolation for lifecycle, authority and cleanup checks.

## Pick the checks

Enter `nix develop` once and run commands inside it. Use the Git-backed flake,
not `path:.`, so ignored build artifacts are not copied into the Nix store.

```sh
just test-features                           # available feature tasks
just test-plan exec history                  # preview commands and test count
just test-feature exec history               # one union, no duplicate tests
just test-plan --paths crates/controller/src/exec.rs
just test-plan --changed BASE                # merge-base, worktree and untracked files
just test-feature -j 4 exec                   # optional four workers; default is two
just test-feature-check                      # no unmapped tests or stale selectors
```

Use `--changed` with the actual branch you will merge into, not just `HEAD`,
when you need to cover already committed branch changes. Path recommendations
are conservative, not a dependency proof: include every affected feature and
cross-feature behavior. Unknown paths and shared authority/protocol/lifecycle
or fixture code recommend `full`. For review of such a change, a justified
targeted selection is still allowed; explain its limits and leave the full
gate pending. Documentation-only review needs no runtime tests.

| Changed behavior | Feature tasks |
| --- | --- |
| Filesystem, binds, snapshot, GC retention | `sandbox` |
| Daemon management, configuration, names, responsiveness | `lifecycle` |
| Exec, signals, terminal attachment | `exec` |
| Shared namespaces, networking, storage | `scopes`; add `docker` for engine behavior |
| Sandbox engines and Docker integrations | `docker` |
| Nix access, devshell launch/refresh/diff, flake run | `devshell` |
| Raids, ownership, members, run targets | `raids` |
| Mailbox, activity, completion notices | `messaging`; add the affected agent integration |
| History, recreation and resume | `history` |
| Codex adapter, configuration and hooks | `codex` |
| Claude configuration and hooks | `claude` |
| Host commands and editor requests | `host-tools` |
| Terminal UI | `tui` |
| Emacs Lisp and live frontend integration | `emacs` |
| Runner, selection and test harness | `infrastructure`, plus affected integration smoke checks |
| Nix packaging, installed launchers and completions | `packaging`, plus affected runtime features |

Feature tasks run their matching Python integration tests and language checks.
Rust features use the complete fast Rust suite; `emacs` also runs all ERT
tests. During edits, individual Python modules/classes/methods and
`just test-emacs` remain useful. Before review, expand to the relevant feature
tasks when a change crosses those narrower boundaries.

Submit multiple features in one invocation. Keep class fixtures together and
let the runner manage exclusive engine/frontend groups. Do not launch separate
runners concurrently against the same checkout/cache. Nix fixture builds stay
serialized; do not increase their concurrency to match test worker count.

Useful complete commands: `just test` runs Rust/Python/ERT; `just test-merge`
is the complete required merge gate. `just test-merge -j 4` uses four Python
workers while retaining the existing Cargo/Nix limits.
