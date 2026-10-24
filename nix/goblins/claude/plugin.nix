{ pkgs }:
let
  inherit (pkgs) lib;
  monitor = pkgs.writeShellApplication {
    name = "goblins-claude-monitor";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.jq
    ];
    text = builtins.readFile ./monitor.sh;
  };
  # Read-only messages.get for the mod's views; the goblin CLI has no command for it.
  message = pkgs.writeShellApplication {
    name = "goblins-claude-message";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.jq
      pkgs.socat
    ];
    text = builtins.readFile ./rpc.sh;
  };
  # The mod's source, which Claude Code runs as TypeScript; its tests stay out.
  mod = lib.fileset.toSource {
    root = ./mod;
    fileset = lib.fileset.difference ./mod (
      lib.fileset.fileFilter (file: lib.hasInfix ".test." file.name) ./mod
    );
  };
  manifest = builtins.toJSON {
    name = "goblins-daemon";
    version = "0.2.0";
    description = "Goblins daemon inbox notifications for Claude Code";
    types = "./types/index.d.ts";
  };
  monitors = builtins.toJSON [
    {
      name = "goblins-inbox";
      # The mod finds the monitor's notification rows by this description.
      description = "Goblins daemon inbox messages";
      command = "${monitor}/bin/goblins-claude-monitor";
    }
  ];
in
pkgs.runCommand "goblins-claude-plugin"
  {
    inherit manifest monitors;
    passAsFile = [
      "manifest"
      "monitors"
    ];
    # The tests and checks load the mod with its tests beside it.
    passthru.source = ./mod;
  }
  ''
    mkdir -p $out/.claude-plugin $out/monitors $out/bin
    cp -r ${mod}/. $out/
    chmod -R u+w $out
    cp $manifestPath $out/.claude-plugin/plugin.json
    cp $monitorsPath $out/monitors/monitors.json
    ln -s ${message}/bin/goblins-claude-message $out/bin/goblins-claude-message
  ''
