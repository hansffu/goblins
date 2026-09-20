{ pkgs, sandbox }:
let
  mkGoblin = import ./mk-goblin.nix { inherit pkgs sandbox; };
in
{
  inherit mkGoblin;
  mkGoblins = import ./mk-goblins.nix { inherit pkgs; };
  mkCodexGoblin = import ./mk-codex.nix {
    inherit pkgs mkGoblin;
    inherit (sandbox) commonTools;
  };
}
