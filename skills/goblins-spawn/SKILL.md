---
name: goblins-spawn
description: MUST use when the user says Goblin, Goblins, the goblin or goblins CLI, or Goblins daemon, including requests for a goblin subagent. Run new Goblins sandboxes as members of your raid and manage them; never substitute a native Agent/team for an explicitly requested Goblin. Use goblins-messaging for coordination.
---

A goblin is a separate native agent in a daemon-owned sandbox. To delegate to
a new goblin, run it as a member of a raid you own with the sandbox's `goblin`
CLI. There are no child goblins: every goblin is a peer, and the raid is what
connects you.

## Choose the delegation mechanism

Respect an explicit choice of goblins or native subagents. When either would
fit, prefer native subagents for bounded helper work and goblins for ongoing
collaborators the user may attach to and interact with throughout a task.
Messaging or requesting tools alone does not require a separate goblin.

For example, a task spanning several repositories may benefit from an
architect and one developer goblin per application. Keep these roles available
across implementation, review and follow-up decisions. Assign clear repository
and file ownership: members you run share your workspace, so all needed
repositories must already be accessible. A member is not a separate checkout.

## Run a member

Only a raid owner runs goblins. If `goblin status` shows no raid, create one
with `goblin raid create NAME` (lowercase letters, digits and hyphens); you
are its owner. If you are a member of someone else's raid, you cannot run
goblins: ask its owner, or the user.

Pick a configuration from the run targets `goblin status` lists, then run
`goblin run CONFIG --name NAME`, substituting that configuration and a name.
It returns at once with the new goblin's session ID and agent name; `starting`
acknowledges the launch, not task execution. Agent names are unique among
running goblins and are how you address them.

A member shares your workspace, working directory, scope, dev shell and Docker
attachment, but runs its own configuration and access (for example another
agent type); it gains no access for you. Packages you were approved before you
ran it are approved for it on request without asking the user; packages you
get afterwards are not.

Read [goblins-messaging](../goblins-messaging/SKILL.md) to send the member its
task and handle its replies through the daemon. You and your members share the
raid, so you message each other by agent name. Provide the role, relevant
context, repository paths and expected coordination; a launch alone does not
assign work or transfer your conversation. Goblins never attach to each
other's terminals. For direct interaction, give the user the session ID for
`goblins attach SESSION_ID` on the host.

User conversations in one goblin do not automatically reach the others. Relay
relevant task decisions through daemon messaging. See
[goblins-raids](../goblins-raids/SKILL.md) for roles and what happens when an
owner leaves.

## Lifetime and cleanup

Keep collaborators alive while the overall task still needs their role, even
after they finish an individual assignment. Ending a turn or detaching leaves
the session available for follow-up. A delayed reply alone is not a reason to
stop or replace it. Members keep running after you exit; exiting does not
clean them up.

When the overall task is complete, collect final replies and preserve needed
results before stopping members created solely for that task with
`goblin stop NAME`. `goblin list` shows the members you can stop: those created
in the raid you own. Preserve sessions the user wants for continued
interaction, and report which remain. Stopping loses live state and private
temporary files; let the daemon release resources rather than deleting state
files.
