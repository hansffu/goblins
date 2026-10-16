# ADR 0010: Recreate goblins from a minimal record instead of restoring them

Date: 2026-10-16

Status: Accepted

## Context

A goblin ends with its server: a stop, a crash, a server restart or a host reboot loses its sandbox, its package grants and the place its agent had reached. Starting it again by hand means approving every package again and finding the agent's conversation. A faithful restore would have to keep the exact manifest, every granted store path rooted against garbage collection, and possibly sandbox state, for every goblin ever run. That costs disk space for goblins that are rarely restored, and it would keep running old configurations after the user changed them.

## Decision

Recreate instead of restore. For each root goblin the server keeps a small JSON record in `$XDG_STATE_HOME/goblins/sessions`: the configuration name, agent name, working directory, scope, dev shell, whether Docker was attached, the approved and denied package names, and variables the goblin records itself. `goblins resume` launches the *current* configuration of that name and resolves the approved names against the current catalog. Recreating is best effort: an incomplete goblin is better than none, so anything that cannot be restored (a package that is gone, broken or fails to download or build, or Docker the scope no longer offers) is skipped with a reason and the goblin starts anyway. Only cancellation aborts. Skipped packages stay recorded, so the next recreate tries them again. Nothing pins store paths: no GC roots, no manifest snapshot, no sandbox state.

The server writes a record whenever it changes, through a writer thread that replaces files atomically, so nothing depends on a clean exit. One server owns the store through a lock; others run without history. The last 100 records are kept by default, and `goblins history prune` removes older ones by age, date or count. Running records are never removed.

Recorded grants are staged before the agent starts: their closures join the initial store view and one package generation per grant reproduces the original PATH priority, so no live grant is needed. They are the recreated goblin's own grants, approved automatically for its children on request, not initial packages that same-configuration children copy. Docker attaches as it does for an inheriting child.

Resuming the agent's conversation is generic. A goblin records variables with `goblins variable set`, and `mkGoblin`'s `resumeCommand` names the agent's arguments for a recreated launch, with `{key}` placeholders for those variables. Claude's hooks and the Codex adapter record the conversation's id and name; their builders resume with `--resume {session_id}` and `resume {session_id}`. A missing variable, or one starting with `-`, falls back to the normal arguments and a fresh conversation. As in Claude Code and Codex, ids are the keys and names only labels. There is one record per conversation line: recreating continues the record instead of writing a new one, it remembers every goblin id it ran as, and a record that holds the same agent conversation as another goblin's is merged into that one. A record cannot be recreated while a goblin runs it, so two goblins never resume the same conversation. Separate records per recreate, linked first by `recreated_from` and then by an `origin`, let an older record of a line be recreated without the grants made after it.

## Consequences

A record costs a few hundred bytes. A recreated goblin may differ from the original: its configuration may have changed, a package may resolve to a newer build or not at all, and the agent's own conversation store decides whether the conversation still exists. Its own error is then what the user sees.

Children, raids, inbox contents, terminal and process state are not recreated. Goblins of a server running with `--workspace` are neither recorded nor recreated, since their snapshot would be lost. Follow-ups are listed in `docs/specs/TODO.org`.
