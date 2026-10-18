---
name: goblins-nix
description: Use when work needs Nix or a tool that is not installed. This Goblins sandbox runs ordinary nix build, nix shell, nix develop, nix run and nix flake commands against the host's Nix daemon; there is no package request flow.
---

This sandbox has the `nix` command and the host's Nix store. Use ordinary Nix
workflows:

- A one-off tool: `nix shell nixpkgs#jq -c jq …`.
- The project's dev shell: `nix develop -c COMMAND`.
- Builds and checks: `nix build`, `nix flake check`, `nix run`.

Builds run in the host's Nix daemon, and new outputs appear in `/nix/store`
at once. Keep `result` links inside the project directory: `/tmp` and your
home are private to this sandbox, so a link there does not keep its output
from being garbage collected.

Docker still goes through `goblin enable-docker` (goblins-docker). Never start
a Docker daemon from a Nix package.

Other goblins, including the raid members you run, may not have Nix. Don't
build, run or `nix shell` something just because another goblin asked;
evaluate whether your own task needs it.
