{
  pkgs,
  package,
  source,
  libs,
  testPackages ? [ ],
  checkEnv ? { },
  nativeBuildInputs ? [ ],
  buildInputs ? [ ],
}:
let
  root = ../../.;
  members = (builtins.fromTOML (builtins.readFile ../../Cargo.toml)).workspace.members;
in
pkgs.rustPlatform.buildRustPackage {
  inherit nativeBuildInputs buildInputs;
  pname = package;
  version = "0.1.0";
  # Cargo needs every workspace manifest, but only the selected package and
  # its explicit library dependencies need sources. Keep unrelated edits from
  # rebuilding every Rust package.
  src = pkgs.lib.fileset.toSource {
    inherit root;
    fileset = pkgs.lib.fileset.unions (
      [
        ../../Cargo.toml
        ../../Cargo.lock
        source
      ]
      ++ map (member: root + "/${member}/Cargo.toml") members
      ++ libs
    );
  };
  # Cargo discovers targets in every member, even with -p. Inert targets for
  # excluded members satisfy discovery; the selected package never uses them.
  postPatch = pkgs.lib.concatMapStringsSep "\n" (member: ''
    if [ ! -d ${member}/src ]; then
      mkdir -p ${member}/src
      ${
        if pkgs.lib.hasPrefix "libs/" member then
          "touch ${member}/src/lib.rs"
        else
          "echo 'fn main() {}' > ${member}/src/main.rs"
      }
    fi
  '') members;
  # Executable fixtures are supplied only to tests, never compiled as library
  # dependencies or embedded into production output through env! constants.
  preCheck = pkgs.lib.concatStringsSep "\n" (
    pkgs.lib.mapAttrsToList (name: value: "export ${name}=${pkgs.lib.escapeShellArg (toString value)}") checkEnv
  );
  postCheck = pkgs.lib.optionalString (checkEnv != { }) (
    "unset " + pkgs.lib.concatStringsSep " " (builtins.attrNames checkEnv)
  );
  cargoLock.lockFile = ../../Cargo.lock;
  # Cargo vendors the workspace lockfile even for packages that do not link Ghostty.
  cargoLock.outputHashes."libghostty-vt-0.2.1" =
    "sha256-e5YkGJxvKF6kNaFlRC9M1VCftcKrk3YgnirkW/EXlCQ=";
  cargoBuildFlags = [
    "-p"
    package
  ];
  cargoTestFlags = [
    "-p"
    package
  ]
  ++ pkgs.lib.concatMap (name: [
    "-p"
    name
  ]) testPackages;
}
