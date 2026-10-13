{ pkgs, sandbox }:
{
  builders = import ./builders { inherit pkgs sandbox; };
  inherit (sandbox) commonTools;
}
