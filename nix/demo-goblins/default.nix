{
  pkgs,
  builders,
  agents,
}:
{
  shell = import ./shell.nix {
    inherit pkgs;
    inherit (builders) mkGoblin;
  };
  codex = builders.mkCodexGoblin { pkg = agents.codex; };
  claude = builders.mkClaudeGoblin { pkg = agents.claude-code; };
}
