{
  pkgs,
  goblinsLib,
  sandbox,
}:
let
  inherit (goblinsLib) mkGoblin mkGoblins;
  base = {
    pkg = pkgs.bashInteractive;
    binName = "bash";
  };
  configured = mkGoblins {
    goblins = {
      fishy = mkGoblin {
        pkg = pkgs.fish;
        binName = "fish";
        args = [
          "--no-config"
          "--interactive"
        ];
        allowedPackages = [ pkgs.coreutils ];
        env.GOBLIN_MARKER = "literal $HOME $(false)";
      };
      utility = mkGoblin (
        base
        // {
          args = [
            "--noprofile"
            "--norc"
            "-i"
          ];
          allowedPackages = [
            pkgs.coreutils
            pkgs.tree
          ];
          env = {
            GOBLIN_MARKER = "utility";
            PS1 = "utility> ";
          };
        }
      );
    };
  };
  rejected = configuration: !(builtins.tryEval (mkGoblins configuration).config.drvPath).success;
  invalid = [
    { goblins = { }; }
    { goblins.bad = pkgs.hello; }
    { goblins."../bad" = mkGoblin base; }
  ]
  ++ map (options: { goblins.bad = mkGoblin (base // options); }) [
    { binName = "../bash"; }
    { allowNix = true; }
    { allowUnixSockets = false; }
    { allowedDomains = null; }
    { allowedDomains = [ "example.com" ]; }
    { allowedHostPorts = [ 80 ]; }
    { publishedPorts = [ 80 ]; }
    { roDirs = [ "/etc" ]; }
    { rwFiles = [ "/tmp/file" ]; }
    { args = "-i"; }
    { env.PATH = "/host/bin"; }
    { env.BAD = 1; }
  ];
in
{
  # Build without this repository's flake, lock or tests in the source tree.
  production-only =
    let
      source = pkgs.lib.fileset.toSource {
        root = ../.;
        fileset = pkgs.lib.fileset.unions [
          ../nix
          ../Cargo.toml
          ../Cargo.lock
          ../src
          ../goblins.py
          ../runtime.py
          ../request.py
        ];
      };
      api = import "${source}/nix/lib.nix" { inherit pkgs sandbox; };
    in
    api.mkGoblins {
      goblins.shell = api.goblins.shell;
    };
  named-goblins = configured;
  goblins-api =
    assert builtins.all rejected invalid;
    pkgs.runCommand "goblins-api-evaluation" { } "touch $out";
}
