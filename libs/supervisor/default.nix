{ pkgs, ghostty-vt }:
import ../../nix/builders/rust.nix {
  inherit pkgs;
  package = "goblins-supervisor";
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [ ghostty-vt ];
  source = ./.;
  libs = [
    ../runtime
    ../api
    ../jsonrpc
    ../init
    ../unix
  ];
}
