{
  pkgs,
  mkGoblin,
  commonTools,
}:
{
  pkg ? pkgs.codex,
  binName ? "codex",
  codexConfigDir ? "$HOME/.codex",
  codexSettings ? { },
  ...
}@options:
let
  inherit (pkgs) lib;
  # A runtime path string, never a Nix path containing credentials. Expand only
  # HOME/tilde here; all other mkGoblin environment values remain literal.
  homePrefixes = [
    "$HOME/"
    "\${HOME}/"
    "~/"
  ];
  homePrefix = lib.findFirst (prefix: lib.hasPrefix prefix codexConfigDir) null homePrefixes;
  directory =
    if homePrefix != null then
      ''"$HOME"/${lib.escapeShellArg (lib.removePrefix homePrefix codexConfigDir)}''
    else
      lib.escapeShellArg codexConfigDir;
  hook = pkgs.writeShellApplication {
    name = "goblins-codex-hook";
    runtimeInputs = [ pkgs.jq ];
    text = builtins.readFile ./codex/hook.sh;
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
  hooks = (codexSettings.hooks or { }) // {
    SessionStart = [ handler ] ++ (codexSettings.hooks.SessionStart or [ ]);
    Stop = [ handler ] ++ (codexSettings.hooks.Stop or [ ]);
  };
  etc = {
    "codex/config.toml" = (pkgs.formats.toml { }).generate "goblins-codex.toml" (
      codexSettings // { inherit hooks; }
    );
    "codex/skills/goblins/SKILL.md" = builtins.readFile ./codex/goblins/SKILL.md;
  };
  launcher = pkgs.writeShellScriptBin binName ''
    export CODEX_HOME=${directory}
    # Goblins owns the outer sandbox and package approvals. Nested namespace
    # creation is prohibited by its seccomp policy.
    exec ${lib.escapeShellArg "${pkg}/bin/${binName}"} \
      --sandbox danger-full-access --ask-for-approval never --enable hooks "$@"
  '';
  forwarded = builtins.removeAttrs options [
    "pkg"
    "binName"
    "codexConfigDir"
    "codexSettings"
  ];
in
assert lib.assertMsg (builtins.isString codexConfigDir)
  "mkCodexGoblin: codexConfigDir must be a runtime path string";
assert lib.assertMsg (
  !(pkg ? version) || lib.versionAtLeast pkg.version "0.146.0"
) "mkCodexGoblin: Codex 0.146.0 or later is required for the managed hook setup";
assert lib.assertMsg (
  codexConfigDir != "" && (lib.hasPrefix "/" codexConfigDir || homePrefix != null)
) "mkCodexGoblin: codexConfigDir must be absolute or start with ~/ or $HOME/";
assert lib.assertMsg (
  !((options.env or { }) ? CODEX_HOME)
) "mkCodexGoblin: use codexConfigDir instead of env.CODEX_HOME";
assert lib.assertMsg (
  lib.intersectLists (builtins.attrNames etc) (builtins.attrNames (options.sandboxEtc or { })) == [ ]
) "mkCodexGoblin: sandboxEtc cannot replace the Goblins Codex config or skill";
mkGoblin (
  forwarded
  // {
    pkg = launcher;
    inherit binName;
    outName = options.outName or "goblin-codex";
    allowedPackages = (options.allowedPackages or commonTools) ++ [
      pkgs.fish
      hook
    ];
    rwDirs = lib.unique ((options.rwDirs or [ ]) ++ [ codexConfigDir ]);
    sandboxEtc = (options.sandboxEtc or { }) // etc;
  }
)
