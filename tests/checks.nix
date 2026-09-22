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
        }
      );
    };
  };
  rejected = configuration: !(builtins.tryEval (mkGoblins configuration).config.drvPath).success;
  invalid = [
    { goblins = { }; }
    { goblins.bad = pkgs.hello; }
    { goblins."../bad" = mkGoblin base; }
  ]
  ++ map (options: { goblins.bad = mkGoblin (base // options); }) [
    { binName = "../bash"; }
    { description = ""; }
    { description = "two\nlines"; }
    { description = 1; }
    { allowNix = true; }
    { docker.enable = "yes"; }
    { docker.unknown = true; }
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
  docker-goblins = mkGoblins {
    goblins = builtins.listToAttrs (
      map
        (name: {
          inherit name;
          value = mkGoblin {
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
              pkgs.python3
            ]
            ++ pkgs.lib.optional (name == "java") pkgs.jdk;
            docker.enable = builtins.elem name [
              "shell"
              "offline"
            ];
            allowedDomains =
              if
                builtins.elem name [
                  "shell"
                  "java"
                  "ondemand"
                ]
              then
                null
              else
                [ ];
            env.PS1 = "docker-test> ";
            roDirs = [ "$GOBLINS_TEST_ROOT/readonly" ];
            rwDirs = [ "$GOBLINS_TEST_ROOT/writable" ];
          };
        })
        [
          "shell"
          "offline"
          "plain"
          "java"
          "ondemand"
        ]
    );
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
      ];
    pkgs.runCommand "goblins-api-evaluation" { } "touch $out";
}
