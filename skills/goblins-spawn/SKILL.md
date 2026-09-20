---
name: goblins-spawn
description: Create and manage child Goblins sandboxes for explicit goblin requests or long-lived collaborators with individually attachable agent sessions. Use for persistent roles across a task; use goblins-messaging for their coordination.
---

A goblin is a separate native agent in a daemon-owned sandbox. For a Goblins
delegation, create a child with the sandbox's `goblins` CLI.

## Choose the delegation mechanism

Respect an explicit choice of goblins or native subagents. When either would
fit, prefer native subagents for bounded helper work and goblins for ongoing
collaborators the user may attach to and interact with throughout a task.
Messaging or requesting tools alone does not require a separate goblin.

For example, a task spanning several repositories may benefit from an
architect and one developer goblin per application. Keep these roles available
across implementation, review and follow-up decisions. Assign clear repository
and file ownership: children share the workspace and inherit the parent's
configuration and access, so all needed repositories must already be accessible.
A child is not a separate checkout and gains no additional host access.

## Create and coordinate

Read `goblins status` to get the current configuration name, then run
`goblins run CONFIG --name CHILD --detached`, substituting that configuration
and a child name. Children inherit the loaded configuration and share the
parent's workspace. `--detached` leaves the child running without attaching its
terminal. Use the returned identity; a `starting` response acknowledges the
launch, not task execution.

Read [goblins-messaging](../goblins-messaging/SKILL.md) to send the child's task
and handle its replies through the daemon. Provide the role, relevant context,
repository paths and expected coordination; a launch alone does not assign work
or transfer the parent's conversation. Give the user the returned session ID
for `goblins attach SESSION_ID` when direct interaction is useful.

User conversations in one goblin do not automatically reach the others. Relay
relevant task decisions through daemon messaging. Siblings cannot send directly
to each other; use the parent to coordinate between them.

## Lifetime and cleanup

Keep collaborators alive while the overall task still needs their role, even
after they finish an individual assignment. Ending a turn or detaching leaves
the session available for follow-up. A delayed reply alone is not a reason to
stop or replace it. Exiting the parent sandbox stops its descendants.

When the overall task is complete, collect final replies and preserve needed
results before stopping children created solely for that task with
`goblins stop SESSION_ID`. Preserve sessions the user wants for continued
interaction, and report which remain. Stopping loses live state and private
temporary files. Use `--kill-children` only when all descendants are also ready
for cleanup; let the daemon release resources rather than deleting state files.
