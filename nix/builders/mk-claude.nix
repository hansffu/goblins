{
  pkgs,
  mkGoblin,
  commonTools,
}:
let
  claude = import ../goblins/claude { inherit pkgs; };
  # Arguments as literal template elements: their braces are text, not
  # placeholders, as in -c inline tables or --mcp-config JSON.
  literal = map (builtins.replaceStrings [ "{" "}" ] [ "{{" "}}" ]);
in
{
  pkg ? pkgs.claude-code,
  binName ? "claude",
  outName ? "goblin-claude",
  description ? "Claude Code running in a Goblins sandbox",
  claudeConfigDir ? "$HOME/.claude",
  claudeSettings ? { },
  allowedPackages ? commonTools,
  args ? [ ],
  # `goblins resume` resumes the conversation the hooks recorded. This
  # default appends to args, so it suits args made of options only: a goblin
  # whose args contain a prompt, a subcommand or -- sets its own (or null).
  resumeCommand ? literal args ++ [
    "--resume"
    "{session_id}"
  ],
  rwDirs ? [ ],
  env ? { },
  injectedFiles ? { },
  devShellInjectedFiles ? { },
  ...
}@options:
assert claude.validate {
  inherit
    claudeConfigDir
    env
    injectedFiles
    devShellInjectedFiles
    ;
};
mkGoblin (
  claude.goblinOptions options
  // {
    inherit
      binName
      outName
      description
      args
      resumeCommand
      env
      ;
    integration = "claude";
    pkg = claude.mkLauncher {
      inherit
        pkg
        binName
        claudeConfigDir
        ;
      inherit (claude) plugin;
    };
    allowedPackages = allowedPackages ++ claude.packages;
    rwDirs = pkgs.lib.unique (rwDirs ++ [ claudeConfigDir ]);
    injectedFiles = injectedFiles // claude.mkInjectedFiles claudeSettings;
    devShellInjectedFiles = devShellInjectedFiles // claude.devShellInjectedFiles;
  }
)
