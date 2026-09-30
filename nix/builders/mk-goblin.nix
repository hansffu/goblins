{ pkgs, sandbox }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
  client = import ../packages/internal-goblins.nix { inherit pkgs; };
  dockerDaemon = import ../packages/docker.nix { inherit pkgs; };
  # Scope defaults come first; the goblin's own values win on conflicts.
  withDefaults =
    defaults: options:
    let
      list = name: fallback: lib.unique ((defaults.${name} or [ ]) ++ (options.${name} or fallback));
      attrs = name: (defaults.${name} or { }) // (options.${name} or { });
    in
    options
    // {
      allowedPackages = list "allowedPackages" sandbox.commonTools;
      rwDirs = list "rwDirs" [ ];
      rwFiles = list "rwFiles" [ ];
      roDirs = list "roDirs" [ ];
      roFiles = list "roFiles" [ ];
      env = attrs "env";
      injectedFiles = attrs "injectedFiles";
      scopeStorage = attrs "scopeStorage";
    };
  mkGoblin =
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
      # Mounted like injectedFiles, only in sandboxes launched with a dev shell.
      devShellInjectedFiles ? { },
      integration ? null,
      docker ? { },
      scope ? null,
      allowedScopes ? [ ],
      scopeStorage ? { },
      # Configurations this one may launch as children; null means only itself.
      # Not a scope default: mkGoblins resolves it once per configuration.
      allowedChildren ? null,
    }@options:
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
      etcFiles = lib.mapAttrs (
        name: value:
        if builtins.isString value then
          pkgs.writeText "goblins-etc-${builtins.baseNameOf name}" value
        else
          value
      );
    in
    assert validate.goblin (
      sandboxOptions
      // {
        inherit
          args
          description
          integration
          injectedFiles
          devShellInjectedFiles
          scope
          allowedScopes
          scopeStorage
          allowedChildren
          ;
        docker = dockerOptions;
      }
    );
    wrapped
    // {
      # mkGoblins renders one configuration per scope the goblin may join.
      withScopeDefaults = defaults: mkGoblin (withDefaults defaults options);
      goblin = {
        build_spec = wrapped.buildSpec;
        inherit
          args
          description
          integration
          scope
          ;
        allowed_scopes = allowedScopes;
        allowed_children = allowedChildren;
        scope_storage = scopeStorage;
        env = effectiveEnv;
        client_package = client;
        network = allowedDomains == null;
        docker = {
          daemon = "${dockerDaemon}/bin/dockerd";
          client = "${pkgs.docker-client}";
          enabled = dockerOptions.enable;
        };
        sandbox_etc = etcFiles injectedFiles;
        dev_shell_sandbox_etc = etcFiles devShellInjectedFiles;
      };
    };
in
mkGoblin
