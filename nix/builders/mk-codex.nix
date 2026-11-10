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
  agentLaunchOptions = import ./agent-launch-options.nix { inherit lib; };
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
  # model and effort take a list of names; see agent-launch-options.nix.
  launchOptions ? { },
  ...
}@options:
assert lib.assertMsg (serverOptions args)
  "mkCodexGoblin: args are App Server options only: -c, --enable, --disable and -p, each with its value";
let
  expandedLaunchOptions = agentLaunchOptions "mkCodexGoblin" {
    model = name: [
      "-c"
      "model=\"${name}\""
    ];
    effort = name: [
      "-c"
      "model_reasoning_effort=\"${name}\""
    ];
  } launchOptions;
  choiceArgs = lib.concatMap (
    option: lib.concatMap (choice: choice.args or [ ]) (builtins.attrValues (option.choices or { }))
  ) (builtins.attrValues expandedLaunchOptions);
  choices = lib.concatMap (option: builtins.attrValues (option.choices or { })) (
    builtins.attrValues expandedLaunchOptions
  );
  # filterUnavailableMcp probes `codex mcp list` with the raw arguments, and
  # Codex refuses a repeated --profile: the profile stays in args.
  selectsProfile = arg: builtins.match "-p.*|--profile(=.*)?" arg != null;
in
assert lib.assertMsg (builtins.all (choice: serverOptions (choice.args or [ ])) choices)
  "mkCodexGoblin: launchOptions choices add App Server options only: -c, --enable and --disable, each with its value";
assert lib.assertMsg (!builtins.any selectsProfile choiceArgs)
  "mkCodexGoblin: launchOptions choices cannot select a profile (-p, --profile); set it in args";
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
    launchOptions = expandedLaunchOptions;
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
