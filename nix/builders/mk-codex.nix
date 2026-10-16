{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  codex = import ../goblins/codex { inherit pkgs; };
  # Arguments as literal template elements: their braces are text, not
  # placeholders, as in -c inline tables or --mcp-config JSON.
  literal = map (builtins.replaceStrings [ "{" "}" ] [ "{{" "}}" ]);
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
  # `goblins resume` resumes the foreground thread the adapter recorded.
  # Options before `resume`, such as -p and -c, still reach the App Server.
  # This default appends to args, so it suits args made of options only: a
  # goblin whose args contain a prompt, a subcommand or -- sets its own (or
  # null).
  resumeCommand ? literal args ++ [
    "resume"
    "{session_id}"
  ],
  rwDirs ? [ ],
  env ? { },
  injectedFiles ? { },
  devShellInjectedFiles ? { },
  ...
}@options:
assert codex.validate {
  inherit
    pkg
    codexConfigDir
    env
    injectedFiles
    devShellInjectedFiles
    ;
};
mkGoblin (
  codex.goblinOptions options
  // {
    inherit
      binName
      outName
      description
      args
      resumeCommand
      env
      ;
    integration = "codex";
    pkg = codex.mkLauncher {
      inherit
        pkg
        binName
        codexConfigDir
        filterUnavailableMcp
        ;
    };
    allowedPackages = allowedPackages ++ codex.packages;
    rwDirs = pkgs.lib.unique (rwDirs ++ [ codexConfigDir ]);
    injectedFiles = injectedFiles // codex.mkInjectedFiles codexSettings;
    devShellInjectedFiles = devShellInjectedFiles // codex.devShellInjectedFiles;
  }
)
