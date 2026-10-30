{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-controller";
  source = ./.;
  libs = [ ../../libs/api ];
  testPackages = [ "goblins-protocol" ];
}
