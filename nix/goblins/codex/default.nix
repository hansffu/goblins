# Internal Codex setup; the public constructor only composes these pieces.
{ pkgs }:
let
  inherit (pkgs) lib;
  hooks = import ./hooks.nix { inherit pkgs; };
  managedFiles = [
    "codex/config.toml"
    "codex/skills/goblins/SKILL.md"
  ];
  runtimeDirectory =
    path:
    assert lib.assertMsg (builtins.isString path)
      "mkCodexGoblin: codexConfigDir must be a runtime path string";
    let
      # Credentials stay outside the store. Expand HOME/tilde only at launch.
      prefixes = [
        "$HOME/"
        "\${HOME}/"
        "~/"
      ];
      prefix = lib.findFirst (prefix: lib.hasPrefix prefix path) null prefixes;
    in
    assert lib.assertMsg (
      path != "" && (lib.hasPrefix "/" path || prefix != null)
    ) "mkCodexGoblin: codexConfigDir must be absolute or start with ~/ or $HOME/";
    if prefix != null then
      ''"$HOME"/${lib.escapeShellArg (lib.removePrefix prefix path)}''
    else
      lib.escapeShellArg path;
in
{
  packages = [
    pkgs.fish
    hooks.package
  ];
  mkLauncher = import ./launcher.nix { inherit pkgs runtimeDirectory; };
  mkEtc = settings: {
    "codex/config.toml" = (pkgs.formats.toml { }).generate "goblins-codex.toml" (
      settings // { hooks = hooks.mkHooks (settings.hooks or { }); }
    );
    "codex/skills/goblins/SKILL.md" = builtins.readFile ./goblins/SKILL.md;
  };

  # Preserve ordinary mkGoblin options, including ones added in the future.
  goblinOptions =
    options:
    builtins.removeAttrs options [
      "codexConfigDir"
      "codexSettings"
      "filterUnavailableMcp"
    ];
  validate =
    {
      pkg,
      codexConfigDir,
      env,
      sandboxEtc,
    }:
    assert lib.assertMsg (
      !(pkg ? version) || lib.versionAtLeast pkg.version "0.146.0"
    ) "mkCodexGoblin: Codex 0.146.0 or later is required for the managed hook setup";
    assert lib.assertMsg (
      !(env ? CODEX_HOME)
    ) "mkCodexGoblin: use codexConfigDir instead of env.CODEX_HOME";
    assert lib.assertMsg (
      lib.intersectLists managedFiles (builtins.attrNames sandboxEtc) == [ ]
    ) "mkCodexGoblin: sandboxEtc cannot replace the Goblins Codex config or skill";
    builtins.seq (runtimeDirectory codexConfigDir) true;
}
