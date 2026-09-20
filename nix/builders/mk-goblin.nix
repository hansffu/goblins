{ pkgs, sandbox }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
  client = import ../packages/internal-goblins.nix { inherit pkgs; };
in
{
  pkg,
  binName,
  outName ? "goblin-${binName}",
  description ? "${binName} running in a Goblins sandbox",
  allowedPackages ? sandbox.commonTools,
  args ? [ ],
  env ? { },
  allowNix ? false,
  allowUnixSockets ? true,
  allowedDomains ? null,
  allowedHostPorts ? [ ],
  publishedPorts ? [ ],
  rwDirs ? [ ],
  rwFiles ? [ ],
  roDirs ? [ ],
  roFiles ? [ ],
  injectedFiles ? { },
  integration ? null,
}:
let
  sandboxOptions = {
    inherit
      pkg
      binName
      outName
      env
      allowNix
      allowUnixSockets
      allowedDomains
      allowedHostPorts
      publishedPorts
      rwDirs
      rwFiles
      roDirs
      roFiles
      ;
    allowedPackages = allowedPackages ++ [ client ];
  };
  wrapped = sandbox.mkSandbox sandboxOptions;
in
assert validate.goblin (
  sandboxOptions
  // {
    inherit
      args
      description
      integration
      injectedFiles
      ;
  }
);
wrapped
// {
  goblin = {
    build_spec = wrapped.buildSpec;
    inherit
      args
      description
      env
      integration
      ;
    client_package = client;
    network = allowedDomains == null;
    sandbox_etc = lib.mapAttrs (
      name: value:
      if builtins.isString value then
        pkgs.writeText "goblins-etc-${builtins.baseNameOf name}" value
      else
        value
    ) injectedFiles;
  };
}
