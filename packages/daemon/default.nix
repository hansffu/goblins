{ pkgs }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-controller";
  source = ./.;
  # Actor tests execute the separate production binary; its implementation
  # libraries are absent from this package's source and Cargo graph.
  checkEnv.GOBLINS_TEST_SUPERVISOR_EXECUTABLE =
    "${import ../supervisor-bwrap { inherit pkgs; }}/bin/goblins-supervisor-bwrap";
  libs = [
    ../../libs/unix
    ../../libs/runtime
    ../../libs/api
    ../../libs/jsonrpc
  ];
  testPackages = [
    "goblins-unix"
    "goblins-runtime"
    "goblins-protocol"
    "goblins-jsonrpc"
  ];
}
