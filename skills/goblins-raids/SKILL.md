---
name: goblins-raids
description: Use when Goblins work involves other goblins, or the user mentions a Goblins raid. Form a raid, run members into it, find members with goblin raid members, handle a paused raid, and message them through the daemon. Use goblins-messaging for sending and replying.
---

A raid is a temporary group of goblins that may message each other directly.
Goblins outside your raid are not reachable, except the host. Its owner can run
new members and stop the members created in it; membership shares no files,
terminals or packages by itself. Communication stays one to one through
[goblins-messaging](../goblins-messaging/SKILL.md).

## Am I in a raid?

The host can add you to a raid without notifying you, and a raid owner may
have run you into its raid. `goblin status` shows your current raid, role and
owner, if any, and whether the raid is paused. When you are asked to work with
other goblins, check `goblin status` or `goblin raid members` before assuming
you cannot reach them. A goblin belongs to at most one raid.

## Create and run members

- `goblin raid create NAME` creates a raid with you as its owner. It fails if
  the name exists or you are already in a raid. Names use lowercase letters,
  digits and hyphens.
- As owner, `goblin run CONFIG --name NAME` runs a new goblin that joins your
  raid as a created member (see [goblins-spawn](../goblins-spawn/SKILL.md)).
  It follows your configuration's run targets.
- `goblin list` shows the members created in your raid and `goblin stop NAME`
  stops one. You cannot stop members the host invited.
- Only the host invites goblins that already run, and a goblin already in
  another raid is never moved.

## Find members and message them

`goblin raid members` lists each member's name and role, then configuration,
state, owner, whether a raid owner created it, and ID. Send with the name, or
the ID:

```sh
goblin raid members
goblin send review --message "The build passes; please review."
```

Roles are not addresses: `goblin raid set-role ROLE` sets or changes your own
role (for example `builder` or `reviewer`) so others can pick a recipient. A
role grants nothing and cannot be cleared. Ownership never moves by role.

## When the owner leaves

When the owner leaves, stops, exits or is removed while members remain, the
raid is paused until the user picks a new owner or closes it. Members keep
running and can keep working on their own, but sends and replies between them
are refused with a "paused" warning. A goblin that got that warning receives a
notice from `goblins` once a new owner is selected; then it can send again.
Complete that notice with `goblin inbox complete`; it takes no reply. If you
are selected as the new owner, you can run and stop the members created in the
raid. Do not keep retrying a refused send: continue independent work, or end
the turn and wait for the notice.

## Leaving and dissolution

`goblin raid leave` removes you from the raid; when the owner leaves, the raid
pauses. A member created by a raid owner cannot leave on its own: it stays
until it is stopped, the user removes it, or the raid dissolves. The raid
dissolves when its last member leaves or the user closes it; no goblin is
stopped by that unless the user asks. After you no longer share a raid, new
sends to that goblin fail. Messages already delivered stay in your inbox, but
a `goblin reply` to a goblin you no longer share a raid with fails as "not
allowed"; complete the claim with
`goblin inbox complete MESSAGE_ID --claim-generation N` instead, and report
the result another way if still needed.
