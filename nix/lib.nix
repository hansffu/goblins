{ pkgs, sandbox }:
let
  mkGoblin = import ./mk-goblin.nix { inherit pkgs sandbox; };
  mkGoblins = import ./mk-goblins.nix { inherit pkgs; };
  mkCodexGoblin = import ./goblins/mk-codex.nix {
    inherit pkgs mkGoblin;
    inherit (sandbox) commonTools;
  };
in
{
  inherit mkGoblin mkCodexGoblin mkGoblins;
  inherit (sandbox) commonTools;
  goblins.shell = import ./goblins/shell.nix { inherit pkgs mkGoblin; };
  goblins.codex = mkCodexGoblin { };
}
