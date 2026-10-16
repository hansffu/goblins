# ADR 0010: Recreate goblins from a minimal record instead of restoring them

Date: 2026-10-16

Status: Accepted

## Context

A goblin ends with its server: a stop, a crash, a server restart or a host reboot loses its sandbox, its package grants and the place its agent had reached. Starting it again by hand means approving every package again and finding the agent's conversation. A faithful restore would have to keep the exact manifest, every granted store path rooted against garbage collection, and possibly sandbox state, for every goblin ever run. That costs disk space for goblins that are rarely restored, and it would keep running old configurations after the user changed them.

## Decision

Recreate instead of restore. For each root goblin the server keeps a small JSON record in `$XDG_STATE_HOME/goblins/sessions`: the configuration name, agent name, working directory, scope, dev shell, whether Docker was attached, the approved and denied package names, and variables the goblin records itself. `goblins recreate` launches the *current* configuration of that name and resolves the approved names against the current catalog; names that no longer resolve are skipped and reported. Nothing pins store paths: no GC roots, no manifest snapshot, no sandbox state.

The server writes a record whenever it changes, through a writer thread that replaces files atomically, so nothing depends on a clean exit. One server owns the store through a lock; others run without history. The last 100 records are kept by default, and `goblins history prune` removes older ones by age, date or count. Running records are never removed.

Recorded grants are staged before the agent starts: their closures join the initial store view and one package generation per grant reproduces the original PATH priority, so no live grant is needed. They are the recreated goblin's own grants, approved automatically for its children on request, not initial packages that same-configuration children copy. Docker attaches as it does for an inheriting child.

Resuming the agent's conversation is generic. A goblin records variables with `goblins variable set`, and `mkGoblin`'s `resumeCommand` names the agent's arguments for a recreated launch, with `{key}` placeholders for those variables. Claude's hooks and the Codex adapter record the conversation's id and name; their builders resume with `--resume {session_id}` and `resume {session_id}`. A missing variable, or one starting with `-`, falls back to the normal arguments and a fresh conversation. A record is reserved while it, an ancestor or a descendant has a live goblin, so two goblins never resume the same conversation.

## Consequences

A record costs a few hundred bytes. A recreated goblin may differ from the original: its configuration may have changed, a package may resolve to a newer build or not at all, and the agent's own conversation store decides whether the conversation still exists. Its own error is then what the user sees.

Children, raids, inbox contents, terminal and process state are not recreated. Goblins of a server running with `--workspace` are neither recorded nor recreated, since their snapshot would be lost. Follow-ups are listed in `docs/specs/TODO.org`.
