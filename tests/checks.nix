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
    { roDirs = "/etc"; }
    { rwFiles = [ 1 ]; }
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
          ../crates
        ];
      };
      api = import "${source}/nix/lib.nix" { inherit pkgs sandbox; };
    in
    api.mkGoblins {
      goblins.shell = api.goblins.shell;
    };
  runtime-no-python =
    let
      minimal = mkGoblins { goblins.shell = mkGoblin (base // { allowedPackages = [ ]; }); };
      closure = pkgs.closureInfo { rootPaths = [ minimal ]; };
    in
    pkgs.runCommand "goblins-runtime-no-python" { nativeBuildInputs = [ pkgs.gnugrep ]; } ''
      if grep -E '/[^/]*-python[0-9.]*-' ${closure}/store-paths; then
        echo 'Unexpected mandatory Python runtime' >&2
        exit 1
      fi
      touch $out
    '';
  named-goblins = configured;
  updated-goblins =
    let
      changed = mkGoblin (
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
            GOBLIN_MARKER = "updated";
            PS1 = "updated> ";
          };
        }
      );
    in
    mkGoblins {
      goblins = {
        fishy = changed;
        added = changed;
      };
    };
  configured-shell = mkGoblins {
    goblins.shell = mkGoblin {
      pkg = pkgs.fish;
      binName = "fish";
      args = [ "--interactive" ];
      roDirs = [ "$HOME/.config/fish" ];
    };
  };
  bind-targets = pkgs.runCommand "goblins-bind-targets" { } ''
    mkdir -p $out/functions
    echo 'set -gx GOBLIN_FISH_CONFIG loaded' > $out/config.fish
    echo 'function config_probe; echo config-function-loaded; end' > $out/functions/config_probe.fish
    echo private-sibling > $out/unselected-secret
  '';
  bound-goblins = mkGoblins {
    goblins.bound = mkGoblin (
      base
      // {
        args = [
          "--noprofile"
          "--norc"
          "-i"
        ];
        allowedPackages = [ pkgs.coreutils ];
        env.PS1 = "bind-test> ";
        roDirs = [ "$GOBLINS_TEST_ROOT/readonly" ];
        rwDirs = [ "\${GOBLINS_TEST_ROOT}/writable" ];
        roFiles = [ "$GOBLINS_TEST_ROOT/ro-file" ];
        rwFiles = [ "$GOBLINS_TEST_ROOT/rw-file" ];
      }
    );
  };
  goblins-api =
    assert builtins.all rejected invalid;
    pkgs.runCommand "goblins-api-evaluation" { } "touch $out";
}
