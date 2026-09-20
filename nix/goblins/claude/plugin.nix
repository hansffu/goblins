{ pkgs }:
pkgs.symlinkJoin {
  name = "goblins-claude-plugin";
  paths = [
    (pkgs.writeTextDir ".claude-plugin/plugin.json" (
      builtins.toJSON {
        name = "goblins-daemon";
        description = "Goblins daemon inbox notifications for Claude Code";
      }
    ))
    (pkgs.writeTextDir "monitors/monitors.json" (
      builtins.toJSON [
        {
          name = "goblins-inbox";
          description = "Goblins daemon inbox messages";
          command = ''/run/goblins/bin/goblins integration watch --epoch "$GOBLINS_INTEGRATION_EPOCH" --delivery notification'';
        }
      ]
    ))
  ];
}
