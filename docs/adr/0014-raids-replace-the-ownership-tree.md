# ADR 0014: Raids replace the ownership tree

Date: 2026-10-18

Status: Accepted. Supersedes the parent/child parts of [ADR 0008](0008-raids.md) and its rule that raid ownership never moves. The plan is [RAIDS-REPLACE-CHILDREN.org](../specs/RAIDS-REPLACE-CHILDREN.org).

## Context

Goblins had two ways to relate goblins: the ownership tree (a parent launches children, can attach to and stop its subtree, messages its descendants, and its exit stops them) and raids (a communication-only group across branches). Collaborators were usually launched as children and then joined to a raid anyway, so every goblin carried both relations, and a coordinator exiting took its whole team with it.

## Decision

There is no ownership tree. Every goblin is a root, addressed by its agent name.

- A goblin that owns a raid runs new goblins with `goblin run CONFIG`. The configuration must be in the owner's `allowedRunTargets` (formerly `allowedChildren`), and is resolved from the daemon's current manifest: the last validated manifest the host used. The new goblin joins the raid as a *created member* and inherits the owner's scope, workspace, working directory, dev shell generation and Docker attachment, as children did.
- The raid owner can list and stop members created in its raid; a new owner takes that over. It cannot stop host-invited members, and no goblin attaches to another goblin's terminal. A created member cannot leave the raid on its own.
- A created member gets an *auto-approve* set: the owner's approved packages and auto-approve set, copied at creation. A package in it is granted on request without asking the host. Later grants to the owner do not reach existing members.
- When the owner leaves the raid in any way and members remain, the raid is *paused*: members keep running, sends and replies between them are refused with a warning, and the host is asked to pick a new owner, dissolve the raid, or stop all members. Warned senders are notified when the raid resumes.
- Two guarantees replace ADR 0008's single authorization rule. Message routing uses only membership recorded in the communications log: a pause takes effect at once, a new owner routes only once logged. Control rights (running, listing and stopping created members) use the controller's raid state on every call: revocation is immediate, and a new owner's rights start when the host's choice is committed.

## Alternatives considered

- **Keep children next to raids.** Two overlapping relations with different lifecycle, messaging and control rules; raids already covered the collaboration case.
- **Dissolve the raid when the owner leaves (ADR 0008).** Without a tree, nothing would remain to coordinate or stop the created members, so the host chooses instead.
- **Let the owner attach to created members' terminals.** Attachments outlive the authorization check, so each would need revoking whenever ownership or membership changes. The host can attach to any goblin.
- **Resolve created members from the owner's launch-time snapshot.** Immune to rebuilds, but needs snapshot roots that outlive the owner, and a rebuild would never reach a long-running raid.

## Consequences

An owner exiting no longer stops anyone. Every goblin gets a history record and can be resumed. Goblins can only message the host and their raid. Created members follow the host's latest validated manifest, which can lag behind the installed build until the next host launch or server restart. Approvals copied at creation are fixed for that member, and history records move to version 2 with an auto-approve list.
