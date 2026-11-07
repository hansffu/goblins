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
  # A goblin with a worktree folder works in its worktrees by path: make the
  # folder one of Claude Code's working directories, so it reads and edits
  # there without asking. It goes before the other options because
  # --add-dir takes several values and would swallow a prompt.
  worktrees=()
  if [ -n "''${GOBLINS_WORKTREES:-}" ]; then worktrees=(--add-dir "$GOBLINS_WORKTREES"); fi
  # Without bypassPermissions, Claude Code's own settings choose the
  # permission mode.
  exec "$claude_executable" "''${worktrees[@]}" --plugin-dir ${lib.escapeShellArg plugin} ${lib.optionalString bypassPermissions "--dangerously-skip-permissions "}"$@"
''
