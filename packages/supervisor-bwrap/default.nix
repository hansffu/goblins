{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-supervisor-bwrap";
  source = ./.;
  libs = [
    ../../libs/supervisor
    ../../libs/runtime
    ../../libs/api
    ../../libs/jsonrpc
    ../../libs/init
    ../../libs/unix
  ];
}
