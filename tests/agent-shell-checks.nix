# Behavioral checks of the generated shell launchers, without real agents.
{ pkgs }:
let
  harness = ./codex_shell.py;
  mock = name: pkgs.writeScriptBin (if name == "adapter" then "goblins-codex" else "codex") ''
    #!${pkgs.python3}/bin/python3
    import runpy
    runpy.run_path("${harness}", run_name="${name}_mock")
  '';
  codex = import ../nix/goblins/codex/launcher.nix {
    inherit pkgs;
    runtimeDirectory = _: ''"$HOME/.codex"'';
    adapter = mock "adapter";
  };
  launcher = bypassPermissions: codex {
    pkg = mock "codex";
    binName = "codex";
    codexConfigDir = "$HOME/.codex";
    filterUnavailableMcp = false;
    inherit bypassPermissions;
  };
  mcp = pkgs.writeText "goblins-mcp-filter.sh" (
    pkgs.lib.replaceStrings [ "\${coreutils}" "\${jq}" ] [ "${pkgs.coreutils}" "${pkgs.jq}" ]
      (builtins.readFile ../nix/goblins/codex/mcp.sh)
  );
in
{
  codex-shell = pkgs.runCommand "goblins-codex-shell" {
    nativeBuildInputs = [ pkgs.python3 pkgs.bash pkgs.coreutils ];
  } ''
    python3 ${harness} ${launcher false} ${launcher true} ${mcp} ${mock "codex"}
    touch $out
  '';
}
