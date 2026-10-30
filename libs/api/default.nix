{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-protocol";
  source = ./.;
  libs = [ ../jsonrpc ];
}
