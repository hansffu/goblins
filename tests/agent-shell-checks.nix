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
  claudeHarness = ./claude_shell.py;
  claudeMock = pkgs.symlinkJoin {
    name = "goblins-claude-shell-endpoints";
    paths = map (name: pkgs.writeScriptBin name ''
      #!${pkgs.python3}/bin/python3
      import runpy
      runpy.run_path("${claudeHarness}", run_name="claude_mock", init_globals={"ENDPOINT": "${name}"})
    '') [ "claude" "goblins-claude" "goblin" ];
  };
  claude = import ../nix/goblins/claude/launcher.nix {
    inherit pkgs;
    runtimeDirectory = _: ''"$HOME/.claude"'';
    adapter = claudeMock;
  };
  claudeLauncher = bypassPermissions: claude {
    pkg = claudeMock;
    binName = "claude";
    claudeConfigDir = "$HOME/.claude";
    plugin = "/fixture plugin";
    inherit bypassPermissions;
  };
  # Replace only ambient endpoints; execute the production shell logic intact.
  claudeHook = pkgs.writeShellScript "goblins-claude-hook" (
    pkgs.lib.replaceStrings
      [ "runtime=/tmp/goblins-claude" "/run/goblins/bin/goblin" ]
      [ ''runtime="$HOOK_RUNTIME"'' "${claudeMock}/bin/goblin" ]
      (builtins.readFile ../nix/goblins/claude/hook.sh)
  );
in
{
  codex-shell = pkgs.runCommand "goblins-codex-shell" {
    nativeBuildInputs = [ pkgs.python3 pkgs.bash pkgs.coreutils ];
  } ''
    python3 ${harness} ${launcher false} ${launcher true} ${mcp} ${mock "codex"}
    touch $out
  '';
  claude-shell = pkgs.runCommand "goblins-claude-shell" {
    nativeBuildInputs = [ pkgs.python3 pkgs.bash pkgs.coreutils pkgs.jq pkgs.util-linux pkgs.gnugrep ];
  } ''
    python3 ${claudeHarness} ${claudeHook} ${claudeMock} ${claudeLauncher false} ${claudeLauncher true}
    touch $out
  '';

}
