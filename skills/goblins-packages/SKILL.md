---
name: goblins-packages
description: Request a missing command or development tool inside a Goblins sandbox through its live Nix package approval flow. Use when work requires software that is not currently available. Not for Docker, containers or Testcontainers; use goblins-docker instead.
---

The sandbox client is `/run/goblins/bin/goblins`; use that absolute path if a
dev shell shadows `goblins` with a project or host CLI.

Check whether the required command is already available. If it is missing
and belongs in the project's development dependencies, check `goblins status`:
when a dev shell is active, follow
[goblins-devshell](../goblins-devshell/SKILL.md) to update that environment.
For a one-off tool, or when no dev shell is active, use
`goblins request-package PACKAGE --reason "REASON"` to request a package
from the host-controlled pinned nixpkgs set. Give the nixpkgs attribute name,
such as `jq` or `python3Packages.black`, not a path, URL, Nix expression or
output selector. State briefly which user task needs it.

The command waits for the human decision and package provisioning. Exit 0 means
the complete runtime closure is mounted read-only and newly launched commands
can use it without restarting the sandbox. Development setup hooks are not
activated; a runtime grant is not a Nix development shell.
Exit 1 means denial or a known lookup, build or grant failure.
Exit 2 means the outcome is unknown; do not
automatically submit another request, because the original may still complete.
If you end your turn while a request is pending, its outcome arrives as an
operation notice in your inbox (see goblins-messaging).

Nix commands are not supported in the sandbox, even if a dev shell provides
a `nix` binary; use Goblins instead of `nix develop`, `nix run` or `nix shell`.

Docker is not a package: to use containers, follow goblins-docker and run
`goblins enable-docker` instead of requesting `docker`.

Request only tools needed for the current task, through Goblins rather than a
host package manager. This does not replace the project's normal dependency
workflow inside its existing sandbox permissions. After a successful grant,
retry the command that needed the package. On denial or failure, report the
result and use an already available alternative if it satisfies the request.
