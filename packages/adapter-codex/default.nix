{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-codex";
  source = ./.;
  libs = [
    ../../libs/api
    ../../libs/jsonrpc
  ];
}
