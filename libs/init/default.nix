{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-init";
  source = ./.;
  libs = [
    ../api
    ../jsonrpc
  ];
}
