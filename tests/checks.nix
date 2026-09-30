{
  pkgs,
  goblinsLib,
  sandbox,
}:
let
  inherit (goblinsLib.builders)
    mkGoblin
    mkCodexGoblin
    mkClaudeGoblin
    mkGoblins
    mkScope
    ;
  base = {
    pkg = pkgs.bashInteractive;
    binName = "bash";
  };
  configured = mkGoblins {
    goblins = {
      fishy = mkGoblin {
        pkg = pkgs.fish;
        binName = "fish";
        args = [
          "--no-config"
          "--interactive"
        ];
        allowedPackages = [ pkgs.coreutils ];
        env.GOBLIN_MARKER = "literal $HOME $(false)";
        allowedChildren = [
          "fishy"
          "utility"
        ];
      };
      utility = mkGoblin (
        base
        // {
          args = [
            "--noprofile"
            "--norc"
            "-i"
          ];
          allowedPackages = [
            pkgs.coreutils
            pkgs.tree
          ];
          env = {
            GOBLIN_MARKER = "utility";
            PS1 = "utility> ";
          };
          allowedChildren = [ ];
        }
      );
    };
  };
  rejected = configuration: !(builtins.tryEval (mkGoblins configuration).config.drvPath).success;
  invalid = [
    { goblins = { }; }
    { goblins.bad = pkgs.hello; }
    { goblins."../bad" = mkGoblin base; }
    {
      docker.scopes = [ "work" ];
      goblins.shell = mkGoblin base;
    }
    {
      scopes."../bad" = mkScope { };
      goblins.shell = mkGoblin base;
    }
    {
      scopes.work = { };
      goblins.shell = mkGoblin base;
    }
    { goblins.shell = mkGoblin (base // { scope = "missing"; }); }
    {
      scopes.work = mkScope { };
      goblins.shell = mkGoblin (base // { allowedScopes = [ "missing" ]; });
    }
    {
      scopes.work = mkScope { };
      goblins.shell = mkGoblin (
        base
        // {
          scope = "work";
          scopeStorage.cache = "/var/cache/x";
        }
      );
    }
    {
      scopes.work = mkScope { storage.cache = { }; };
      goblins.shell = mkGoblin (
        base
        // {
          allowedScopes = [ "work" ];
          scopeStorage.cache = "/var/cache/x";
        }
      );
    }
    {
      scopes.work = mkScope { };
      goblins = {
        online = mkGoblin (base // { scope = "work"; });
        offline = mkGoblin (
          base
          // {
            scope = "work";
            allowedDomains = [ ];
          }
        );
      };
    }
  ]
  ++ [
    { goblins.shell = mkGoblin (base // { allowedChildren = [ "missing" ]; }); }
    # Over the bound: 65 declared goblins, or a name longer than 64 characters.
    (
      let
        names = map (i: "g${toString i}") (pkgs.lib.range 1 65);
      in
      {
        goblins = pkgs.lib.genAttrs names (_: mkGoblin (base // { allowedChildren = names; }));
      }
    )
    {
      goblins = {
        shell = mkGoblin (base // { allowedChildren = [ (pkgs.lib.fixedWidthString 65 "x" "") ]; });
        ${pkgs.lib.fixedWidthString 65 "x" ""} = mkGoblin base;
      };
    }
  ]
  ++ [
    { goblins.shell = mkGoblin (base // { docker.enable = true; }); }
    {
      scopes.work = mkScope { };
      goblins.shell = mkGoblin (
        base
        // {
          scope = "work";
          docker.enable = true;
        }
      );
    }
  ]
  ++
    map
      (options: {
        scopes.work = mkScope options;
        goblins.shell = mkGoblin (base // { scope = "work"; });
      })
      [
        { namespaces.net.enable = false; }
        { namespaces.pid.enable = "yes"; }
        { persistent = "yes"; }
        { storage.cache = "/var/cache"; }
        { storage.cache.path = "relative"; }
        { storage.cache.path = "$OTHER/cache"; }
        { storage.cache.mode = "ro"; }
        { storage."../bad" = { }; }
        { docker.unknown = true; }
        { defaults.args = [ ]; }
        { defaults.allowedDomains = [ ]; }
      ]
  ++ map (options: { goblins.bad = mkGoblin (base // options); }) [
    { binName = "../bash"; }
    { description = ""; }
    { description = "two\nlines"; }
    { description = 1; }
    { allowNix = true; }
    { docker.enable = "yes"; }
    { docker.unknown = true; }
    { docker.defaultScope = "work"; }
    { docker.allowedScopes = [ "work" ]; }
    { scope = "../bad"; }
    {
      allowedScopes = [
        "work"
        "work"
      ];
    }
    { scopeStorage.cache = "relative"; }
    { scopeStorage.cache = "/nix/store/x"; }
    { scopeStorage.cache = "/run/goblins/x"; }
    {
      scopeStorage = {
        a = "/var/cache/x";
        b = "/var/cache/x";
      };
    }
    { allowUnixSockets = false; }
    { allowedDomains = "open"; }
    { allowedDomains = [ "example.com" ]; }
    { allowedHostPorts = [ 80 ]; }
    { publishedPorts = [ 80 ]; }
    { roDirs = "/etc"; }
    { rwFiles = [ 1 ]; }
    { args = "-i"; }
    { env.PATH = "/host/bin"; }
    { env.BAD = 1; }
    { injectedFiles = [ ]; }
    { injectedFiles."../escape" = "bad"; }
    { injectedFiles."/etc/absolute" = "bad"; }
    { injectedFiles."codex/config.toml" = 1; }
    { devShellInjectedFiles = [ ]; }
    { devShellInjectedFiles."../escape" = "bad"; }
    {
      injectedFiles."same.conf" = "a";
      devShellInjectedFiles."same.conf" = "b";
    }
    { allowedChildren = "bad"; }
    {
      allowedChildren = [
        "bad"
        "bad"
      ];
    }
    { allowedChildren = [ "../bad" ]; }
    { allowedChildren = [ 1 ]; }
  ];
  codexProbe = pkgs.writeShellScriptBin "codex" ''
    printf 'CODEX_DIR=%s\n' "$CODEX_HOME"
    printf 'CODEX_ARG=%s\n' "$@"
    export PS1='codex-probe> '
    exec ${pkgs.bashInteractive}/bin/bash --noprofile --norc -i
  '';
  claudeProbe = pkgs.writeShellScriptBin "claude" ''
    printf 'CLAUDE_DIR=%s\n' "$CLAUDE_CONFIG_DIR"
    printf 'CLAUDE_ARG=%s\n' "$@"
    export PS1='claude-probe> '
    exec ${pkgs.bashInteractive}/bin/bash --noprofile --norc -i
  '';
in
{
  # Build without this repository's flake, lock or tests in the source tree.
  production-only =
    let
      source = pkgs.lib.fileset.toSource {
        root = ../.;
        fileset = pkgs.lib.fileset.unions [
          ../nix
          ../skills
          ../Cargo.toml
          ../Cargo.lock
          ../src
          ../crates
        ];
      };
      api = import "${source}/nix/lib.nix" { inherit pkgs sandbox; };
    in
    api.builders.mkGoblins {
      goblins = api.defaultGoblins;
    };
  runtime-no-python =
    let
      minimal = mkGoblins { goblins.shell = mkGoblin (base // { allowedPackages = [ ]; }); };
      closure = pkgs.closureInfo { rootPaths = [ minimal ]; };
    in
    pkgs.runCommand "goblins-runtime-no-python" { nativeBuildInputs = [ pkgs.gnugrep ]; } ''
      if grep -E '/[^/]*-python[0-9.]*-' ${closure}/store-paths; then
        echo 'Unexpected mandatory Python runtime' >&2
        exit 1
      fi
      touch $out
    '';
  named-goblins = configured;
  docker-goblins =
    let
      online = [
        "shell"
        "java"
        "ondemand"
      ];
      docker = mkScope { docker.enable = true; };
      temporary = mkScope {
        persistent = false;
        docker.enable = true;
      };
      # Temporary scopes stand in for per-shell engines: a second, independent
      # engine is a second scope.
      scope = {
        shell = "online";
        java = "online";
        ondemand = "online";
        offline = "offline-a";
        plain = "offline-a";
        scoped = "work";
        extra = "work";
        eager-extra = "work";
        member = "other";
        nodocker = "nodocker";
        unscoped = null;
      };
      allowedScopes = {
        offline = [ "offline-b" ];
        plain = [ "offline-b" ];
        member = [ "work" ];
      };
    in
    mkGoblins {
      scopes = {
        work = docker;
        other = docker;
        online = temporary;
        offline-a = temporary;
        offline-b = temporary;
        nodocker = mkScope { persistent = false; };
      };
      goblins = builtins.mapAttrs (
        name: default:
        mkGoblin {
          pkg = pkgs.bashInteractive;
          binName = "bash";
          args = [
            "--noprofile"
            "--norc"
            "-i"
          ];
          scope = default;
          allowedScopes = allowedScopes.${name} or [ ];
          allowedPackages = [
            pkgs.coreutils
            pkgs.curl
            pkgs.python3
          ]
          ++ pkgs.lib.optional (name == "java") pkgs.jdk
          ++ pkgs.lib.optional (builtins.elem name [
            "extra"
            "eager-extra"
          ]) pkgs.hello;
          docker.enable = builtins.elem name [
            "shell"
            "offline"
            "eager-extra"
          ];
          allowedDomains = if builtins.elem name online then null else [ ];
          env.PS1 = "docker-test> ";
          roDirs = [
            "$GOBLINS_TEST_ROOT/readonly"
          ]
          ++ pkgs.lib.optional (builtins.elem name [
            "extra"
            "eager-extra"
          ]) "$GOBLINS_TEST_ROOT/extra-ro";
          rwDirs = [
            "$GOBLINS_TEST_ROOT/writable"
          ]
          ++ pkgs.lib.optional (builtins.elem name [
            "extra"
            "eager-extra"
          ]) "$GOBLINS_TEST_ROOT/extra-rw";
        }
      ) scope;
    };
  docker-test-image = pkgs.dockerTools.buildImage {
    name = "goblins-test";
    tag = "latest";
    copyToRoot = pkgs.buildEnv {
      name = "docker-test-root";
      paths = [ pkgs.pkgsStatic.busybox ];
      pathsToLink = [ "/bin" ];
    };
    config.Cmd = [ "/bin/sh" ];
  };
  codex-goblins = mkGoblins {
    goblins = {
      codex = mkCodexGoblin {
        pkg = codexProbe;
        filterUnavailableMcp = false;
        args = [ "literal $(false) argument" ];
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
        codexSettings.model = "fixture-model";
        env.GOBLINS_TEST_MARKER = "inherited";
        injectedFiles."goblins-test.conf" = "sandbox-only\n";
      };
      notifier = mkCodexGoblin {
        pkg = pkgs.writeScriptBin "codex" (
          "#!${pkgs.python3}/bin/python3\n" + builtins.readFile ./fake_codex.py
        );
        allowedDomains = [ ];
        allowedPackages = [ ];
      };
      custom = mkCodexGoblin {
        pkg = codexProbe;
        filterUnavailableMcp = false;
        codexConfigDir = "\${HOME}/custom codex";
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
      };
      native = mkCodexGoblin {
        filterUnavailableMcp = true;
        allowedDomains = [ ];
        args = [
          "login"
          "status"
        ];
        allowedPackages = [ ];
      };
      native-ui = mkCodexGoblin {
        allowedDomains = [ ];
        args = [ "--no-alt-screen" ];
        allowedPackages = [ ];
      };
      native-mcp = mkCodexGoblin {
        filterUnavailableMcp = true;
        allowedDomains = [ ];
        args = [
          "mcp"
          "list"
          "--json"
        ];
        allowedPackages = [ ];
      };
    };
  };
  claude-goblins = mkGoblins {
    goblins = {
      claude = mkClaudeGoblin {
        pkg = claudeProbe;
        args = [ "literal $(false) argument" ];
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
        claudeSettings.model = "fixture-model";
        env.GOBLINS_TEST_MARKER = "inherited";
        injectedFiles."goblins-test.conf" = "sandbox-only\n";
      };
      custom = mkClaudeGoblin {
        pkg = claudeProbe;
        claudeConfigDir = "\${HOME}/custom claude";
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
      };
      notifier = mkClaudeGoblin {
        pkg = pkgs.writeScriptBin "claude" (
          "#!${pkgs.python3}/bin/python3\n" + builtins.readFile ./fake_codex.py
        );
        allowedDomains = [ ];
        allowedPackages = [ ];
        env.GOBLINS_TEST_COMPOSER = "  ❯ ";
      };
    };
  };
  scope-goblins =
    let
      member =
        options:
        mkGoblin (
          {
            pkg = pkgs.bashInteractive;
            binName = "bash";
            args = [
              "--noprofile"
              "--norc"
              "-i"
            ];
            allowedPackages = [
              pkgs.coreutils
              pkgs.python3
              pkgs.util-linux
            ];
          }
          // options
          // {
            env = {
              PS1 = "scope-test> ";
            }
            // options.env or { };
          }
        );
      env = {
        SCOPE_MARKER = "from-scope";
        OVERRIDDEN = "from-scope";
      };
    in
    mkGoblins {
      scopes = {
        # The scope's storage path is every member's default mount location.
        work = mkScope {
          storage.cache.path = "/var/cache/probe";
          defaults = { inherit env; };
        };
        # Without a path, defaults (or the goblin) choose the mount location.
        scratch = mkScope {
          persistent = false;
          storage.cache = { };
          defaults = {
            inherit env;
            scopeStorage.cache = "/var/cache/probe";
          };
        };
        private = mkScope {
          persistent = false;
          namespaces.pid.enable = false;
        };
        # Gradle builds run in each member; only the Gradle home, at its
        # default location, is shared.
        builds = mkScope {
          persistent = false;
          storage.gradle.path = "$HOME/.gradle";
          defaults = {
            allowedPackages = [
              pkgs.gradle
              pkgs.jdk
            ];
            env.GRADLE_OPTS = "-Dorg.gradle.daemon.registry.base=/tmp/gradle-daemons";
          };
        };
      };
      goblins = {
        member = member {
          scope = "work";
          allowedScopes = [
            "scratch"
            "private"
          ];
          env.OVERRIDDEN = "from-goblin";
        };
        loner = member { allowedScopes = [ "work" ]; };
        builder = member {
          scope = "builds";
          rwDirs = [ "$GOBLINS_TEST_ROOT/projects" ];
          roDirs = [ "$GOBLINS_TEST_ROOT/repository" ];
        };
      };
    };
  network-goblins = mkGoblins {
    goblins = {
      online = mkGoblin {
        pkg = pkgs.bashInteractive;
        binName = "bash";
        args = [
          "--noprofile"
          "--norc"
          "-i"
        ];
        allowedPackages = [
          pkgs.coreutils
          pkgs.curl
        ];
        env.PS1 = "network-test> ";
      };
      offline = mkGoblin {
        pkg = pkgs.bashInteractive;
        binName = "bash";
        args = [
          "--noprofile"
          "--norc"
          "-i"
        ];
        allowedDomains = [ ];
        allowedPackages = [
          pkgs.coreutils
          pkgs.curl
        ];
        env.PS1 = "network-test> ";
      };
    };
  };
  # Different child configurations (tests/test_ownership.py).
  children-goblins =
    let
      member =
        name: options:
        mkGoblin (
          {
            pkg = pkgs.bashInteractive;
            binName = "bash";
            args = [
              "--noprofile"
              "--norc"
              "-i"
            ];
            description = "${name} child configuration";
            allowedPackages = [ pkgs.coreutils ];
            allowedScopes = [ "work" ];
            env = {
              CHILD_MARKER = name;
              PS1 = "children-test> ";
            };
            injectedFiles."children-test.conf" = "${name}\n";
          }
          // options
        );
      docker =
        options:
        {
          scope = "docker";
          allowedScopes = [ ];
          # One engine cannot serve members different files at one path.
          injectedFiles = { };
        }
        // options;
    in
    mkGoblins {
      scopes = {
        work = mkScope { persistent = false; };
        docker = mkScope {
          persistent = false;
          docker.enable = true;
        };
      };
      goblins = {
        coordinator = member "coordinator" {
          allowedChildren = [
            "coordinator"
            "reviewer"
            "shell"
            "broken"
            "unscoped"
          ];
        };
        reviewer = member "reviewer" {
          allowedChildren = [ ];
          rwDirs = [ "$GOBLINS_TEST_ROOT/reviewer" ];
        };
        shell = member "shell" { allowedChildren = [ "reviewer" ]; };
        unlisted = member "unlisted" { };
        broken = member "broken" { roDirs = [ "$GOBLINS_TEST_ROOT/missing" ]; };
        unscoped = member "unscoped" { allowedScopes = [ ]; };
        # Attached at start; its "plain" children attach through it.
        attached = member "attached" (docker {
          docker.enable = true;
          allowedChildren = [ "plain" ];
        });
        plain = member "plain" (docker { });
        # Not attached; its "eager" children attach through their own option.
        detached = member "detached" (docker {
          allowedChildren = [ "eager" ];
        });
        eager = member "eager" (docker {
          docker.enable = true;
        });
      };
    };
  # A rebuild that removed every configuration but the coordinator.
  children-goblins-updated = mkGoblins {
    goblins.coordinator = mkGoblin (
      base
      // {
        allowedChildren = [ ];
        env.CHILD_MARKER = "updated";
      }
    );
  };
  allowed-children-manifest =
    let
      manifest = mkGoblins {
        scopes.work = mkScope { };
        goblins = {
          claude = mkClaudeGoblin {
            pkg = claudeProbe;
            allowedChildren = [
              "claude"
              "codex"
            ];
          };
          codex = mkCodexGoblin {
            pkg = codexProbe;
            allowedChildren = [ ];
          };
          shell = mkGoblin (base // { allowedScopes = [ "work" ]; });
        };
      };
    in
    pkgs.runCommand "goblins-allowed-children-manifest" { nativeBuildInputs = [ pkgs.jq ]; } ''
      # Resolved on top-level configurations (omitted means only itself) and
      # never carried by scope variants.
      jq -e '.api == 2 and .helper_api == 5
        and .goblins.claude.allowed_children == ["claude", "codex"]
        and .goblins.codex.allowed_children == []
        and .goblins.shell.allowed_children == ["shell"]
        and (.goblins.shell.scopes.work | has("allowed_children") | not)' ${manifest.config}
      touch $out
    '';
  cli-completions =
    pkgs.runCommand "goblins-cli-completions"
      {
        nativeBuildInputs = [
          pkgs.bash
          pkgs.fish
          pkgs.zsh
          pkgs.gnugrep
        ];
      }
      ''
        bash -n ${configured}/share/bash-completion/completions/goblins
        zsh -n ${configured}/share/zsh/site-functions/_goblins
        fish --no-config -c 'source ${configured}/share/fish/vendor_completions.d/goblins.fish; complete -C "goblins run "' > candidates
        grep -q fishy candidates
        grep -q utility candidates
        touch $out
      '';

  updated-goblins =
    let
      changed = mkGoblin (
        base
        // {
          args = [
            "--noprofile"
            "--norc"
            "-i"
          ];
          allowedPackages = [
            pkgs.coreutils
            pkgs.tree
          ];
          env = {
            GOBLIN_MARKER = "updated";
            PS1 = "updated> ";
          };
        }
      );
    in
    mkGoblins {
      goblins = {
        fishy = changed;
        added = changed;
      };
    };
  configured-shell = mkGoblins {
    goblins.shell = mkGoblin {
      pkg = pkgs.fish;
      binName = "fish";
      args = [ "--interactive" ];
      allowedPackages = [
        (pkgs.writeShellScriptBin "goblins-symlink-output-probe" ''
          test -x ${
            pkgs.runCommand "goblins-symlink-output" { } ''
              ln -s ${pkgs.hello}/bin/hello $out
            ''
          }
        '')
      ];
      roDirs = [ "$HOME/.config/fish" ];
    };
  };
  bind-targets = pkgs.runCommand "goblins-bind-targets" { } ''
    mkdir -p $out/functions
    echo 'set -gx GOBLIN_FISH_CONFIG loaded' > $out/config.fish
    echo 'function config_probe; echo config-function-loaded; end' > $out/functions/config_probe.fish
    echo private-sibling > $out/unselected-secret
  '';
  bound-goblins = mkGoblins {
    goblins.bound = mkGoblin (
      base
      // {
        args = [
          "--noprofile"
          "--norc"
          "-i"
        ];
        allowedPackages = [ pkgs.coreutils ];
        env.PS1 = "bind-test> ";
        roDirs = [ "$GOBLINS_TEST_ROOT/readonly" ];
        rwDirs = [ "\${GOBLINS_TEST_ROOT}/writable" ];
        roFiles = [ "$GOBLINS_TEST_ROOT/ro-file" ];
        rwFiles = [ "$GOBLINS_TEST_ROOT/rw-file" ];
      }
    );
  };
  goblins-api =
    assert builtins.all rejected invalid;
    assert (mkGoblin (base // { scope = "work"; })).goblin.scope == "work";
    assert (mkCodexGoblin { allowedScopes = [ "work" ]; }).goblin.allowed_scopes == [ "work" ];
    assert (mkClaudeGoblin { scope = "work"; }).goblin.scope == "work";
    assert (mkGoblin base).goblin.allowed_children == null;
    assert (mkClaudeGoblin { allowedChildren = [ "codex" ]; }).goblin.allowed_children == [ "codex" ];
    assert (mkCodexGoblin { allowedChildren = [ ]; }).goblin.allowed_children == [ ];
    # Not a scope default: a scope variant keeps the goblin's own value.
    assert
      ((mkGoblin (base // { allowedChildren = [ "x" ]; })).withScopeDefaults { }).goblin.allowed_children
      == [ "x" ];
    assert
      let
        merged =
          (mkGoblin (
            base
            // {
              env.A = "goblin";
              rwDirs = [ "/goblin" ];
            }
          )).withScopeDefaults
            {
              env = {
                A = "scope";
                B = "scope";
              };
              rwDirs = [
                "/scope"
                "/goblin"
              ];
              scopeStorage.cache = "/var/cache/x";
            };
      in
      merged.goblin.env == {
        A = "goblin";
        B = "scope";
        TESTCONTAINERS_RYUK_CONTAINER_PRIVILEGED = "false";
      }
      && merged.goblin.scope_storage == { cache = "/var/cache/x"; };
    assert
      (mkScope {
        storage = {
          gradle.path = "$HOME/.gradle";
          m2.path = "$HOME/.m2";
          other = { };
        };
        defaults.scopeStorage.m2 = "/var/cache/m2";
      }).defaults.scopeStorage == {
        gradle = "$HOME/.gradle";
        m2 = "/var/cache/m2";
      };
    assert ((mkClaudeGoblin { }).withScopeDefaults { env.FROM_SCOPE = "1"; }).goblin.env ? FROM_SCOPE;
    assert !(mkGoblin base).goblin.docker.enabled;
    assert (mkGoblin (base // { docker.enable = true; })).goblin.docker.enabled;
    assert (mkGoblin base).goblin.network;
    assert !(mkGoblin (base // { allowedDomains = [ ]; })).goblin.network;
    assert builtins.all
      (options: !(builtins.tryEval (mkCodexGoblin options).goblin.build_spec.drvPath).success)
      [
        { codexConfigDir = null; }
        { codexConfigDir = "relative"; }
        { env.CODEX_HOME = "/different"; }
        { injectedFiles."codex/config.toml" = "override"; }
        { injectedFiles."codex/skills/goblins-spawn/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-messaging/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-packages/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-docker/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-devshell/SKILL.md" = "override"; }
        { devShellInjectedFiles."codex/skills/goblins-devshell/SKILL.md" = "override"; }
      ];
    assert builtins.all
      (options: !(builtins.tryEval (mkClaudeGoblin options).goblin.build_spec.drvPath).success)
      [
        { claudeConfigDir = null; }
        { claudeConfigDir = "relative"; }
        { env.CLAUDE_CONFIG_DIR = "/different"; }
        { injectedFiles."claude-code/managed-settings.json" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-spawn/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-messaging/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-packages/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-docker/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-devshell/SKILL.md" = "override"; }
        { devShellInjectedFiles."claude-code/.claude/skills/goblins-devshell/SKILL.md" = "override"; }
      ];
    # The dev shell skill is mounted only in sandboxes launched with a dev shell.
    assert
      let
        codex = (mkCodexGoblin { }).goblin;
        claude = (mkClaudeGoblin { }).goblin;
      in
      !(codex.sandbox_etc ? "codex/skills/goblins-devshell/SKILL.md")
      && codex.dev_shell_sandbox_etc ? "codex/skills/goblins-devshell/SKILL.md"
      && !(claude.sandbox_etc ? "claude-code/.claude/skills/goblins-devshell/SKILL.md")
      && claude.dev_shell_sandbox_etc ? "claude-code/.claude/skills/goblins-devshell/SKILL.md";
    assert
      (mkGoblin (base // { devShellInjectedFiles."x.conf" = "x"; })).goblin.dev_shell_sandbox_etc
        ? "x.conf";
    pkgs.runCommand "goblins-api-evaluation" { } "touch $out";
}
