{
  pkgs,
  sandbox,
  helper,
}:
let
  data = pkgs.writeText "goblins-runtime-data" "runtime-data-ok\n";
in
{
  # Retain the already-realized mount gate and runtime-dependency fixtures.
  runtime = pkgs.writeText "goblins-runtime.json" (
    builtins.toJSON {
      shell = "${pkgs.bashInteractive}/bin/bash";
      posix_shell = "${pkgs.bashInteractive}/bin/bash";
      flake = "path:${pkgs.path}";
      client_package = import ../packages/sandbox-cli { inherit pkgs; };
      # Duplicate Bash commands must coexist just as they do on a Nix shell PATH.
      initial_packages = [
        pkgs.python3
        pkgs.bashInteractive
        pkgs.bashNonInteractive
      ];
      python = "${pkgs.python3}/bin/python3";
      helper = "${helper}/bin/goblins-mount-helper";
      bwrap = "${pkgs.bubblewrap}/bin/bwrap";
    }
  );
  inherit (pkgs)
    nix
    jq
    hello
    tree
    cowsay
    ;
  script-tool = pkgs.writeScriptBin "script-tool" ''
    #!${pkgs.dash}/bin/dash
    printf '%s\n' script-interpreter-ok
  '';
  data-tool = pkgs.writeScriptBin "data-tool" ''
    #!${pkgs.dash}/bin/dash
    read -r message < ${data}
    printf '%s\n' "$message"
  '';
  collision = pkgs.writeScriptBin "jq" "#!${pkgs.dash}/bin/dash\nexit 0\n";
  # Its closure holds an output that is itself a symlink to a directory,
  # as Chromium's sandbox output is in mermaid-cli's closure.
  symlink-tool =
    let
      target = pkgs.writeScriptBin "symlink-target" ''
        #!${pkgs.dash}/bin/dash
        printf '%s\n' symlink-output-ok
      '';
      linked = pkgs.runCommand "goblins-symlink-output" { } "ln -s ${target} $out";
    in
    pkgs.writeScriptBin "symlink-tool" ''
      #!${pkgs.dash}/bin/dash
      exec ${linked}/bin/symlink-target
    '';

  fish-shell = sandbox.mkSandbox {
    pkg = pkgs.fish;
    binName = "fish";
    outName = "goblins-fish";
    allowedPackages = sandbox.commonTools ++ [ pkgs.fish ];
    allowNix = false;
    allowUnixSockets = false;
    allowedDomains = [ ];
    allowedHostPorts = [ ];
  };
  # Static agent-sandbox baseline with the intentional Unix transport adjustment.
  agent-sandbox-baseline = sandbox.mkSandbox {
    pkg = pkgs.bashInteractive;
    binName = "bash";
    outName = "goblins-agent-sandbox-baseline";
    allowedPackages = [ pkgs.bashInteractive ];
    allowNix = false;
    allowUnixSockets = true;
  };
}
