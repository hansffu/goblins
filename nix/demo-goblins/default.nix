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
  # For developing Goblins from a goblin: ordinary Nix commands against the
  # host daemon. Host commands and the self-hosting setup join it as they are
  # implemented (docs/specs/NIX-AND-HOST-COMMANDS.org).
  goblins-dev = builders.mkClaudeGoblin {
    pkg = agents.claude-code;
    description = "Claude Code with the host's Nix, for developing Goblins";
    allowedPackages = tools;
    allowNix = true;
    allowedChildren = [
      "goblins-dev"
      "claude"
      "codex"
    ];
  };
}
