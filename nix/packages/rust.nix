{ pkgs, package }:
pkgs.rustPlatform.buildRustPackage {
  pname = package;
  version = "0.1.0";
  # Cargo needs every workspace manifest, but each binary only depends on its
  # own sources and (except the mount helper) the protocol crate. Keeping the
  # other sources out prevents unrelated edits rebuilding every Rust package.
  src = pkgs.lib.fileset.toSource {
    root = ../../.;
    fileset = pkgs.lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../crates/protocol/Cargo.toml
      ../../crates/client/Cargo.toml
      ../../crates/controller/Cargo.toml
      ../../crates/codex/Cargo.toml
      ../../crates/claude/Cargo.toml
      (
        if package == "goblins-mount-helper" then
          ../../src
        else
          ../../crates/${pkgs.lib.removePrefix "goblins-" package}
      )
      (if package == "goblins-mount-helper" then pkgs.lib.fileset.empty else ../../crates/protocol)
    ];
  };
  # Cargo discovers targets in every workspace member, even with -p. Supply
  # inert targets only for the excluded members; they are never compiled.
  postPatch = ''
    for dir in . crates/client crates/controller crates/codex crates/claude; do
      if [ ! -d "$dir/src" ]; then
        mkdir -p "$dir/src"
        echo 'fn main() {}' > "$dir/src/main.rs"
      fi
    done
    if [ ! -d crates/protocol/src ]; then
      mkdir -p crates/protocol/src
      touch crates/protocol/src/lib.rs
    fi
  '';
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
