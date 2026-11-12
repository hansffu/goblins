{
  description = "Goblins: sandboxed shells with approved live Nix package grants";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/21a67dc470149f337cecafbe965d8d252a390518";
    # Only for the Emacs the tests run: a current Emacs and Ghostel, as the
    # Emacs package expects, independent of the developer's own Emacs.
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Agent packages for the demo goblins, updated daily. Not following
    # nixpkgs: this is the combination numtide builds and caches.
    llm-agents.url = "github:numtide/llm-agents.nix/69d1f4621804d13b9658ff9ac9b2a151815132cb";
    agent-sandbox = {
      url = "github:archie-judd/agent-sandbox.nix/566defa1560051c4e48049389f942a6c1a53aee9";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      nixpkgs-unstable,
      agent-sandbox,
      llm-agents,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "claude-code";
      };
      sandbox = agent-sandbox.lib.${system};
      goblinsLib = import ./nix/lib.nix { inherit pkgs sandbox; };
      # Development-only configurations: the dev shell's goblins and the
      # test suite's daemon. Users declare their own with the builders.
      demoGoblins = import ./nix/demo-goblins {
        inherit pkgs;
        inherit (goblinsLib) builders commonTools;
        agents = llm-agents.packages.${system};
      };
      demoScopes = import ./nix/demo-goblins/scopes.nix { inherit (goblinsLib.builders) mkScope; };
      demo = goblinsLib.builders.mkGoblins {
        goblins = demoGoblins;
        scopes = demoScopes;
      };
      # The demo goblins for the Python tests, whose shells start shells.
      testGoblins = goblinsLib.builders.mkGoblins {
        goblins = demoGoblins // {
          shell = import ./nix/demo-goblins/shell.nix {
            inherit pkgs;
            inherit (goblinsLib.builders) mkGoblin;
            options.allowedRunTargets = [ "shell" ];
          };
        };
        scopes = demoScopes;
      };
      dockerShell = import ./nix/demo-goblins/shell.nix {
        inherit pkgs;
        inherit (goblinsLib.builders) mkGoblin;
        options = {
          scope = "dev";
          docker.enable = true;
        };
      };
      # A temporary scope: its Docker data is deleted after the last member.
      devScopes.dev = goblinsLib.builders.mkScope {
        persistent = false;
        docker.enable = true;
      };
      devGoblins = goblinsLib.builders.mkGoblins {
        scopes = demoScopes // devScopes;
        editor.emacs.enable = true;
        # The shell runs in the dev scope, which its agent children join.
        goblins =
          import ./nix/demo-goblins {
            inherit pkgs;
            inherit (goblinsLib) builders commonTools;
            agents = llm-agents.packages.${system};
            agentScopes = [ "dev" ];
          }
          // {
            shell = dockerShell;
          };
      };
      # The Emacs the tests run, with the packages the Emacs code and its
      # optional integrations need, so no test depends on a local Emacs.
      testEmacs =
        let
          unstable = nixpkgs-unstable.legacyPackages.${system};
        in
        (unstable.emacsPackagesFor unstable.emacs-nox).emacsWithPackages (e: [
          e.magit-section
          e.transient
          e.ghostel
          e.evil
          e.consult
          e.embark
          e.embark-consult
          e.marginalia
        ]);
      devShell = pkgs.mkShell {
        # Not on PATH: `just emacs` keeps running the developer's own Emacs.
        GOBLINS_TEST_EMACS = "${testEmacs}/bin/emacs";
        packages = [
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.clippy
          pkgs.just
          pkgs.git
          (pkgs.python3.withPackages (p: [ p.pyte p.jsonschema ]))
          pkgs.bubblewrap
          pkgs.nix
          pkgs.nixfmt
          pkgs.jq
          pkgs.nodejs
          pkgs.bashInteractive
          pkgs.fish
          pkgs.zsh
        ];
      };
    in
    {
      lib.${system} = goblinsLib;
      packages.${system} = {
        inherit (demo) helper;
        # The demo goblins, for trying and testing a checkout (`just emacs`).
        # Not in the dev shell: inside a goblin, `nix develop` would put this
        # host CLI ahead of the sandbox's own `goblin`.
        dev-goblins = devGoblins;
        openrpc-playground = import ./nix/openrpc-playground.nix { inherit pkgs; };
      }
      // import ./tests/fixtures.nix {
        inherit pkgs sandbox;
        inherit (demo) helper;
      };
      checks.${system} = import ./tests/checks.nix {
        inherit
          pkgs
          goblinsLib
          sandbox
          devShell
          devGoblins
          ;
        demo = testGoblins;
        agents = llm-agents.packages.${system};
      };
      formatter.${system} = pkgs.nixfmt;
      devShells.${system}.default = devShell;
      # Only validated attributes from this pinned package set are requestable.
      legacyPackages.${system} = pkgs;
    };
}
