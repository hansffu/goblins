{ pkgs, runtimeDirectory }:
{
  pkg,
  binName,
  codexConfigDir,
  filterUnavailableMcp,
}:
let
  inherit (pkgs) lib;
  directory = runtimeDirectory codexConfigDir;
in
pkgs.writeShellScriptBin binName ''
  export CODEX_HOME=${directory}
  codex_executable=${lib.escapeShellArg "${pkg}/bin/${binName}"}
  mcp_overrides=()
  ${lib.optionalString filterUnavailableMcp (
    lib.replaceStrings [ "\${coreutils}" "\${jq}" ] [ "${pkgs.coreutils}" "${pkgs.jq}" ] (
      builtins.readFile ./mcp.sh
    )
  )}
  # Register before native launch; the notifier never claims inbox items.
  registration=$(/run/goblins/bin/goblins integration register --driver codex)
  export GOBLINS_INTEGRATION_EPOCH=$(${pkgs.jq}/bin/jq -r .epoch <<< "$registration")
  /run/goblins/bin/goblins integration watch --epoch "$GOBLINS_INTEGRATION_EPOCH" </dev/null &
  # Goblins owns the outer sandbox and package approvals. Nested namespace
  # creation is prohibited by its seccomp policy.
  exec "$codex_executable" \
    --sandbox danger-full-access --ask-for-approval never --enable hooks "''${mcp_overrides[@]}" "$@"
''
