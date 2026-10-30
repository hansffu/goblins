{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-mount-helper";
  source = ./.;
  libs = [ ../../libs/unix ../../libs/runtime ../../libs/api ../../libs/jsonrpc ];
}
