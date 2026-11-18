{ pkgs, sandbox, ghostty-vt }:
{
  builders = import ./builders { inherit pkgs sandbox ghostty-vt; };
  inherit (sandbox) commonTools;
}
