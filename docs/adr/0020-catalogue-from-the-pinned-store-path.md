# ADR 0020: Evaluate the pinned catalogue from its store path

Date: 2026-11-13

Status: Accepted. Task 6 of [TEST-SUITE-SPEEDUP.org](../specs/TEST-SUITE-SPEEDUP.org).

## Context

A package request names an attribute of the catalogue, which the runtime manifest gives as the flake reference `flake`, always `path:${pkgs.path}` (the pinned nixpkgs source). The supervisor evaluates `<flake>#legacyPackages.x86_64-linux.<name>` for the preview, again when the request is approved, and builds it. Nix 2.34 hashes the whole 469 MB tree of a `path:` flake on every command before it evaluates anything, about 2.7 s, and `?narHash=` does not avoid it. A user waits about 3 s for a preview and 6 s for a grant, and grants dominate the integration tests.

## Decision

- The runtime manifest gets an optional entry `nixpkgs`, the store path of the nixpkgs source tree that `flake` serves. `packages/daemon/wrapper.nix` and the test fixtures set it beside `flake`. Manifests without it stay valid.
- When `nixpkgs` is present and names an entry directly under `/nix/store`, the supervisor imports it instead of evaluating the flake: `nix eval --impure --option restrict-eval true -I nixpkgs=<path> --expr '(import <nixpkgs> { system = "x86_64-linux"; config = {}; overlays = []; }).<name>'`, and builds `<name>^<output>` of the same expression. This took 0.29 s instead of 2.9 s and selected the same output paths.
- Only the explicit entry selects this form. The location of `flake` is never inspected: a store-backed flake can offer `legacyPackages` without being an importable nixpkgs, and without the entry the flake form is used exactly as before.
- The package name is validated before any command is built, as before. A missing attribute is reported with the flake form's wording.

## Isolation

Flake evaluation is pure. The direct import needs `--impure` for `<nixpkgs>`, so isolation comes from three things together instead:

- `restrict-eval` hides every path outside `-I` and makes `builtins.getEnv` return the empty string, so `NIXPKGS_ALLOW_UNFREE` and `$HOME` do not reach nixpkgs. Without it, plain `--impure` would let `NIXPKGS_ALLOW_UNFREE=1` allow unfree packages.
- `config = {}` keeps `~/.config/nixpkgs/config.nix` out.
- `overlays = []` keeps user overlays and `<nixpkgs-overlays>` out.

`test_recreate` checks that unfree packages stay refused when the daemon's environment allows them, and that a store-backed flake that is not nixpkgs is still served through the flake form.

## Alternatives considered

- Selecting the direct import whenever `flake` is a `path:/nix/store/...` reference. Rejected: the manifest accepts any flake, and such a flake need not be nixpkgs.
- Adding `?narHash=` to the `path:` reference. Measured: Nix still hashes the tree.

## Future catalogue extensions

The user plans to let goblins request packages beyond the pinned nixpkgs. This decision covers only the plain pinned nixpkgs, and is meant to sit beside those extensions:

- A few extra named packages (packages built in the user's own configuration, or from flakes such as llm-agents) can be resolved when `mkGoblin` builds the configuration: the manifest would map each name to its derivation path and the metadata a preview shows, and a grant would build that derivation without evaluating anything. That is independent of this decision and keeps the pinned nixpkgs on the fast path.
- A whole nixpkgs with overlays (for example the Emacs overlay) must be evaluated at runtime. It works today through `flake`, pointing at a flake whose `legacyPackages` is the overlaid set, at the flake form's cost. A faster form would extend the `nixpkgs` entry to a store expression that applies the overlays; whether restricted evaluation lets such an expression read the overlay sources, and how overlays that exist only as flake outputs fit, needs a spike first.

## Consequences

Package previews and grants from the pinned catalogue no longer hash nixpkgs. There are two evaluation forms to keep correct, with the flake form still covered by its own test. Nix's error text for a missing attribute differs between them and is mapped back to the flake wording. The nixpkgs flake adds version metadata to `lib` that a plain import lacks; no tested output path changed.
