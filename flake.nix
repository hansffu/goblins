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
      pkgs = import nixpkgs { inherit system; };
      sandbox = agent-sandbox.lib.${system};
      goblinsLib = import ./nix/lib.nix { inherit pkgs sandbox; };
      goblins = goblinsLib.mkGoblins {
        goblins.shell = goblinsLib.goblins.shell;
      };
    in
    {
      lib.${system} = goblinsLib;
      packages.${system} = {
        inherit goblins;
        inherit (goblins) helper;
        default = goblins;
        shell-runtime = goblins.config;
      }
      // import ./tests/fixtures.nix {
        inherit pkgs sandbox;
        inherit (goblins) helper;
      };
      checks.${system} = import ./tests/checks.nix { inherit pkgs goblinsLib sandbox; };
      formatter.${system} = pkgs.nixfmt;
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          goblins
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.clippy
          pkgs.python3
          pkgs.bubblewrap
          pkgs.nix
          pkgs.jq
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
