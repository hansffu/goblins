{ pkgs }:
let
  client = import ./rust.nix {
    inherit pkgs;
    package = "goblins-client";
  };
  adapter = import ./rust.nix {
    inherit pkgs;
    package = "goblins-codex";
  };
in
pkgs.runCommand "goblins-request-client" { nativeBuildInputs = [ pkgs.installShellFiles ]; } ''
  mkdir -p $out/bin
  ln -s ${adapter}/bin/goblins-codex $out/bin/goblins-codex
  # `goblin`, not the host's `goblins`: a dev shell that ships the host CLI
  # cannot shadow it inside a sandbox.
  ln -s ${client}/bin/goblins-client $out/bin/goblin
  # The old name, for conversations recorded before the rename. Not completed
  # or documented.
  ln -s ${client}/bin/goblins-client $out/bin/goblins
  ln -s ${client}/bin/goblins-client $out/bin/goblins-request
  # argv[0] aliases: the sandbox root process and the EDITOR/VISUAL command.
  ln -s ${client}/bin/goblins-client $out/bin/goblins-init
  ln -s ${client}/bin/goblins-client $out/bin/goblins-editor
  for shell in bash fish zsh; do
    ${client}/bin/goblins-client completions "$shell" > "goblin.$shell"
  done
  installShellCompletion --bash --name goblin goblin.bash
  installShellCompletion --fish --name goblin.fish goblin.fish
  installShellCompletion --zsh --name _goblin goblin.zsh
''
