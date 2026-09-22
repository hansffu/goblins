{ pkgs, sandbox }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
  client = import ../packages/internal-goblins.nix { inherit pkgs; };
  dockerDaemon = import ../packages/docker.nix { inherit pkgs; };
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
  docker ? { },
}:
let
  dockerOptions = {
    enable = false;
  }
  // docker;
  effectiveEnv = {
    # Ryuk needs the session Docker socket, not a writable sysfs mount.
    # Keep its cleanup enabled without requesting a privileged container.
    TESTCONTAINERS_RYUK_CONTAINER_PRIVILEGED = "false";
  }
  // env;
  sandboxOptions = {
    inherit
      pkg
      binName
      outName
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
    env = effectiveEnv;
    allowedPackages =
      allowedPackages ++ [ client ] ++ lib.optional dockerOptions.enable pkgs.docker-client;
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
    docker = dockerOptions;
  }
);
wrapped
// {
  goblin = {
    build_spec = wrapped.buildSpec;
    inherit
      args
      description
      integration
      ;
    env = effectiveEnv;
    client_package = client;
    network = allowedDomains == null;
    docker = {
      daemon = "${dockerDaemon}/bin/dockerd";
      client = "${pkgs.docker-client}";
      enabled = dockerOptions.enable;
    };
    sandbox_etc = lib.mapAttrs (
      name: value:
      if builtins.isString value then
        pkgs.writeText "goblins-etc-${builtins.baseNameOf name}" value
      else
        value
    ) injectedFiles;
  };
}
