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
      # The agent's arguments when `goblins recreate` resumes its recorded
      # conversation, instead of `args`: {key} is the variable the goblin
      # recorded (`goblin variable set`), {{ and }} are literal braces. Each
      # element stays one argument. Null: a recreated goblin starts afresh.
      resumeCommand ? null,
      # { command, timeout ? 30, waitFor ? [ ] }: run the main command without
      # a terminal for the goblin's whole life, and start `command` on a new
      # terminal at each attach, once every {key} in it and every waitFor name
      # was set during this run (`goblin variable set`), waiting up to timeout
      # seconds. Placeholders work as in resumeCommand.
      onAttach ? null,
      # The argument list run when a detach is requested, before the attached
      # client is stopped; {attach.pid} is that client's process ID. Null:
      # hang up the client.
      onDetach ? null,
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
      # enable: the goblin may ask the host to run commands outside its
      # sandbox (`goblin host run`), each approved like a package request.
      hostCommands ? { },
      # The agent skips its own permission prompts: Claude Code's
      # --dangerously-skip-permissions, Codex's approval policy "never". Off,
      # Goblins requests no bypass and the agent's own settings decide. Agent
      # goblins only.
      bypassPermissions ? false,
      scope ? null,
      allowedScopes ? [ ],
      scopeStorage ? { },
      # Configurations this one may run as raid members (goblin run); none by default, not
      # even itself. Not a scope default: mkGoblins resolves it once per
      # configuration.
      allowedRunTargets ? [ ],
    }@options:
    let
      dockerOptions = {
        enable = false;
      }
      // docker;
      hostCommandOptions = {
        enable = false;
      }
      // hostCommands;
      effectiveEnv = {
        # Ryuk needs the session Docker socket, not a writable sysfs mount.
        # Keep its cleanup enabled without requesting a privileged container.
        TESTCONTAINERS_RYUK_CONTAINER_PRIVILEGED = "false";
      }
      # The sandbox has no /etc/nix: enable the flake CLI, as agent-sandbox.nix
      # documents. A goblin's own env.NIX_CONFIG wins.
      // lib.optionalAttrs allowNix {
        NIX_CONFIG = "experimental-features = nix-command flakes\naccept-flake-config = false";
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
          resumeCommand
          onAttach
          onDetach
          description
          integration
          injectedFiles
          devShellInjectedFiles
          scope
          allowedScopes
          scopeStorage
          allowedRunTargets
          bypassPermissions
          ;
        docker = dockerOptions;
        hostCommands = hostCommandOptions;
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
        allowed_run_targets = allowedRunTargets;
        resume_command = resumeCommand;
        on_attach =
          if onAttach == null then
            null
          else
            {
              inherit (onAttach) command;
              timeout = onAttach.timeout or 30;
              wait_for = onAttach.waitFor or [ ];
            };
        on_detach = onDetach;
        scope_storage = scopeStorage;
        env = effectiveEnv;
        client_package = client;
        network = allowedDomains == null;
        # For mkGoblins' scope check only; the daemon reads the build spec.
        nix = allowNix;
        host_commands = hostCommandOptions.enable;
        bypass_permissions = bypassPermissions;
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
