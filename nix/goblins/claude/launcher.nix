{ pkgs, runtimeDirectory }:
{
  pkg,
  binName,
  bypassPermissions,
  claudeConfigDir,
  plugin,
}:
let
  inherit (pkgs) lib;
  directory = runtimeDirectory claudeConfigDir;
in
pkgs.writeShellScriptBin binName ''
  export CLAUDE_CONFIG_DIR=${directory}
  export DISABLE_AUTOUPDATER=1
  claude_executable=${lib.escapeShellArg "${pkg}/bin/${binName}"}
  # Register before native launch; the notifier never claims inbox items.
  registration=$(/run/goblins/bin/goblin integration register --driver claude)
  export GOBLINS_INTEGRATION_EPOCH=$(${pkgs.jq}/bin/jq -r .epoch <<< "$registration")
  # Without bypassPermissions, Claude Code's own settings choose the
  # permission mode.
  exec "$claude_executable" --plugin-dir ${lib.escapeShellArg plugin} ${lib.optionalString bypassPermissions "--dangerously-skip-permissions "}"$@"
''
