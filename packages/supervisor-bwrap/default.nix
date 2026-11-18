{ pkgs, ghostty-vt }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-supervisor-bwrap";
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [ ghostty-vt ];
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
