{
  pkgs,
  goblins,
  configurations,
}:
let
  inherit (pkgs) lib;
  helper = import ./rust.nix {
    inherit pkgs;
    package = "goblins-mount-helper";
  };
  controller = import ./rust.nix {
    inherit pkgs;
    package = "goblins-controller";
  };
  config = pkgs.writeText "goblins-config.json" (
    builtins.toJSON {
      api = 1;
      helper_api = 3;
      goblins = lib.mapAttrs (
        _: configuration:
        configuration
        // {
          # Keep mkSandbox's policy/closure specification, but remove references
          # used only by its static Python launcher. The Rust adapter uses
          # bwrap and pasta; configured packages may still include Python.
          build_spec = pkgs.runCommand "goblins-live-spec.json" { nativeBuildInputs = [ pkgs.jq ]; } ''
            jq '.dependencies = {bwrap: .dependencies.bwrap, pasta: .dependencies.pasta} | del(.proxy, .pre_entry_script, .env_keys, .hosts_file, .empty_file)' \
              ${configuration.build_spec} > $out
          '';
          helper = "${helper}/bin/goblins-mount-helper";
          # Resolve approved attributes directly from our pinned nixpkgs source.
          flake = "path:${pkgs.path}";
        }
      ) configurations;
    }
  );
  package =
    (pkgs.writeShellScriptBin "goblins" ''
      export PATH=${pkgs.nix}/bin:$PATH
      exec ${controller}/bin/goblins --runtime ${config} "$@"
    '').overrideAttrs
      (old: {
        nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.installShellFiles ];
        buildCommand = old.buildCommand + ''
          for shell in bash fish zsh; do
            ${controller}/bin/goblins --runtime ${config} completions "$shell" > "goblins.$shell"
          done
          installShellCompletion --bash --name goblins goblins.bash
          installShellCompletion --fish --name goblins.fish goblins.fish
          installShellCompletion --zsh --name _goblins goblins.zsh
        '';
      });
in
package // { inherit config goblins helper; }
