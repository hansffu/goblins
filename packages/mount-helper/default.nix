{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-mount-helper";
  source = ./.;
  libs = [ ];
}
