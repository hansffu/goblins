---
name: goblins-packages
description: Request a missing command or development tool inside a Goblins sandbox through its live Nix package approval flow. Use when work requires software that is not currently available. Not for Docker, containers or Testcontainers; use goblins-docker instead.
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

There is no `nix` command in the sandbox; do not try `nix develop`, `nix run`
or `nix shell`. When `GOBLINS_DEV_SHELL` is set, the host started this sandbox
in that project flake's dev shell, like `nix develop`: its tools, variables and
`shellHook` exports are already in your environment. Run
`source /run/goblins/devshell/env.sh` in Bash if you also need the dev shell's
functions. Editing `flake.nix` or
`flake.lock` does not change the running environment; report that a new dev
shell needs the host to relaunch the sandbox. Do not edit `flake.lock` by hand.

Docker is not a package: to use containers, follow goblins-docker and run
`goblins enable-docker` instead of requesting `docker`.

Request only tools needed for the current task, through Goblins rather than a
host package manager. This does not replace the project's normal dependency
workflow inside its existing sandbox permissions. After a successful grant,
retry the command that needed the package. On denial or failure, report the
result and use an already available alternative if it satisfies the request.
