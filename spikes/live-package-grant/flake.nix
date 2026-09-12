{
  description = "Goblins live package mount proof of concept (Linux x86_64)";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/21a67dc470149f337cecafbe965d8d252a390518";
    agent-sandbox = {
      url = "github:archie-judd/agent-sandbox.nix/566defa1560051c4e48049389f942a6c1a53aee9";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = { self, nixpkgs, agent-sandbox }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      sandbox = agent-sandbox.lib.${system};
      commonTools = sandbox.commonTools;
      helper = pkgs.rustPlatform.buildRustPackage {
        pname = "goblins-mount-helper";
        version = "0.1.0";
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type: type == "directory" && baseNameOf path == "src"
            || builtins.elem (baseNameOf path) [ "Cargo.toml" "Cargo.lock" "main.rs" ];
        };
        cargoLock.lockFile = ./Cargo.lock;
      };
      data = pkgs.writeText "goblins-runtime-data" "runtime-data-ok\n";
      scriptTool = pkgs.writeScriptBin "script-tool" ''
        #!${pkgs.dash}/bin/dash
        printf '%s\n' script-interpreter-ok
      '';
      dataTool = pkgs.writeScriptBin "data-tool" ''
        #!${pkgs.dash}/bin/dash
        read -r message < ${data}
        printf '%s\n' "$message"
      '';
      runtime = pkgs.writeText "goblins-runtime.json" (builtins.toJSON {
        shell = "${pkgs.bashInteractive}/bin/bash";
        python = "${pkgs.python3}/bin/python3";
        helper = "${helper}/bin/goblins-mount-helper";
        bwrap = "${pkgs.bubblewrap}/bin/bwrap";
      });
      catalogSource = pkgs.lib.cleanSourceWith {
        src = ./.;
        filter = path: type:
          !(builtins.elem (baseNameOf path) [ "target" "__pycache__" ]
            || pkgs.lib.hasPrefix "result" (baseNameOf path));
      };
      shellRuntime = pkgs.writeText "goblins-shell-runtime.json" (builtins.toJSON {
        shell = "${pkgs.fish}/bin/fish";
        shell_args = [ "--no-config" "--interactive" ];
        initial_packages = map (p: "${pkgs.lib.getBin p}") commonTools;
        python = "${pkgs.python3}/bin/python3";
        helper = "${helper}/bin/goblins-mount-helper";
        bwrap = "${pkgs.bubblewrap}/bin/bwrap";
        flake = "path:${catalogSource}";
      });
      controller = pkgs.runCommand "goblins-controller" { } ''
        mkdir -p $out
        cp ${./goblins.py} $out/goblins.py
        cp ${./runtime.py} $out/runtime.py
        cp ${./request.py} $out/request.py
      '';
      goblins = pkgs.writeShellScriptBin "goblins" ''
        export PATH=${pkgs.nix}/bin:$PATH
        exec ${pkgs.python3}/bin/python3 ${controller}/goblins.py --runtime ${shellRuntime} "$@"
      '';
    in {
      # The request controller selects validated attribute paths from this pinned
      # package set, then chooses the executable output on the trusted side.
      legacyPackages.${system} = pkgs;
      packages.${system} = {
        default = goblins;
        inherit helper runtime goblins;
        shell-runtime = shellRuntime;
        jq = pkgs.jq;
        hello = pkgs.hello;
        tree = pkgs.tree;
        script-tool = scriptTool;
        data-tool = dataTool;
        collision = pkgs.writeScriptBin "jq" "#!${pkgs.dash}/bin/dash\nexit 0\n";
        fish-shell = sandbox.mkSandbox {
          pkg = pkgs.fish;
          binName = "fish";
          outName = "goblins-fish";
          allowedPackages = commonTools ++ [ pkgs.fish ];
          allowNix = false;
          allowUnixSockets = false;
          allowedDomains = [ ];
          allowedHostPorts = [ ];
        };
        # Evaluates the preferred upstream with the intentional Unix transport
        # adjustment. The live launcher adapter is runtime.py, not this wrapper.
        upstream-baseline = agent-sandbox.lib.${system}.mkSandbox {
          pkg = pkgs.bashInteractive;
          binName = "bash";
          outName = "goblins-upstream-baseline";
          allowedPackages = [ pkgs.bashInteractive ];
          allowNix = false;
          allowUnixSockets = true;
        };
      };
      apps.${system}.default = {
        type = "app";
        program = "${goblins}/bin/goblins";
      };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ goblins pkgs.rustc pkgs.cargo pkgs.python3 pkgs.bubblewrap pkgs.nix pkgs.jq ];
      };
    };
}
