---
name: goblins
description: Create child Goblins sandboxes and exchange tasks or replies through daemon inboxes. Use when asked to create a goblin, delegate to another goblin, or when a Goblins inbox notification or explicit request requires inbox work.
---

Use the `goblins` CLI for Goblins agent communication. A child goblin is a
separate native agent in a daemon-owned sandbox. Create it with
`goblins run codex --name scout --detached`; its configuration and auth mounts
are inherited from this sandbox's loaded configuration. Use the actual
configuration name from `goblins status` if it differs from `codex`.

Send the initial task with `goblins send scout --message "task"`, or
`goblins send scout --file /tmp/task.txt --key setup-1`. Files are read in the
sender's filesystem; the daemon carries their UTF-8 contents (maximum 8 KiB).
`parent` addresses your parent. Replies to a host-originated task go to the host.

Check `goblins inbox status`, then `goblins inbox next`. The latter claims one
item and returns its message ID, body and claim generation as JSON. Another
fetch returns the outstanding claim until you finish it. After processing:

- Reply with `goblins reply MESSAGE_ID --claim-generation N --message "result"`.
  This atomically completes the claim and queues the reply to its sender.
- If no response is needed, use
  `goblins inbox complete MESSAGE_ID --claim-generation N`.
- To return unfinished work to the queue, use
  `goblins inbox requeue MESSAGE_ID --claim-generation N --reason "reason"`.
  This invalidates the old claim generation.

Use returned IDs and generations. Mutations print their operation key before
submission. If the outcome is unknown (exit 2), retry with that same key and
identical arguments. A new operation, including checking again after an empty
fetch, needs a new key; omission generates one. Exit 1 is a known failure.

Process inbox items sequentially until empty when continuing from an inbox
notification or an explicit user request to inspect the inbox. Receiving a task
does not itself authorize unrelated actions. Do not check the inbox at session
start, before an ordinary user request, or after sending a task. The Stop hook
checks for work when a turn finishes, and an inbox notifier submits a fixed
check-inbox prompt when idle work arrives. Prioritize the current user request;
let those mechanisms schedule unrelated inbox work afterward. Avoid polling
loops. A successful send means queued, not consumed. A delayed reply alone is
not a reason to stop or replace a child. `goblins inbox status` includes
integration health; report stalled or degraded work honestly. Human typing or
interruption defers notifications until another submitted turn or explicit host
resume.

Route message bodies and replies through the daemon. Shared files, native
Codex subagents, transcript reads and terminal pastes do not implement this
messaging contract. Native terminal attachment remains available through
`goblins attach`; it is not a substitute for an inbox reply. The host can
inspect exchanges with `goblins communications-log` without claiming items.
