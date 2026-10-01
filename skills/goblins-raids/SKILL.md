---
name: goblins-raids
description: Use when a Goblins task needs goblins in different branches (not parent and child) to talk directly, or when the user mentions a Goblins raid. Form or join a raid, find members with goblins raid members, and message them through the daemon. Use goblins-messaging for sending and replying.
---

A raid is a temporary group of goblins that may message each other directly,
even across branches of the ownership tree. It only lets members find each
other and send; it shares no files, terminals, packages or lifecycle control.
Communication stays one to one through
[goblins-messaging](../goblins-messaging/SKILL.md).

## When to form one

Form a raid when collaborators outside your parent/descendant line must
exchange messages, for example a reviewer launched by the host and a
developer in another branch, or siblings that would otherwise relay everything
through their parent. Your direct parent and your descendants are always
reachable without a raid. A goblin belongs to at most one raid.

## Am I in a raid?

The host or your parent can add you to a raid without notifying you.
`goblins status` shows your current raid, role and owner, if any. When you are
asked to work with other goblins or with raid members, check
`goblins status` or `goblins raid members` before assuming you can only reach
your parent and descendants.

## Create, invite and launch

- `goblins raid create NAME` creates a raid with you as its owner. It fails if
  the name exists or you are already in a raid. Names use lowercase letters,
  digits and hyphens.
- `goblins raid invite CHILD...` adds your direct children to your current
  raid. Any member may invite its own children. You cannot invite siblings,
  grandchildren or unrelated goblins; ask the host (`goblins raid invite` on
  the host) or have their parent invite them.
- `goblins run CONFIG --name CHILD --detached --inherit-raid` launches a child
  that joins your raid. It still follows `allowedChildren`.
- A goblin already in another raid is never moved; the invite fails instead.

## Find members and message them

`goblins raid members` lists each member's name, role and full tree path,
followed by configuration, state, owner and ID. Send with the path or ID when a
name is ambiguous:

```sh
goblins raid members
goblins send chief/review --message "The build passes; please review."
```

Roles are not addresses: `goblins raid set-role ROLE` sets or changes your own
role (for example `builder` or `reviewer`) so others can pick a recipient. A
role grants nothing and cannot be cleared. Ownership never moves by role.

## Leaving and dissolution

`goblins raid leave` removes you from the raid. When the owner leaves, stops
or exits, the raid dissolves for everyone; no goblin is stopped by that. After
you no longer share a raid (and are not parent/descendant), new sends to that
goblin fail. Messages already delivered stay in your inbox, but a
`goblins reply` to a raid member who is gone fails as "not allowed"; complete
the claim with `goblins inbox complete MESSAGE_ID --claim-generation N`
instead, and report the result another way if still needed.
