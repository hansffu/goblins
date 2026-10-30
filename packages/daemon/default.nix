{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-controller";
  source = ./.;
  libs = [
    ../../libs/runtime
    ../../libs/api
    ../../libs/jsonrpc
    ../../libs/init
  ];
  testPackages = [
    "goblins-runtime"
    "goblins-protocol"
    "goblins-jsonrpc"
    "goblins-init"
  ];
}
