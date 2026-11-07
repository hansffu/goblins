---
name: goblins-worktrees
description: Use when work should happen on its own Git branch in its own directory, when several goblins edit one repository at once, or when GOBLINS_WORKTREES is set. Create or enter a Git worktree with goblin worktree NAME, work in the path it prints, and start raid members in one with goblin run --worktree. Do not use Claude Code's --worktree or EnterWorktree, or Codex's own worktree features: Goblins does not track them.
---

A worktree is a second working directory of the same repository on its own branch, so parallel lines of work do not overwrite each other's files.
Goblins keeps a repository's worktrees in one folder, `$GOBLINS_WORKTREES`, at `$GOBLINS_WORKTREES/NAME`, and the branch of a worktree is named `NAME`.
If `$GOBLINS_WORKTREES` is unset, this goblin has no worktree support: it was not launched at a repository's root or in one of its worktrees, or its configuration has worktrees off.
Then `goblin worktree` fails, and the session detail may say why.

## Work in a worktree

Run `goblin worktree NAME` inside the repository.
It creates `$GOBLINS_WORKTREES/NAME` if it is missing, on branch `NAME` (an existing branch is checked out, a new one starts at `HEAD`, or at `--base REF`), records it as the worktree you work in, so the user's editor and `goblins resume` follow you, and prints its absolute path.
Run it again for an existing worktree and nothing is created.
Names use letters, digits, `.`, `_` and `-`, start with a letter or digit, and are at most 100 characters.

Your process keeps running in its launch directory: work in the printed path, with `git -C PATH ...`, `cd PATH && ...` and absolute paths in file edits.
To work in the main checkout again, run `goblin worktree --main`: it clears the record and prints the main checkout's path (also in `$GOBLINS_CHECKOUT`), which you then work in the same way.
The branch the main checkout has, such as `main` or `master`, cannot be a worktree: when asked to work on it, run `goblin worktree --main` instead of `goblin worktree main`.

Do not use Claude Code's `--worktree` or EnterWorktree, or Codex's own worktree features (`create_worktree`, `--worktree`): they put worktrees where Goblins does not look and do not tell it where you work.
Do not use plain `git worktree add` for a worktree Goblins should know about: use `goblin worktree NAME`.

## Others

The main checkout is mounted too, and every worktree of the repository is visible to every goblin of that repository, other raids' included.
Do not change a worktree another goblin works in.

As a raid owner, `goblin run CONFIG --worktree NAME` starts a member whose working directory is `$GOBLINS_WORKTREES/NAME`.
It fails if `NAME` is not a worktree in your folder.
Members without `--worktree` inherit your working directory.

Listing and removing are plain Git: `git worktree list`, and `git worktree remove PATH` once nobody works there.
The branch stays; delete it yourself once it is merged.
Run `goblin worktree --main` if you removed your own worktree.
