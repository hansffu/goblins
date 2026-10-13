{ pkgs }:
let
  hook = pkgs.writeShellApplication {
    name = "goblins-codex-hook";
    runtimeInputs = [ ];
    text = builtins.readFile ./hook.sh;
  };
  handler = {
    hooks = [
      {
        type = "command";
        command = "${hook}/bin/goblins-codex-hook";
        timeout = 5;
      }
    ];
  };
in
{
  package = hook;
  mkHooks =
    hooks:
    hooks // pkgs.lib.genAttrs [ "SessionStart" ] (event: [ handler ] ++ (hooks.${event} or [ ]));
}
