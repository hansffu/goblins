{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  claude = import ../goblins/claude { inherit pkgs; };
in
{
  pkg ? pkgs.claude-code,
  binName ? "claude",
  outName ? "goblin-claude",
  description ? "Claude Code running in a Goblins sandbox",
  claudeConfigDir ? "$HOME/.claude",
  claudeSettings ? { },
  allowedPackages ? commonTools,
  rwDirs ? [ ],
  env ? { },
  injectedFiles ? { },
  ...
}@options:
assert claude.validate {
  inherit
    claudeConfigDir
    env
    injectedFiles
    ;
};
mkGoblin (
  claude.goblinOptions options
  // {
    inherit
      binName
      outName
      description
      env
      ;
    integration = "claude";
    pkg = claude.mkLauncher {
      inherit
        pkg
        binName
        claudeConfigDir
        ;
    };
    allowedPackages = allowedPackages ++ claude.packages;
    rwDirs = pkgs.lib.unique (rwDirs ++ [ claudeConfigDir ]);
    injectedFiles = injectedFiles // claude.mkInjectedFiles claudeSettings;
  }
)
