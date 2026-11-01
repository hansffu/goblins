{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-supervisor";
  source = ./.;
  libs = [
    ../runtime
    ../api
    ../jsonrpc
    ../init
    ../unix
  ];
}
