# Internal Claude Code setup; the public constructor only composes these pieces.
{ pkgs }:
let
  inherit (pkgs) lib;
  hooks = import ./hooks.nix { inherit pkgs; };
  plugin = import ./plugin.nix { inherit pkgs; };
  skills = import ../skills.nix { inherit lib; };
  skillFile = name: "claude-code/.claude/skills/${name}/SKILL.md";
  skillFiles = lib.mapAttrs' (name: text: lib.nameValuePair (skillFile name) text);
  managedFiles = [ "claude-code/managed-settings.json" ] ++ map skillFile (skills { }).managed;
  runtimeDirectory =
    path:
    assert lib.assertMsg (builtins.isString path)
      "mkClaudeGoblin: claudeConfigDir must be a runtime path string";
    let
      prefixes = [
        "$HOME/"
        "\${HOME}/"
        "~/"
      ];
      prefix = lib.findFirst (candidate: lib.hasPrefix candidate path) null prefixes;
    in
    assert lib.assertMsg (
      path != "" && (lib.hasPrefix "/" path || prefix != null)
    ) "mkClaudeGoblin: claudeConfigDir must be absolute or start with ~/ or $HOME/";
    if prefix != null then
      ''"$HOME"/${lib.escapeShellArg (lib.removePrefix prefix path)}''
    else
      lib.escapeShellArg path;
in
{
  packages = [ hooks.package ];
  inherit plugin;
  mkLauncher = import ./launcher.nix { inherit pkgs runtimeDirectory; };
  mkInjectedFiles =
    { settings, nix }:
    let
      selected = skills { inherit nix; };
    in
    {
      "claude-code/managed-settings.json" =
        (pkgs.formats.json { }).generate "goblins-claude-settings.json"
          (
            settings
            // {
              hooks = hooks.mkHooks (settings.hooks or { });
              # The dev shell skill is mounted only in dev shell sandboxes.
              skillOverrides =
                (settings.skillOverrides or { })
                // lib.mapAttrs (_: _: "on") (selected.always // selected.devShell);
            }
          );
    }
    // skillFiles selected.always;
  devShellInjectedFiles = nix: skillFiles (skills { inherit nix; }).devShell;

  goblinOptions =
    options:
    builtins.removeAttrs options [
      "claudeConfigDir"
      "claudeSettings"
    ];
  validate =
    {
      claudeConfigDir,
      env,
      injectedFiles,
      devShellInjectedFiles,
    }:
    assert lib.assertMsg (
      !(env ? CLAUDE_CONFIG_DIR)
    ) "mkClaudeGoblin: use claudeConfigDir instead of env.CLAUDE_CONFIG_DIR";
    assert lib.assertMsg (
      lib.intersectLists managedFiles (
        builtins.attrNames injectedFiles ++ builtins.attrNames devShellInjectedFiles
      ) == [ ]
    ) "mkClaudeGoblin: injectedFiles cannot replace the Goblins Claude settings or skills";
    builtins.seq (runtimeDirectory claudeConfigDir) true;
}
