{
  pkgs,
  runtimeDirectory,
  adapter,
}:
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
  # A fresh wakeup ledger for this launch; an earlier launch's monitor exits.
  ${adapter}/bin/goblins-claude start >/dev/null
  # Without bypassPermissions, Claude Code's own settings choose the
  # permission mode.
  exec "$claude_executable" --plugin-dir ${lib.escapeShellArg plugin} ${lib.optionalString bypassPermissions "--dangerously-skip-permissions "}"$@"
''
