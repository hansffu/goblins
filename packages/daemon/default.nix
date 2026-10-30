{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-controller";
  source = ./.;
  libs = [
    ../../libs/api
    ../../libs/jsonrpc
    ../../libs/init
  ];
  testPackages = [
    "goblins-protocol"
    "goblins-jsonrpc"
    "goblins-init"
  ];
}
