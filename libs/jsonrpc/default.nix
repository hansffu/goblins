{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-jsonrpc";
  source = ./.;
  libs = [ ];
}
