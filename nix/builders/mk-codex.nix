{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  codex = import ../goblins/codex { inherit pkgs; };
in
{
  pkg ? pkgs.codex,
  binName ? "codex",
  outName ? "goblin-codex",
  description ? "Codex running in a Goblins sandbox",
  codexConfigDir ? "$HOME/.codex",
  codexSettings ? { },
  filterUnavailableMcp ? false,
  allowedPackages ? commonTools,
  rwDirs ? [ ],
  env ? { },
  injectedFiles ? { },
  ...
}@options:
assert codex.validate {
  inherit
    pkg
    codexConfigDir
    env
    injectedFiles
    ;
};
mkGoblin (
  codex.goblinOptions options
  // {
    inherit
      binName
      outName
      description
      env
      ;
    integration = "codex";
    pkg = codex.mkLauncher {
      inherit
        pkg
        binName
        codexConfigDir
        filterUnavailableMcp
        ;
    };
    allowedPackages = allowedPackages ++ codex.packages;
    rwDirs = pkgs.lib.unique (rwDirs ++ [ codexConfigDir ]);
    injectedFiles = injectedFiles // codex.mkInjectedFiles codexSettings;
  }
)
