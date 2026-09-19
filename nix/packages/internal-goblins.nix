{ pkgs }:
let
  client = import ./rust.nix {
    inherit pkgs;
    package = "goblins-client";
  };
in
pkgs.runCommand "goblins-request-client" { nativeBuildInputs = [ pkgs.installShellFiles ]; } ''
  mkdir -p $out/bin
  ln -s ${client}/bin/goblins-client $out/bin/goblins
  ln -s ${client}/bin/goblins-client $out/bin/goblins-request
  for shell in bash fish zsh; do
    ${client}/bin/goblins-client completions "$shell" > "goblins.$shell"
  done
  installShellCompletion --bash --name goblins goblins.bash
  installShellCompletion --fish --name goblins.fish goblins.fish
  installShellCompletion --zsh --name _goblins goblins.zsh
''
