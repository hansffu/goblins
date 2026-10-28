{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  codex = import ../goblins/codex { inherit pkgs; };
  inherit (pkgs) lib;
  # Arguments as literal template elements: their braces are text, not
  # placeholders, as in -c inline tables.
  literal = map (builtins.replaceStrings [ "{" "}" ] [ "{{" "}}" ]);
  # args are the App Server's options: -c, --enable, --disable and -p, each
  # with its value. Prompts, subcommands and permission flags do not apply to
  # a server.
  serverOptions =
    args:
    let
      valued = [
        "-c"
        "--config"
        "--enable"
        "--disable"
        "-p"
        "--profile"
      ];
      attached = arg: builtins.match "-[cp].+|--(config|enable|disable|profile)=.*" arg != null;
      check =
        rest:
        rest == [ ]
        || builtins.elem (builtins.head rest) valued
          && builtins.length rest >= 2
          && check (lib.drop 2 rest)
        || attached (builtins.head rest) && check (builtins.tail rest);
    in
    builtins.all builtins.isString args && check args;
in
{
  pkg ? pkgs.codex,
  binName ? "codex",
  outName ? "goblin-codex",
  description ? "Codex running in a Goblins sandbox",
  codexConfigDir ? "$HOME/.codex",
  codexSettings ? { },
  filterUnavailableMcp ? false,
  allowedPackages ? commonTools,
  args ? [ ],
  # Passed to mkGoblin too, which validates and records it.
  bypassPermissions ? false,
  # The App Server's options: -c, --enable, --disable and -p.
  # `goblins resume` loads the conversation the adapter recorded last.
  resumeCommand ? literal args ++ [
    "--thread"
    "{session_id}"
  ],
  rwDirs ? [ ],
  env ? { },
  injectedFiles ? { },
  devShellInjectedFiles ? { },
  ...
}@options:
assert lib.assertMsg (serverOptions args)
  "mkCodexGoblin: args are App Server options only: -c, --enable, --disable and -p, each with its value";
assert codex.validate {
  inherit
    pkg
    codexConfigDir
    env
    injectedFiles
    devShellInjectedFiles
    ;
};
let
  launcher = codex.mkLauncher {
    inherit
      pkg
      binName
      bypassPermissions
      codexConfigDir
      filterUnavailableMcp
      ;
  };
in
mkGoblin (
  codex.goblinOptions options
  // {
    inherit
      outName
      description
      args
      resumeCommand
      env
      ;
    integration = "codex";
    # The App Server and the adapter run for the goblin's whole life; each
    # attach starts the native terminal as a client of that server, on the
    # conversation the adapter recorded for this run.
    pkg = launcher;
    binName = "goblins-codex-server";
    onAttach.command = [
      "${launcher}/bin/goblins-codex-attach"
      "{session_id}"
    ];
    # `codex` runs its own engine with the goblin's settings.
    allowedPackages = allowedPackages ++ codex.packages ++ [ launcher ];
    rwDirs = pkgs.lib.unique (rwDirs ++ [ codexConfigDir ]);
    injectedFiles =
      injectedFiles
      // codex.mkInjectedFiles {
        settings = codexSettings;
        nix = options.allowNix or false;
        host = options.hostCommands.enable or false;
      };
    devShellInjectedFiles =
      devShellInjectedFiles // codex.devShellInjectedFiles (options.allowNix or false);
  }
)
