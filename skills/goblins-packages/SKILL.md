---
name: goblins-packages
description: Request a missing command or development tool inside a Goblins sandbox through its live Nix package approval flow. Use when work requires software that is not currently available.
---

Check whether the required command is already available. If it is missing,
use `goblins request-package PACKAGE --reason "REASON"` to request a package
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

Request only tools needed for the current task, through Goblins rather than a
host package manager. This does not replace the project's normal dependency
workflow inside its existing sandbox permissions. After a successful grant,
retry the command that needed the package. On denial or failure, report the
result and use an already available alternative if it satisfies the request.
