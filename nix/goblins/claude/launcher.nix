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
  # Without bypassPermissions, Claude Code's own settings choose the
  # permission mode.
  permissions=(${lib.optionalString bypassPermissions "--dangerously-skip-permissions"})
  # A Claude started from inside the goblin's Claude (claude --version, claude
  # -p) is plain Claude Code: it keeps the goblin's wakeup ledger, has no
  # plugin monitor, and its hooks change nothing.
  if [[ -n ''${GOBLINS_CLAUDE_SESSION:-} ]]; then
    export GOBLINS_CLAUDE_SESSION=nested
    exec "$claude_executable" "''${permissions[@]}" "$@"
  fi
  export GOBLINS_CLAUDE_SESSION=main
  # A fresh wakeup ledger for this launch; an earlier launch's monitor exits.
  ${adapter}/bin/goblins-claude start >/dev/null
  exec "$claude_executable" --plugin-dir ${lib.escapeShellArg plugin} "''${permissions[@]}" "$@"
''
