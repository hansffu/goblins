# ADR 0019: Git worktrees through one goblin command

Date: 2026-11-06

Status: Proposed. The plan is [WORKTREES.org](../specs/WORKTREES.org).

## Context

Several goblins often work on one repository at the same time and overwrite each other in one checkout. A goblin started in a linked worktree cannot run Git today, because only its working directory is mounted and the main `.git` is not. Nothing tells Emacs or resume which worktree a goblin works in. The user wants to ask a goblin for a worktree mid-task, and to work in it natively on the host.

## Decision

- Worktrees are real `git worktree`s at `DIRECTORY/REPO/NAME`, with `mkGoblins.worktrees.directory` configuring `DIRECTORY`.
- A goblin launched at a checkout's root gets the repository's folder read-write. A goblin launched in a worktree also gets the checkout, after a back-pointer check.
- `goblin worktree NAME` creates the worktree if it is missing, using Git inside the sandbox. It records the goblin's worktree, which the daemon publishes to host clients, and prints the path.
- History keeps the worktree for resume. Agents get a skill, not hooks.

## Alternatives considered

An earlier draft added a repository-key registry, live tracking through Claude Code hooks, per-agent tracking modes and removal checks. They handled edge cases the user does not need, at the cost of most of the complexity, and were dropped.

## Consequences

Goblins in a repository can read and change all of its worktrees. Two checkouts with the same basename share a folder. Worktrees made with an agent's own worktree feature are not tracked.
