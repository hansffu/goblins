{
  pkgs,
  sandbox,
  # Agent packages for the default goblins: claude-code and codex. The
  # flake passes numtide/llm-agents.nix.
  agents ? pkgs,
}:
let
  builders = import ./builders { inherit pkgs sandbox; };
in
{
  inherit builders;
  inherit (sandbox) commonTools;
  defaultGoblins = import ./default-goblins { inherit pkgs builders agents; };
}
