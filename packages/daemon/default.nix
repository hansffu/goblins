{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-controller";
  source = ./.;
  libs = [
    ../../libs/supervisor
    ../../libs/unix
    ../../libs/runtime
    ../../libs/api
    ../../libs/jsonrpc
    ../../libs/init
  ];
  testPackages = [
    "goblins-supervisor"
    "goblins-unix"
    "goblins-runtime"
    "goblins-protocol"
    "goblins-jsonrpc"
    "goblins-init"
  ];
}
