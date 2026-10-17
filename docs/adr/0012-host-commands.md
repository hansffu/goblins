# ADR 0012: Run approved host commands under a subreaper supervisor

Date: 2026-10-17

Status: Accepted. Security-reviewed on 2026-10-17; the decisions are under Consequences.

## Context

Developing Goblins from a goblin needs tests a sandbox cannot run: Goblins' own integration and Docker tests use Bubblewrap, namespaces and UID/GID mappings that the payload's seccomp filter and single-ID mapping deny. Giving the agent's sandbox all of those permissions would undo what it isolates. `goblins exec` runs commands inside a goblin, so it does not help.

## Decision

A goblin whose configuration sets `hostCommands.enable` may ask the host to run a command outside its sandbox, with `goblin host run`. Each run is a `host-command` permission request, approved through the same queue, approval views and operation records as a package request; nothing is approved automatically. The daemon resolves the request's directory before it is shown, beneath the pinned source of the shared working directory or a directory grant, so the approval names the host directory and the job runs exactly what was approved. The approval slot is held only while the request is pending; a running job has its own table.

The job runs as the user, with no shell, in the server's environment plus the request's variables, with `GOBLINS_HOST_JOB` set and `TMPDIR` unset (nested daemons and `nix develop` put sockets beneath it, and those paths have a 107-byte limit). It is supervised by the helper's `--host-job` mode: a subreaper that copies output to a capped log, records the outcome, and on every ending (the command exiting, cancellation, a timeout, its goblin stopping, or the server dying) stops the command's process group and every process it left behind before it reports. The daemon controls it only through a lifetime pipe: closing it stops the job, and it closes by itself when the server dies.

Output goes to the session directory, which the sandbox sees read-only at `/run/goblins/host-jobs`, so the client tails a file and agents can read logs with ordinary tools. An attached run waits for the outcome and exits with the job's code; interrupting it stops the job. A detached run returns at once and its outcome arrives as an operation notice, so an agent need not hold a tool call open past its time limit. Inside a host command the host CLI refuses its default server without `--state-dir`, since that server runs the goblin that asked.

The interface is a CLI command with a skill that explains it, like the other Goblins features, not an MCP tool.

## Consequences

After the security review the human accepted that an approved command runs whatever the shared files say, also after approval, and kept the daemon's environment and arbitrary request variables, which reach the command only. Which goblins can ask is configuration: `hostCommands.enable` on a configuration, and `allowedChildren`, which now defaults to no children, decides which configurations a goblin can start.

A goblin with host commands can do anything the user can, once each command is approved. The approval view is the control, which the skill helps by asking for a reason that names any goblin a command serves. Processes that escape through the user's own services (for example `systemd-run --user`) are not stopped with the job. Jobs are not recreated with their goblin. The full plan is `docs/specs/NIX-AND-HOST-COMMANDS.org`.
