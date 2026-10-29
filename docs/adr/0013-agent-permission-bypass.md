# ADR 0013: Agents keep their own permission prompts unless a configuration bypasses them

Date: 2026-10-17

Status: Accepted.

## Context

The Claude Code and Codex launchers always skipped the agents' own permission prompts (`--dangerously-skip-permissions`; `--ask-for-approval never`), on the grounds that Goblins is the sandbox. With `allowNix` and host commands, a goblin's sandbox is much weaker, and the human wants the agents' prompts as a second check unless a configuration says otherwise.

## Decision

`bypassPermissions` is one boolean on every agent goblin, default `false`, independent of Nix, host commands and scopes. `true` restores the old launch flags. `false` means Goblins requests no bypass: the agent's own settings decide, including a bypass the user configured in them. It is not an enforced policy. A plain `mkGoblin` refuses `true`, since it has no agent prompts. The manifest records it, and `goblin status` shows what the configuration requests, not the agent's effective mode.

Finer control stays in each agent's own terms (`claudeSettings.permissions.defaultMode`, `codexSettings.approval_policy`), since the agents' permission models do not map onto one another.

Codex's own command sandbox needs namespaces, which a goblin lacks, so Codex always runs with full access, and only its approval policy follows the option. Codex's retired `untrusted` policy is not used; without bypass the policy is Codex's own, by default `on-request`, which with full access leaves asking to the model. A Codex thread keeps the approval policy it was saved with, so the adapter sets the configured policy (and full access) on the terminal's resume and fork requests, reading Codex's configuration for the directory when the request names none. Claude Code takes its mode from the launch on every start, including `--resume`.

## Consequences

Existing configurations start prompting after an upgrade until they set `bypassPermissions = true`. A Codex goblin without bypass prompts much less than a Claude Code one. A Codex thread that is still loaded in its App Server keeps its live policy.
