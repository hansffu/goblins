{
  pkgs,
  builders,
  commonTools,
  agents,
}:
let
  # Fish too, for a shell inside the agent goblins (goblins-shell-program).
  tools = commonTools ++ [ pkgs.fish ];
in
{
  shell = import ./shell.nix {
    inherit pkgs;
    inherit (builders) mkGoblin;
  };
  codex = builders.mkCodexGoblin {
    pkg = agents.codex;
    allowedPackages = tools;
  };
  claude = builders.mkClaudeGoblin {
    pkg = agents.claude-code;
    allowedPackages = tools;
  };
}
