# Agent instructions

These instructions apply to both Codex and Claude Code (`CLAUDE.md` imports this file).

## Testing

Work inside `nix develop`. Use the Git-backed flake, not `path:.`, so ignored build artifacts are not copied into the Nix store.

Pick the feature tasks a change touches with `just test-plan --changed BASE`, using the branch you will merge into, or `just test-plan --paths FILE...`. Unknown paths and shared protocol, lifecycle or fixture code recommend `full`; include every affected feature. `just test-features` lists the tasks.

Before presenting work, run those tasks in one invocation with `just test-feature FEATURE...`, and state what passed, what failed or was skipped, and whether the merge gate has passed, failed or is still pending.

Before merge, run `just test-merge` on the final candidate. It is required and runs every Rust, Python and Emacs test, the Nix checks and the packaged smoke tests. Targeted passes do not count, and any change after the run needs a new run.

Documentation-only changes are exempt from both: a change that touches only documentation (the `docs` paths in `tests/features.toml`, such as `docs/`, `README.org`, `AGENTS.md` and `CLAUDE.md`) needs no feature tasks before review and no `just test-merge` before merge. Say that no tests were run because the change is documentation only. This exception takes precedence over the older wording in the `docs` task description and `just test-plan` output, which still say the full gate is required before merge.

Never fake a pass: do not remove assertions, disable cases or add retries, and preserve isolation for lifecycle, authority and cleanup checks. Report skipped Docker or Testcontainers checks; when you change those integrations, run them before merge or report the blocker.

Run one test runner at a time per checkout and cache. This applies across the whole raid: members share the checkout and cache, so coordinate through daemon messages before starting a runner. Other agents using the same checkout count too. Do not raise Nix build concurrency to match test workers.

Keep `tests/features.toml` current when adding tests or features; `just test-feature-check` finds unmapped tests and stale selectors.

## Documentation

Write documentation prose with one line per paragraph; preserve the structure of lists, tables and code blocks.

`docs/API.org` documents every JSON-RPC method, parameter, result, notification, error and record field. Update it in the same change as any RPC API change, and treat a missing update as a review defect.

## Review

Every change is reviewed by a goblin running a different model: Claude is reviewed by a `goblins-codex` goblin, and Codex by a `goblins-claude` goblin. Configuration keys differ between setups (for example `codex` and `claude`): run the other agent's key from the run targets `goblin status` lists, and identify members by the `CONFIGURATION` column of `goblin raid members`.

Reuse a raid member of the other model whose role is `reviewer`. If there is none, run one as raid owner and have it set its role with `goblin raid set-role reviewer`; if you are not the owner, ask the owner.

Send the reviewer the uncommitted diff, the intent and the files to check against. Verify each finding before acting on it, and send fixes back until the reviewer has no remaining findings.

## Commits

Commit a change only after its reviewer has no remaining findings. Until then, keep the change and its review fixes uncommitted, so review rounds never add commits and the history shows the change, not how it was reviewed. Commit each logical change on its own; a change the user asks for after review is a new change, with its own review and commit.

## Responses to the user

The user reads the latest response and should not need to scroll back. Every response, including one started by an inbox message such as a review reply, stands on its own: it describes the work the user asked for as it now is, with what changed, why, the test results and what is still pending.

Report the change, not the review. After a review round, do not answer with "the reviewer found issues and they have been fixed"; present the reviewed change itself, as if presenting it for the first time. Mention review only as its outcome (reviewed by whom, whether anything remains open), and raise individual findings only when they need the user's decision or were not resolved.
