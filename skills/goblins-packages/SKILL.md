---
name: goblins-packages
description: Request a missing command or development tool inside a Goblins sandbox through its live Nix package approval flow. Use when work requires software that is not currently available.
---

Use `goblins request-package PACKAGE --reason "REASON"` to request a package
from the host-controlled pinned nixpkgs set. Give the nixpkgs attribute name,
such as `yq` or `python3Packages.black`, not a path, URL, Nix expression or
output selector. State briefly which user task needs it.

The command waits for the human decision and package provisioning. Exit 0 means
the complete runtime closure is mounted read-only and newly launched commands
can use it without restarting the sandbox. Exit 1 means denial or a known
lookup, build or grant failure. Exit 2 means the outcome is unknown; do not
automatically submit another request, because the original may still complete.

Request only software needed for the current task. Do not substitute `nix
profile`, `nix-shell`, `apt`, or another installer: package approval and
provisioning belong to Goblins. After a successful grant, retry the command that
needed the package. If the request is denied or fails, report the result and use
an already available alternative when one satisfies the user's request.
