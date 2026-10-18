{
  pkgs,
  builders,
  commonTools,
  agents,
  # More scopes the agents may join as children, such as the dev shell's.
  agentScopes ? [ ],
}:
let
  # Fish too, for a shell inside the agent goblins (goblins-shell-program).
  tools = commonTools ++ [ pkgs.fish ];
  # Every demo goblin may start the agents as helpers; none may start
  # goblins-dev, the only one with Nix and host commands.
  allowedRunTargets = [
    "claude"
    "codex"
  ];
in
{
  shell = import ./shell.nix {
    inherit pkgs;
    inherit (builders) mkGoblin;
  };
  # Unscoped when started directly; as goblins-dev's children they join its
  # scope.
  codex = builders.mkCodexGoblin {
    pkg = agents.codex;
    allowedPackages = tools;
    allowedScopes = [ "goblins-dev" ] ++ agentScopes;
    inherit allowedRunTargets;
  };
  claude = builders.mkClaudeGoblin {
    pkg = agents.claude-code;
    allowedPackages = tools;
    allowedScopes = [ "goblins-dev" ] ++ agentScopes;
    inherit allowedRunTargets;
  };
  # For developing Goblins from a goblin: ordinary Nix commands against the
  # host daemon, and approved host commands for the Bubblewrap, namespace and
  # Docker tests. The self-hosting setup joins it as it is implemented
  # (docs/specs/NIX-AND-HOST-COMMANDS.org).
  goblins-dev = builders.mkClaudeGoblin {
    pkg = agents.claude-code;
    description = "Claude Code with the host's Nix, for developing Goblins";
    allowedPackages = tools;
    allowNix = true;
    hostCommands.enable = true;
    # Cargo and Nix fetcher caches that outlive the session (scopes.nix).
    scope = "goblins-dev";
    inherit allowedRunTargets;
  };
}
