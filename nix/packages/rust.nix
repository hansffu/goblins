{ pkgs, package }:
pkgs.rustPlatform.buildRustPackage {
  pname = package;
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ../../.;
    fileset = pkgs.lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../src
      ../../crates
    ];
  };
  cargoLock.lockFile = ../../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    package
  ];
  cargoTestFlags = [
    "-p"
    package
  ]
  ++ pkgs.lib.optionals (package == "goblins-controller") [
    "-p"
    "goblins-protocol"
  ];
}
