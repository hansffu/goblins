{ pkgs }:
let
  hook = pkgs.writeShellApplication {
    name = "goblins-codex-hook";
    runtimeInputs = [ pkgs.jq ];
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
    hooks
    // pkgs.lib.genAttrs [ "SessionStart" "UserPromptSubmit" "Stop" ] (
      event: [ handler ] ++ (hooks.${event} or [ ])
    );
}
