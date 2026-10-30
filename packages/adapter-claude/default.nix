{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-claude";
  source = ./.;
  libs = [ ../../libs/api ];
}
