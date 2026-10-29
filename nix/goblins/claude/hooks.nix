{ pkgs, adapter }:
let
  hook = pkgs.writeShellApplication {
    name = "goblins-claude-hook";
    runtimeInputs = [
      pkgs.jq
      pkgs.coreutils
      pkgs.gnugrep
      pkgs.util-linux
      adapter
    ];
    text = builtins.readFile ./hook.sh;
  };
  handler = matcher: {
    inherit matcher;
    hooks = [
      {
        type = "command";
        command = "${hook}/bin/goblins-claude-hook";
        timeout = 5;
      }
    ];
  };
  # Turn boundaries drive inbox wakeups; the rest only report activity.
  events = {
    SessionStart = "";
    UserPromptSubmit = "";
    Stop = "";
    StopFailure = "";
    PreToolUse = "AskUserQuestion|ExitPlanMode";
    # Every tool end, so waiting for an approval ends with its tool.
    PostToolUse = "";
    PostToolUseFailure = "";
    Elicitation = "";
    ElicitationResult = "";
    PermissionRequest = "";
    Notification = "idle_prompt|permission_prompt|elicitation_dialog|elicitation_url_dialog";
  };
in
{
  package = hook;
  mkHooks =
    hooks:
    hooks
    // builtins.mapAttrs (event: matcher: [ (handler matcher) ] ++ (hooks.${event} or [ ])) events;
}
