{
  pkgs,
  goblins,
  configurations,
}:
let
  inherit (pkgs) lib;
  helper = pkgs.rustPlatform.buildRustPackage {
    pname = "goblins-mount-helper";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ../../.;
      fileset = lib.fileset.unions [
        ../../Cargo.toml
        ../../Cargo.lock
        ../../src
      ];
    };
    cargoLock.lockFile = ../../Cargo.lock;
  };
  controller = pkgs.runCommand "goblins-controller" { } ''
    mkdir -p $out
    cp ${../../goblins.py} $out/goblins.py
    cp ${../../runtime.py} $out/runtime.py
    cp ${../../request.py} $out/request.py
  '';
  config = pkgs.writeText "goblins-config.json" (
    builtins.toJSON {
      goblins = lib.mapAttrs (
        _: configuration:
        configuration
        // {
          helper = "${helper}/bin/goblins-mount-helper";
          # Resolve approved attributes directly from our pinned nixpkgs source.
          flake = "path:${pkgs.path}";
        }
      ) configurations;
    }
  );
  package = pkgs.writeShellScriptBin "goblins" ''
    export PATH=${pkgs.nix}/bin:$PATH
    exec ${pkgs.python3}/bin/python3 ${controller}/goblins.py --runtime ${config} "$@"
  '';
in
package // { inherit config goblins helper; }
