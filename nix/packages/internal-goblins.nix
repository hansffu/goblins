{ pkgs }:
let
  client = import ./rust.nix {
    inherit pkgs;
    package = "goblins-client";
  };
in
pkgs.runCommand "goblins-request-client" { } ''
  mkdir -p $out/bin
  ln -s ${client}/bin/goblins-client $out/bin/goblins
  ln -s ${client}/bin/goblins-client $out/bin/goblins-request
''
