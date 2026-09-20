{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  codex = import ./codex { inherit pkgs; };
in
{
  pkg ? pkgs.codex,
  binName ? "codex",
  outName ? "goblin-codex",
  codexConfigDir ? "$HOME/.codex",
  codexSettings ? { },
  filterUnavailableMcp ? false,
  allowedPackages ? commonTools,
  rwDirs ? [ ],
  env ? { },
  sandboxEtc ? { },
  ...
}@options:
assert codex.validate {
  inherit
    pkg
    codexConfigDir
    env
    sandboxEtc
    ;
};
mkGoblin (
  codex.goblinOptions options
  // {
    inherit binName outName env;
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
    sandboxEtc = sandboxEtc // codex.mkEtc codexSettings;
  }
)
