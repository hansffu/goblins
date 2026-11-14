# Handoff: test suite restructuring (branch `test-speedup`)

Written 2026-11-14 by the Claude Code session that did phases 1–6 and part of 7. This directory (`.handoff/`) is untracked; do not commit it. Delete it when the work is done.

## Goal

Make the goblins test suite faster by turning the Python host integration tests (`tests/test_*.py`) into a few **user-workflow** tests, and moving implementation-detail checks to Rust unit tests (and, for kernel-only behaviour, Rust host tests run by `just test-rust-host`). The plan, with per-phase status, is `docs/specs/INTEGRATION-WORKFLOWS.org` — read it first, especially "Decisions", "Workflows", "Unit test work" and "Order of work".

## User decisions (binding)

1. **No backward compatibility at all.** Goblins is experimental; delete old-daemon support, legacy wire forms and their tests freely.
2. **Real agents (real Codex App Server, real Claude) are opt-in.** The default suite uses fakes.
3. **Slow tier**: Gradle, Compose with online networking, Testcontainers are opt-in (`just test-feature slow`, sets `GOBLINS_TEST_SLOW=1`), but the behaviour they rely on stays in the normal suite in a fast form.
4. **Kernel backstops are Rust host tests** (`#[ignore]` tests in `host` modules, run via `just test-rust-host` → `tests/rust_tests.py --host` → `cargo test ... -- --ignored host::`).
5. Integration tests = user workflows; implementation details go to Rust unit tests. Keep things simple even if a reviewer asks for extra machinery; fix real defects only.

The user said "continue with all steps" / "continue until done": no need to ask before continuing phases 7 and 8.

## Process rules (from AGENTS.md + user memory)

- Work inside `nix develop` with the Git-backed flake (never `path:.`). `git add` new files, or the flake build won't see them.
- **One test runner at a time** per checkout. Before starting anything, check `pgrep -fa "tests/(run|tasks).py"`. Run suites with output to a log file.
- Targeted: `just test-plan --changed master`, then `just test-feature FEATURE...`. Before each commit: `just test-merge` (full gate: Rust, Rust host, Python, ERT, Nix checks, packaged smoke). `.handoff/gate.sh LOGFILE` wraps this.
- Keep `tests/features.toml` current (`just test-feature-check`).
- Never fake a pass: don't remove assertions, disable cases or add retries; preserve isolation for lifecycle/authority/cleanup checks.
- Docs prose: one line per paragraph. Update `docs/API.org` (and `libs/api/schema/`) with any RPC change.
- **Review**: every change is reviewed by a goblin of the other model. If you are Claude, use the Codex goblin `speedup-reviewer` (already running on the user's server; if it's gone: `goblins run goblins-codex --detached --name speedup-reviewer`). `.handoff/ask_review.sh REQUEST_FILE REPLY_FILE` sends a file and waits for the host-inbox reply. Send the uncommitted diff (or its path), the intent, and the files to check. Verify each finding before acting; iterate until no findings.
- **Commits**: one commit per logical change (per phase / sub-phase), only after the reviewer has no findings and `just test-merge` passed. Keep review fixes uncommitted until then. Commit as you go, **never push**, no amend. End commit messages with the attribution line your harness gives.
- Responses to the user stand alone; report the change, not the review.
- Inside a goblin, session-starting Python tests fail (Operation not permitted); run them on the host (`goblin host run`).

## State

Branch `test-speedup`, clean working tree at `b85391b`. Commits on top of the plan:

| Commit | Change | Time before → after |
|---|---|---|
| `c5cf7d7` | Flake fix: name rejected launches in the configuration test | |
| `5e13836` | Plan (`docs/specs/INTEGRATION-WORKFLOWS.org`) | |
| `037c176` | Phase 1: dev shells as workflows (29 → 3 tests) | ~130 → ~25 s |
| `a88345c` | Phase 2: Docker as workflows (36 → 6), slow tier | ~115 → ~50 s |
| `573505f` | Emacs TRAMP flake fix | |
| `fd8a3a2` | Phase 3: scopes, network, Nix (17 → 5); `step()` in `tests/support.py` | ~45 → ~20 s |
| `f214dd6` | Phase 4: drop backward compatibility + contract scaffolding | 36 → ~1 s |
| `e3005db` | Phase 5: raids, history, host tools (77 → 13 + 4 short) | ~240 → ~110 s |
| `e1a2e93` | Phase 6: sandbox, exec, terminals (55 → 10) + `just test-rust-host` stage | ~125 → ~57 s |
| `339a7d7` | Phase 7 part: kernel backstops as Rust host tests (`libs/supervisor/src/cleanup/host.rs`, `libs/supervisor/src/entry/host.rs`); removed `session::tests::host_driver`, `rust_driver()`, run.py prebuild, `tests/test_scope_cleanup.py` | |
| `b85391b` | Plan status update (docs only, not reviewed) | |

Python suite: 485 test methods → 222 tests; Python part of the gate ~385 s → ~190 s. Last full gate (at `339a7d7`) passed.

## Remaining work

### Phase 7 rest: daemon and server

Detailed per-test triage: `.handoff/triage-lifecycle.md` (line refs approximate — verify). Before-timings: `.handoff/timings-before.json`. Items already done there: `test_scope_cleanup` and the kernel backstops of `test_scope_process` (fds closed, PDEATHSIG of a stopped supervisor, SO_PEERCRED parent check).

