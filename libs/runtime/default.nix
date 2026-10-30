{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-runtime";
  source = ./.;
  libs = [
    ../api
    ../jsonrpc
  ];
}
