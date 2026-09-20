{ pkgs, sandbox }:
let
  builders = import ./builders { inherit pkgs sandbox; };
in
{
  inherit builders;
  inherit (sandbox) commonTools;
  defaultGoblins = import ./default-goblins { inherit pkgs builders; };
}
