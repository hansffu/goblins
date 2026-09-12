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
      python = "${pkgs.python3}/bin/python3";
      helper = "${helper}/bin/goblins-mount-helper";
      bwrap = "${pkgs.bubblewrap}/bin/bwrap";
    }
  );
  inherit (pkgs) jq hello tree;
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
  # Static upstream baseline with the intentional Unix transport adjustment.
  upstream-baseline = sandbox.mkSandbox {
    pkg = pkgs.bashInteractive;
    binName = "bash";
    outName = "goblins-upstream-baseline";
    allowedPackages = [ pkgs.bashInteractive ];
    allowNix = false;
    allowUnixSockets = true;
  };
}
