{ pkgs, runtimeDirectory }:
{
  pkg,
  binName,
  claudeConfigDir,
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
  registration=$(/run/goblins/bin/goblins integration register --driver claude)
  export GOBLINS_INTEGRATION_EPOCH=$(${pkgs.jq}/bin/jq -r .epoch <<< "$registration")
  /run/goblins/bin/goblins integration watch --epoch "$GOBLINS_INTEGRATION_EPOCH" </dev/null &
  # Goblins owns the outer sandbox and permission boundary.
  exec "$claude_executable" --dangerously-skip-permissions "$@"
''
