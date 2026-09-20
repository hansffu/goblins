{ pkgs, builders }:
{
  shell = import ./shell.nix {
    inherit pkgs;
    inherit (builders) mkGoblin;
  };
  codex = builders.mkCodexGoblin { };
}
