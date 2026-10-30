{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-unix";
  source = ./.;
  libs = [ ];
}
