# ADR 0008: Raids are a communication-only grouping

Date: 2026-10-01

Status: Accepted

This decision extends the messaging rules of
[ADR 0001](0001-daemon-inboxes-for-agent-communication.md). It is a peer to
[ADR 0006](0006-shared-scopes.md): scopes group goblins that share resources;
raids group goblins that talk to each other. The functional and technical
plans are [CHILDREN-AND-RAIDS.org](../specs/CHILDREN-AND-RAIDS.org) and
[CHILDREN-AND-RAIDS-TECHNICAL.org](../specs/CHILDREN-AND-RAIDS-TECHNICAL.org).

## Context

Daemon messaging follows the ownership tree: a goblin can message its parent
and its descendants. Collaborators in different branches, such as a reviewer
the host launched separately and a developer under a coordinator, or siblings,
have to relay every exchange through a common ancestor or the host. That adds
latency and puts the relay in the middle of work it has no part in.

Widening the tree rule (for example to siblings) would give every goblin more
reach than most tasks need. Sharing a scope is also not the right tool: a scope
is a resource trust group (namespaces, storage, Docker) and says nothing about
who may talk to whom.

## Decision

A **raid** is a named, temporary group of live goblins. Membership only lets
members find each other (`raid members`) and exchange ordinary one-to-one
daemon messages across branches. It grants nothing else: no files, terminals,
packages, lifecycle control, or reading another member's inbox.

- A goblin belongs to at most one raid. The first joiner owns it; ownership
  never moves, and roles are descriptions only. The owner leaving, stopping or
  exiting dissolves the raid; dissolution never stops a goblin.
- The host can invite any goblins and launch into a raid. A member can create
  a raid, invite only its direct children, or launch a child into its raid.
  Recruiting stays inside the ownership tree the caller already controls.
- Raid state lives in the controller's event loop and is never persisted.
- Authorization uses only membership the messaging worker has written to the
  communications log. The controller sends explicit, ordered, attributed
  events; a join authorizes nothing until it is recorded, while removals always
  take effect. If raid events cannot be recorded, raid routes close and tree
  routes keep working.
- Delivered messages stay with their recipient. A reply is authorized like a
  send, so it fails once the pair shares neither a raid nor a tree relation;
  the recipient can still complete the claim.

## Alternatives considered

- **Allow siblings to message each other.** Simpler, but it widens every
  goblin's reach permanently and still does not connect separate branches.
- **Route by role or broadcast to the raid.** Convenient for coordination, but
  it changes messaging from explicit one-to-one exchanges into group delivery
  with harder audit and retry semantics. Out of scope for the first version.
- **Authorize from the controller's membership directly.** The log could then
  lack the membership that allowed a message. Authorizing from logged events
  keeps every raid-routed message explainable from the log alone.

## Consequences

Cross-branch collaborators message each other directly, and the host can see
from each `message.accepted` whether a raid was its only authorization
(`route: "raid"`). Raids disappear with the daemon; nothing needs cleanup or
migration. Replies to a former raid member fail visibly rather than silently
reaching someone the sender can no longer address. Ownership transfer, member
removal, broadcasts, role routing and shared raid resources remain future
work.
