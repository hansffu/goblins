{
  description = "Goblins: sandboxed shells with approved live Nix package grants";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/21a67dc470149f337cecafbe965d8d252a390518";
    agent-sandbox = {
      url = "github:archie-judd/agent-sandbox.nix/566defa1560051c4e48049389f942a6c1a53aee9";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, agent-sandbox, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "claude-code";
      };
      sandbox = agent-sandbox.lib.${system};
      goblinsLib = import ./nix/lib.nix { inherit pkgs sandbox; };
      goblins = goblinsLib.builders.mkGoblins {
        goblins = goblinsLib.defaultGoblins;
      };
      dockerShell = import ./nix/default-goblins/shell.nix {
        inherit pkgs;
        inherit (goblinsLib.builders) mkGoblin;
        docker.enable = true;
      };
      devGoblins = goblinsLib.builders.mkGoblins {
        goblins = goblinsLib.defaultGoblins // {
          shell = dockerShell;
        };
      };
    in
    {
      lib.${system} = goblinsLib;
      packages.${system} = {
        inherit goblins;
        inherit (goblins) helper;
        default = goblins;
        shell-runtime = goblins.config;
        docker-shell = goblinsLib.builders.mkGoblins {
          goblins.shell = dockerShell;
        };
      }
      // import ./tests/fixtures.nix {
        inherit pkgs sandbox;
        inherit (goblins) helper;
      };
      checks.${system} = import ./tests/checks.nix { inherit pkgs goblinsLib sandbox; };
      formatter.${system} = pkgs.nixfmt;
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          devGoblins
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.clippy
          (pkgs.python3.withPackages (p: [ p.pyte ]))
          pkgs.bubblewrap
          pkgs.nix
          pkgs.jq
          pkgs.bashInteractive
          pkgs.fish
          pkgs.zsh
        ];
      };
      # Only validated attributes from this pinned package set are requestable.
      legacyPackages.${system} = pkgs;
      apps.${system}.default = {
        type = "app";
        program = "${goblins}/bin/goblins";
        meta.description = "Sandboxed goblins with approved live Nix package grants";
      };
    };
}