Workflows to write (see the plan's "Daemon and server" section):
- **Server lifecycle**: status/logs/stop with nothing running, start, start again, launch, status counts, logs mode 0600, `server stop` reaps payloads at once with the test process as an idle `PR_SET_CHILD_SUBREAPER`, restart keeps logs.
- **Shell completion**: help and installed completions without a server; live names and resume ids in bash/fish/zsh; a stopped name disappears.
- **Reconfigure without restarting** (from `test_configuration`; keep one bad-config message).
- **Scope data maintenance** (from `test_cleanup`): `scope clear` refused while in use, gc leaves a running goblin alone, dry run then clear of a retired scope, clear after stop, `--all`.
- **New `tests/test_crashes.py`**: "the daemon dies" — fold `test_network`'s server-death test, `test_host_commands` `HostJobCrashTests` (daemon death), `test_raids` `CrashedChainWorkflow` (gc collects the chain); keep Docker's server-death separate if its fixture differs. "One goblin's supervisor dies" — `test_supervised` killing_one + client interrupt + `HostJobCrashTests` supervisor death.
- `test_packages` gains run/list/stop by name and by id (from `test_agent_names`).

Unit tests needed before deleting `test_daemon`, `test_agent_names`, `test_responsiveness`, `test_supervised`, `test_maintenance`, `test_scope_process` (verify each isn't already covered):
- Controller (`packages/daemon` controller tests): re-deciding a refused request; two connections of the same instance + "agent-names"; a permission keeps `agent_name` after name reuse; "already in use"/"conflicting" messages; a session stuck in "starting" blocks nothing.
- Plain frontend `host::decision` parser (plain.rs has no tests); fullscreen stale `y` for a replaced request (check phase 6's TUI tests first).
- Resize before the PTY exists (`libs/supervisor` terminal.rs); "terminal output incomplete" on a failing output writer (`libs/api/src/terminal.rs`).
- `packages/daemon/src/server.rs`: refuses a symlinked log without touching the target; final log line written before the lock is released.
- CLI: `complete_names` under the host-job guard; `run --name` without a value.
- `sandbox.rs`: foreign session request.
- `tests/checks.nix`: resumeCommand cases (verify existing ~1395–1501).
- Maintenance CLI: invalid scope name refused before pinning; SIGINT → "session cancelled", exit 1; TOSTOP/process group (needs a pty).
- Supervisor child side: entry.rs, preflight.rs, supervisor_process.rs, `cleanup.rs` dry-run rules (symlink/hardlink/mode 0/lock preflight/lock inode/top-level symlink/gc name classes — tempdir-based, mostly new).
- Payload `RLIMIT_NOFILE` preserved by the foreground supervisor (unit in spawn, or a cheap `ulimit -n` step in a workflow).

Then delete the six modules, update `tests/features.toml`, compare timings, update phase 7 in the plan, review, gate, commit.

### Phase 8: Codex, Claude, Emacs

Triage: `.handoff/triage-agents-emacs.md`.
- Stop the full gate from setting `GOBLINS_TEST_CODEX` (today `tests/tasks.py` and `tests/test_features.py` expect it); real-Codex tests opt-in (decision 2).
- Workflows: Codex inbox (fake notifier), Codex setup (credentials/rotation, host config untouched, secret hidden), real Codex (opt-in), Claude inbox — drop the 32 s sleep only after confirming a daemon unit test for agent-state heartbeat expiry exists (write it if not); Emacs E1–E4 (profile the 47 s TRAMP test first; edge cases → ERT with stubs).
- New unit infra: scripted fake Codex App Server in Rust for `packages/adapter-codex` (open/approval/enforce/join/catch_up, main.rs ~551–694, likely a `run()` refactor); Nix `runCommand` checks (or Rust ports) for Claude `hook.sh`, Codex `launcher.nix`, `mcp.sh`.
- Drop: nix-skill tests (covered by checks.nix), fake-specific asserts.

Finally: update the plan's status, run `just test-merge`, and report to the user.

## Open items to report to the user (not to fix unasked)

- A live unfree-package error is truncated at 512 chars and loses the word "unfree" (pre-existing product behaviour).
- The Docker on-demand workflow binds a host HTTP server on the default-route address.
- `terminal.sock` listener / legacy attach still open (TODO in `ARCHITECTURE-REVIEW.org`); display fallbacks for malformed current replies kept.
- A plain TUI blocked writing to an unread terminal ignores SIGTERM; tests kill it after 5 s (`tests/terminal_support.py`).
- Leftover agent worktrees: `.claude/worktrees/agent-aaf20e2c07d084cd5`, `agent-abaf74ed22260ff11`, `agent-aee7c754d7366eb98` (at `ac83ec6`, unrelated to this branch).
- Pre-existing: unused import `goblins_jsonrpc as rpc` in `packages/adapter-claude/src/main.rs`; repo not rustfmt-clean at HEAD (don't reformat unrelated code).

## Gotchas learned

- Twice a test run was accidentally started while a gate ran; the overlapping result is invalid. Always `pgrep` first.
- Host tests need deadlines on every child wait (the reviewer flagged unbounded `.output()` / blocking reads); see `entry/host.rs` (pidfd watchdog) and `cleanup/host.rs` (60 s inner run) for the pattern.
- The bind unit test filters targets that don't exist in the Nix sandbox.
- The user's goblins server runs the installed build, not this branch; server-side changes need a restart from the branch build, ask first.
