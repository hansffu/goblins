# Internal Claude Code setup; the public constructor only composes these pieces.
{ pkgs }:
let
  inherit (pkgs) lib;
  hooks = import ./hooks.nix { inherit pkgs; };
  plugin = import ./plugin.nix { inherit pkgs; };
  managedFiles = [
    "claude-code/managed-settings.json"
    "claude-code/.claude/skills/goblins-spawn/SKILL.md"
    "claude-code/.claude/skills/goblins-messaging/SKILL.md"
    "claude-code/.claude/skills/goblins-packages/SKILL.md"
    "claude-code/.claude/skills/goblins-docker/SKILL.md"
    "claude-code/.claude/skills/goblins-devshell/SKILL.md"
  ];
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
  mkInjectedFiles = settings: {
    "claude-code/managed-settings.json" =
      (pkgs.formats.json { }).generate "goblins-claude-settings.json"
        (
          settings
          // {
            hooks = hooks.mkHooks (settings.hooks or { });
            skillOverrides = (settings.skillOverrides or { }) // {
              goblins-spawn = "on";
              goblins-messaging = "on";
              goblins-packages = "on";
              goblins-docker = "on";
              # Mounted only in dev shell sandboxes.
              goblins-devshell = "on";
            };
          }
        );
    "claude-code/.claude/skills/goblins-spawn/SKILL.md" =
      builtins.readFile ../../../skills/goblins-spawn/SKILL.md;
    "claude-code/.claude/skills/goblins-messaging/SKILL.md" =
      builtins.readFile ../../../skills/goblins-messaging/SKILL.md;
    "claude-code/.claude/skills/goblins-packages/SKILL.md" =
      builtins.readFile ../../../skills/goblins-packages/SKILL.md;
    "claude-code/.claude/skills/goblins-docker/SKILL.md" =
      builtins.readFile ../../../skills/goblins-docker/SKILL.md;
  };
  devShellInjectedFiles = {
    "claude-code/.claude/skills/goblins-devshell/SKILL.md" =
      builtins.readFile ../../../skills/goblins-devshell/SKILL.md;
  };

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
