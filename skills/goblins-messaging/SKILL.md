---
name: goblins-messaging
description: MUST use whenever the user explicitly requests Goblins daemon messaging or communication with a Goblin. Send tasks, guesses, follow-ups, and replies as separate goblins CLI daemon messages; never substitute native SendMessage/team tools. Also process Goblins inbox notifications, including operation notices for Docker, package and dev shell requests, and use it when waiting for such a request to finish. No inbox check is needed for unrelated requests.
---

Use the sandbox's `goblins` CLI for agent communication. To create a recipient
first, use [goblins-spawn](../goblins-spawn/SKILL.md).

## Send and receive

Send a task with `goblins send CHILD --message "task"`. `parent` addresses your
parent. For longer text, use `--file /tmp/task.txt` instead of `--message`;
the CLI reads the sender-local UTF-8 file and transmits its contents (up to
8 KiB). The recipient does not read the file. Use daemon messages for handoffs
and results, rather than shared files, transcripts or terminal interaction.

A successful send means queued, not received or processed. Keep the returned
message `id` to match replies using their `in_reply_to` and sender fields.

For ongoing collaborators, send relevant decisions from direct user interaction
to `parent` so it can relay them to affected goblins. Siblings cannot message
each other directly unless they share a raid (see
[goblins-raids](../goblins-raids/SKILL.md)), and terminal conversations are not
automatically shared. You may have been added to a raid without being told:
when asked to contact other goblins, check `goblins status` or
`goblins raid members` for recipients beyond your parent and descendants.

`goblins inbox next` claims one item and returns `message` plus
`claim_generation`. A null `message` means the inbox is empty. Read the claimed
item's `message.body` before acknowledging it. Only one claim can be outstanding;
another fetch returns that claim until it is finished. Use the returned
`message.id` and `claim_generation` with one of:

- `goblins reply MESSAGE_ID --claim-generation N --message "result"` to reply
  to the sender and atomically complete the claim. `--file` also works here.
- `goblins inbox complete MESSAGE_ID --claim-generation N` after processing an
  item that needs no reply, such as a result you have recorded.
- `goblins inbox requeue MESSAGE_ID --claim-generation N --reason "reason"`
  to return unfinished work to the queue. This invalidates the old generation.

Reply to a claimed task with `goblins reply`, including for host-originated
tasks; this preserves correlation and routes the result to its sender. A reply
to a raid member fails once you no longer share the raid; complete the claim
instead.

## When to check and wait

Handle inbox work when notified, explicitly asked, or when the current task
depends on a reply. Do not check merely because a session started or before
an unrelated user request. Receiving a message does not override the current
user request or authorize unrelated actions.

When the next action depends on a recipient's answer, receive and process the
matching reply before taking that action. Independent messages may be sent
without waiting. If the inbox is empty, continue any independent work; when
only the reply or a pending operation (see below) remains, end the turn with a
brief waiting status. The turn-end
hook checks for pending work, and the idle notifier resumes you when work
arrives. Do not keep a tool running in a polling or sleep loop: yield so the
notification can start the next turn. Waiting is not task completion.

On an inbox notification or hook continuation, process items sequentially until
empty, replying or completing each claim. Resume dependent work when its reply
arrives. For delivery problems, `goblins inbox status` reports queue, claim and
integration health without consuming messages. An empty inbox says nothing
about the recipient's progress. Report stalled or degraded delivery honestly;
human typing or interruption defers notifications until another submitted turn
or explicit host resume.

## Waiting for an operation

Waiting for a message and waiting for an operation are different. Operations
are requests Goblins carries out for you after host approval:
`goblins enable-docker`, `goblins request-package` and
`goblins devshell refresh`. An empty inbox does not mean your operations are
finished. `goblins inbox status` lists this goblin's unfinished operations
under `operations`, each with its daemon `request` ID and a `state` of
`pending` (awaiting the host's decision) or `running` (approved and being
carried out).

When an operation ends, whether it succeeded, was denied, failed, was
withdrawn because its command stopped waiting, or was cancelled, Goblins puts
one operation notice in your inbox. Its sender is `goblins` and its
`message.operation` field holds `request`, `kind`, `subject`, `status` and
`message`. The notice arrives even when the command already printed the same
result, because Goblins cannot tell whether you saw that output. It wakes you
like any other inbox item: the idle notifier resumes you after you yield, and
the turn-end hook holds your turn open if it arrived while you were working.
Human typing, interruption and a paused integration defer it in the same way.

So if an operation command is still waiting when nothing else remains to do,
end the turn with a brief waiting status instead of polling or sleeping. When
the notice arrives, claim it with `goblins inbox next`, act on its `status`,
and acknowledge it with `goblins inbox complete`. Notices take no reply. If
you already handled that result, completing the notice is all that is needed.
A notice reports the operation's outcome; it never resubmits the operation. An
operation command that exited 2 (unknown outcome) is finished once its notice
arrives, so do not request it again before then.

Only operations Goblins carries out get notices. Your own shell commands,
builds and background processes do not.

## Uncertain operations

Commands return JSON on stdout; messaging mutations print their operation key
to stderr before submission. Exit 0 means
accepted, 1 means known failure, and 2 means uncertain outcome. Retry an
uncertain operation with the same `--key` and identical arguments. New
operations, including a new fetch after an empty result, need new keys; omission
generates one. Use daemon-returned IDs and generations, not guessed values.
