{
  pkgs,
  goblinsLib,
  sandbox,
  agents,
  # The demo goblins the Python tests use.
  demo,
  devShell,
  # The dev-goblins package: the demo goblins with a Docker shell.
  devGoblins,
}:
let
  inherit (goblinsLib.builders)
    mkGoblin
    mkClaudeGoblin
    mkGoblins
    mkScope
    ;
  mkCodexGoblin = options: goblinsLib.builders.mkCodexGoblin ({ pkg = agents.codex; } // options);
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
        allowedRunTargets = [
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
          allowedRunTargets = [ ];
        }
      );
    };
  };
  rejected = configuration: !(builtins.tryEval (mkGoblins configuration).config.drvPath).success;
  invalid = [
    { goblins = { }; }
    {
      editor.vim.enable = true;
      goblins.shell = mkGoblin base;
    }
    {
      editor.emacs.enable = "yes";
      goblins.shell = mkGoblin base;
    }
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
    {
      scopes.work = mkScope { docker.enable = true; };
      goblins = {
        plain = mkGoblin (base // { scope = "work"; });
        nix = mkGoblin (
          base
          // {
            scope = "work";
            allowNix = true;
          }
        );
      };
    }
  ]
  ++ [
    { goblins.shell = mkGoblin (base // { allowedRunTargets = [ "missing" ]; }); }
    # Over the bound: 65 declared goblins, or a name longer than 64 characters.
    (
      let
        names = map (i: "g${toString i}") (pkgs.lib.range 1 65);
      in
      {
        goblins = pkgs.lib.genAttrs names (_: mkGoblin (base // { allowedRunTargets = names; }));
      }
    )
    {
      goblins = {
        shell = mkGoblin (base // { allowedRunTargets = [ (pkgs.lib.fixedWidthString 65 "x" "") ]; });
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
    { allowNix = "yes"; }
    {
      allowNix = true;
      allowedDomains = [ ];
    }
    { docker.enable = "yes"; }
    { hostCommands.enable = "yes"; }
    { hostCommands.unknown = true; }
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
    { allowedRunTargets = "bad"; }
    { allowedRunTargets = null; }
    { bypassPermissions = "yes"; }
    # A plain goblin has no agent prompts to bypass.
    { bypassPermissions = true; }
    {
      allowedRunTargets = [
        "bad"
        "bad"
      ];
    }
    { allowedRunTargets = [ "../bad" ]; }
    { allowedRunTargets = [ 1 ]; }
    { resumeCommand = "--resume {session_id}"; }
    { resumeCommand = [ 1 ]; }
    { resumeCommand = [ "{Session}" ]; }
    { resumeCommand = [ "{unclosed" ]; }
    { resumeCommand = [ "lone}" ]; }
    { resumeCommand = [ "{session-id}" ]; }
    { resumeCommand = [ "{}" ]; }
    { resumeCommand = [ "{${pkgs.lib.fixedWidthString 33 "x" ""}}" ]; }
    { resumeCommand = [ "{attach.pid}" ]; }
    { onAttach = [ "client" ]; }
    { onAttach.command = [ ]; }
    { onAttach.command = [ "{attach.other}" ]; }
    { onAttach.command = [ "{Bad}" ]; }
    {
      onAttach = {
        command = [ "client" ];
        timeout = 0;
      };
    }
    {
      onAttach = {
        command = [ "client" ];
        waitFor = [ "attach.pid" ];
      };
    }
    {
      onAttach = {
        command = [ "client" ];
        unknown = true;
      };
    }
    { onDetach = [ "stop" ]; }
    {
      onAttach.command = [ "client" ];
      onDetach = [ ];
    }
  ];
  codexProbe = pkgs.writeShellScriptBin "codex" ''
    if [[ "$1" == --version || "$1" == app-server ]]; then
      exec ${agents.codex}/bin/codex "$@"
    fi
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
      goblins = import "${source}/nix/demo-goblins" {
        inherit pkgs agents;
        inherit (api) builders commonTools;
      };
      scopes = import "${source}/nix/demo-goblins/scopes.nix" { inherit (api.builders) mkScope; };
    };
  demo-goblins = demo;
  # Ordinary API/terminal tests need Fish semantics, not the demo's agent and
  # developer tool closures. Keep the complete demo above for packaging and
  # connected-shell coverage. Exec tests only need Bash and core utilities.
  shell-goblins = mkGoblins {
    goblins.shell = mkGoblin {
      pkg = pkgs.fish;
      binName = "fish";
      args = [
        "--no-config"
        "--interactive"
      ];
      allowedPackages = [
        pkgs.coreutils
        pkgs.gnugrep
        pkgs.gnused
      ];
      allowedDomains = [ ];
      allowedRunTargets = [ "shell" ];
    };
  };
  exec-goblins = mkGoblins {
    goblins.shell = mkGoblin {
      pkg = pkgs.bashInteractive;
      binName = "bash";
      args = [
        "--noprofile"
        "--norc"
        "-i"
      ];
      allowedPackages = [ pkgs.coreutils ];
      allowedDomains = [ ];
    };
  };
  # This output references jq without listing it in the sandbox's closure.
  # GC coverage of an output must not grant visibility to every reference.
  root-reference-fixture = pkgs.runCommand "goblins-root-reference-fixture" { } ''
    mkdir $out
    printf '%s\n' ${pkgs.coreutils} > $out/paths
    printf '%s\n' ${pkgs.jq} > $out/unlisted
  '';
  # Every demo goblin may start the agents, also the dev package's shell in
  # its dev scope, and no goblin may start goblins-dev.
  demo-run-targets =
    pkgs.runCommand "goblins-demo-run-targets" { nativeBuildInputs = [ pkgs.jq ]; }
      ''
        jq -e '.goblins as $g
          | ($g | keys) == ["claude", "codex", "goblins-dev", "shell"]
          and all($g[]; .allowed_run_targets == ["claude", "codex"])
          and $g.shell.scope == "dev"
          and ($g.claude.scopes | has("dev")) and ($g.codex.scopes | has("dev"))
          and ($g.claude.host_commands | not) and ($g.codex.host_commands | not)
          and $g["goblins-dev"].host_commands' ${devGoblins.config}
        touch $out
      '';
  # The launchers ask the agent to skip its own prompts only with
  # bypassPermissions; Codex keeps full access either way (Goblins is the
  # sandbox) and gets "never" only when bypassing.
  agent-permissions =
    let
      claude = import ../nix/goblins/claude { inherit pkgs; };
      codex = import ../nix/goblins/codex { inherit pkgs; };
      launchers = bypassPermissions: {
        claude = claude.mkLauncher {
          pkg = agents.claude-code;
          binName = "claude";
          claudeConfigDir = "$HOME/.claude";
          inherit (claude) plugin;
          inherit bypassPermissions;
        };
        codex = codex.mkLauncher {
          pkg = agents.codex;
          binName = "codex";
          codexConfigDir = "$HOME/.codex";
          filterUnavailableMcp = false;
          inherit bypassPermissions;
        };
      };
      own = launchers false;
      bypass = launchers true;
    in
    pkgs.runCommand "goblins-agent-permissions" { } ''
      # `! grep` would not stop the build under set -e.
      if grep -q -- --dangerously-skip-permissions ${own.claude}/bin/claude; then exit 1; fi
      grep -q -- '--plugin-dir [^ ]* --dangerously-skip-permissions "$@"' ${bypass.claude}/bin/claude
      for launcher in ${own.codex} ${bypass.codex}; do
        # `codex` on PATH runs its own engine with full access.
        grep -Fq -- 'exec "$codex_executable" --sandbox danger-full-access' $launcher/bin/codex
        grep -q -- "-c 'sandbox_mode=\"danger-full-access\"'" $launcher/bin/goblins-codex-server
        # The terminal joins the conversation as it is; the adapter set its policy.
        if grep -Eq -- '--sandbox|--ask-for-approval' $launcher/bin/goblins-codex-attach; then exit 1; fi
      done
      if grep -q -e 'approval_policy=' -e '--ask-for-approval never' -e --bypass-permissions ${own.codex}/bin/codex ${own.codex}/bin/goblins-codex-server; then exit 1; fi
      grep -Fq -- 'exec "$codex_executable" --sandbox danger-full-access --ask-for-approval never --enable hooks' ${bypass.codex}/bin/codex
      grep -q -- "-c 'approval_policy=\"never\"'" ${bypass.codex}/bin/goblins-codex-server
      grep -Fq -- '"''${thread[@]}" --bypass-permissions' ${bypass.codex}/bin/goblins-codex-server
      touch $out
    '';
  # The Claude mod as Claude Code loads it: its manifest and hooks module
  # validate, and its tests (kept out of the plugin) pass against the engine.
  claude-mod =
    let
      inherit (import ../nix/goblins/claude { inherit pkgs; }) plugin;
    in
    pkgs.runCommand "goblins-claude-mod"
      {
        nativeBuildInputs = [ agents.claude-code ];
      }
      ''
        export HOME=$TMPDIR/home DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1
        mkdir -p $HOME
        cp -r ${plugin} $TMPDIR/plugin
        chmod -R u+w $TMPDIR/plugin
        cp ${plugin.source}/hooks/*.test.tsx $TMPDIR/plugin/hooks/
        test -x $TMPDIR/plugin/bin/goblins-claude-message
        claude plugin validate $TMPDIR/plugin > validate.log 2>&1 || { cat validate.log; exit 1; }
        grep -F 'hooks: session.start' validate.log
        claude plugin test $TMPDIR/plugin 2>&1 | tee test.log
        grep -Eq '^ *[1-9][0-9]* pass$' test.log
        grep -Eq '^ *0 fail$' test.log
        touch $out
      '';
  # Each feature set gets its own skills, and none of them presents a
  # workflow the sandbox doesn't have (tests/test_*_config.py check the mounts).
  skill-selection =
    let
      inherit (pkgs) lib;
      variants = {
        claude = mkClaudeGoblin { };
        claude-nix = mkClaudeGoblin { allowNix = true; };
        codex = mkCodexGoblin { };
        codex-nix = mkCodexGoblin { allowNix = true; };
        claude-host = mkClaudeGoblin {
          allowNix = true;
          hostCommands.enable = true;
        };
        codex-host = mkCodexGoblin { hostCommands.enable = true; };
      };
      directory =
        name: if lib.hasPrefix "claude" name then "claude-code/.claude/skills" else "codex/skills";
      skills =
        name: files:
        lib.sort (a: b: a < b) (
          lib.concatMap (
            file:
            let
              m = builtins.match "${directory name}/([^/]+)/SKILL.md" file;
            in
            lib.optional (m != null) (builtins.head m)
          ) (builtins.attrNames files)
        );
      always = name: skills name variants.${name}.goblin.sandbox_etc;
      devShell = name: skills name variants.${name}.goblin.dev_shell_sandbox_etc;
      expected = {
        claude = [
          "goblins-docker"
          "goblins-messaging"
          "goblins-packages"
          "goblins-raids"
          "goblins-spawn"
        ];
        claude-nix = [
          "goblins-docker"
          "goblins-messaging"
          "goblins-nix"
          "goblins-raids"
          "goblins-spawn"
        ];
      };
      settings = name: variants.${name}.goblin.sandbox_etc."claude-code/managed-settings.json";
      installed =
        name:
        lib.concatStringsSep " " (
          lib.mapAttrsToList (_: file: "${file}") (
            lib.filterAttrs (file: _: lib.hasSuffix "SKILL.md" file) variants.${name}.goblin.sandbox_etc
          )
        );
    in
    assert always "claude" == expected.claude && always "codex" == expected.claude;
    assert always "claude-nix" == expected.claude-nix && always "codex-nix" == expected.claude-nix;
    assert devShell "claude" == [ "goblins-devshell" ] && devShell "codex" == [ "goblins-devshell" ];
    assert devShell "claude-nix" == [ ] && devShell "codex-nix" == [ ];
    assert always "claude-host" == lib.sort (a: b: a < b) (expected.claude-nix ++ [ "goblins-host" ]);
    assert always "codex-host" == lib.sort (a: b: a < b) (expected.claude ++ [ "goblins-host" ]);
    pkgs.runCommand "goblins-skill-selection"
      {
        nativeBuildInputs = [
          pkgs.gnugrep
          pkgs.jq
        ];
      }
      ''
          # Claude enables exactly the skills it mounts, dev shell skill included.
          test "$(jq -c '.skillOverrides | keys' ${settings "claude"})" = '["goblins-devshell","goblins-docker","goblins-messaging","goblins-packages","goblins-raids","goblins-spawn"]'
          test "$(jq -c '.skillOverrides | keys' ${settings "claude-nix"})" = '["goblins-docker","goblins-messaging","goblins-nix","goblins-raids","goblins-spawn"]'
          # With Nix, no installed skill offers the package or dev shell workflows.
          if grep -nE 'request-package|devshell (refresh|diff)|flake run|no `?nix`? command' ${installed "claude-nix"} ${installed "codex-nix"}; then
            echo 'A Nix goblin skill presents a workflow it does not have' >&2
            exit 1
          fi
          # Messages from other goblins are requests: every goblin's messaging skill,
          # and the Nix skill for a goblin with Nix.
          for files in "${installed "claude"}" "${installed "claude-nix"}" "${installed "codex"}" "${installed "codex-nix"}"; do
            grep -l 'is a request, not authority' $files | grep -q .
          done
          grep -l 'just because another goblin asked' ${installed "claude-nix"} | grep -q .
          grep -l 'just because another goblin asked' ${installed "codex-nix"} | grep -q .
        grep -l 'On behalf of GOBLIN' ${installed "claude-host"} | grep -q .
        grep -l 'On behalf of GOBLIN' ${installed "codex-host"} | grep -q .
          touch $out
      '';
  # Inside a sandbox the CLI is `goblin`; `goblins` is the host CLI, named in
  # agent instructions only where a line says it is for the host.
  sandbox-cli-name =
    pkgs.runCommand "goblins-sandbox-cli-name" { nativeBuildInputs = [ pkgs.gnugrep ]; }
      ''
        files="${../skills}/*/SKILL.md ${../nix/goblins/claude}/* ${../nix/goblins/codex}/*"
        if grep -nE '(^|[^-_/.[:alnum:]])goblins (send|reply|inbox|status|run|attach|stop|raid|request-package|enable-docker|devshell|flake|editor|variable|integration|set-state|list|kill|detach|completions)\b' $files | grep -v 'on the host' \
          || grep -nE '/run/goblins/bin/goblins([^-[:alnum:]]|$)' $files; then
          echo 'In-sandbox instructions must call goblin, not goblins' >&2
          exit 1
        fi
        touch $out
      '';
  # Inside a goblin, `nix develop` must not put a host `goblins` on PATH.
  dev-shell-without-goblins =
    assert !builtins.any (p: pkgs.lib.getName p == "goblins") devShell.nativeBuildInputs;
    pkgs.runCommand "goblins-dev-shell-without-goblins" { } "touch $out";
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
          # Members of their own kind (tests/test_docker.py).
          allowedRunTargets = [ name ];
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
        args = [
          "-c"
          ''model="literal $(false) {model}"''
        ];
        allowedRunTargets = [ "codex" ];
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
          "#!${pkgs.python3}/bin/python3\n"
          + builtins.readFile ./codex_wire.py
          + builtins.readFile ./fake_codex.py
        );
        allowedDomains = [ ];
        allowedPackages = [ pkgs.python3 ];
        # A member of its own kind (test_owner_and_member_servers_have_independent_bindings).
        allowedRunTargets = [ "notifier" ];
      };
      profile = mkCodexGoblin {
        pkg = pkgs.writeScriptBin "codex" (
          "#!${pkgs.python3}/bin/python3\n"
          + builtins.readFile ./codex_wire.py
          + builtins.readFile ./fake_codex.py
        );
        allowedDomains = [ ];
        allowedPackages = [ pkgs.python3 ];
        args = [
          "--profile"
          "fixture"
          "--config"
          ''model_provider="literal $(false) provider"''
          "--enable"
          "plugins"
        ];
      };
      # Nix workflows instead of package grants (test_*_config.py).
      nix = mkCodexGoblin {
        pkg = codexProbe;
        filterUnavailableMcp = false;
        allowNix = true;
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
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
        # The scripted model for `codex exec` (test_codex_config.py).
        allowedPackages = [ pkgs.python3 ];
      };
    };
  };
  claude-goblins = mkGoblins {
    goblins = {
      claude = mkClaudeGoblin {
        pkg = claudeProbe;
        args = [ "literal $(false) argument" ];
        allowedRunTargets = [ "claude" ];
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
        claudeSettings.model = "fixture-model";
        env.GOBLINS_TEST_MARKER = "inherited";
        injectedFiles."goblins-test.conf" = "sandbox-only\n";
      };
      # Nix workflows instead of package grants (test_*_config.py).
      nix = mkClaudeGoblin {
        pkg = claudeProbe;
        allowNix = true;
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
      };
      custom = mkClaudeGoblin {
        pkg = claudeProbe;
        claudeConfigDir = "\${HOME}/custom claude";
        bypassPermissions = true;
        allowedPackages = [
          pkgs.coreutils
          pkgs.jq
        ];
      };
      notifier = mkClaudeGoblin {
        pkg = pkgs.writeScriptBin "claude" (
          "#!${pkgs.python3}/bin/python3\n" + builtins.readFile ./fake_claude.py
        );
        allowedDomains = [ ];
        allowedPackages = [ ];
        env.GOBLINS_TEST_COMPOSER = "  ❯ ";
      };
      # Host command notices (tests/test_operation_notices.py).
      host-notifier = mkClaudeGoblin {
        pkg = pkgs.writeScriptBin "claude" (
          "#!${pkgs.python3}/bin/python3\n" + builtins.readFile ./fake_claude.py
        );
        allowedDomains = [ ];
        allowedPackages = [ ];
        env.GOBLINS_TEST_COMPOSER = "  ❯ ";
        hostCommands.enable = true;
        rwDirs = [ "$HOME/shared" ];
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
          allowedRunTargets = [ "member" ];
        };
        loner = member {
          allowedScopes = [ "work" ];
          allowedRunTargets = [ "member" ];
        };
        builder = member {
          scope = "builds";
          rwDirs = [ "$GOBLINS_TEST_ROOT/projects" ];
          roDirs = [ "$GOBLINS_TEST_ROOT/repository" ];
        };
      };
    };
  # editor.emacs.enable (tests/test_editor.py): the TRAMP tools and EDITOR
  # reach every goblin, also scope variants; a goblin's own env wins.
  editor-goblins = mkGoblins {
    editor.emacs.enable = true;
    scopes.work = mkScope { persistent = false; };
    goblins = {
      plain = mkGoblin {
        pkg = pkgs.bashInteractive;
        binName = "bash";
        args = [
          "--noprofile"
          "--norc"
          "-i"
        ];
        allowedPackages = [ pkgs.hello ];
        env = {
          PS1 = "editor-test> ";
          VISUAL = "custom-visual";
        };
        allowedScopes = [ "work" ];
        allowedRunTargets = [ "plain" ];
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
        allowedRunTargets = [ "online" ];
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
  # Host commands, approved and run outside the sandbox
  # (tests/test_host_commands.py).
  host-goblins =
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
            allowedPackages = [ pkgs.coreutils ];
            env.PS1 = "host-test> ";
          }
          // options
        );
    in
    mkGoblins {
      goblins = {
        host = member {
          hostCommands.enable = true;
          allowedRunTargets = [
            "host"
            "plain"
          ];
        };
        plain = member { };
      };
    };
  # allowNix against the host's Nix daemon (tests/test_nix.py).
  nix-goblins =
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
            allowedPackages = [ pkgs.coreutils ];
          }
          // options
        );
    in
    mkGoblins {
      goblins = {
        nix = member {
          allowNix = true;
          allowedRunTargets = [
            "nix"
            "plain"
          ];
          env = {
            PS1 = "nix-test> ";
            # Evaluated and built inside the sandbox; already in the host store.
            NIXPKGS = "${pkgs.path}";
          };
        };
        plain = member { env.PS1 = "nix-test> "; };
      };
    };
  # Raid members with other configurations (tests/test_run_targets.py).
  run-target-goblins =
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
            description = "${name} run target";
            allowedPackages = [ pkgs.coreutils ];
            allowedScopes = [ "work" ];
            env = {
              TARGET_MARKER = name;
              PS1 = "run-target-test> ";
            };
            injectedFiles."run-target-test.conf" = "${name}\n";
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
          allowedRunTargets = [
            "coordinator"
            "reviewer"
            "shell"
            "broken"
            "unscoped"
          ];
        };
        reviewer = member "reviewer" {
          allowedRunTargets = [ ];
          rwDirs = [ "$GOBLINS_TEST_ROOT/reviewer" ];
        };
        shell = member "shell" { allowedRunTargets = [ "reviewer" ]; };
        unlisted = member "unlisted" { };
        broken = member "broken" { roDirs = [ "$GOBLINS_TEST_ROOT/missing" ]; };
        unscoped = member "unscoped" { allowedScopes = [ ]; };
        # Attached at start; the "plain" members it runs attach through it.
        attached = member "attached" (docker {
          docker.enable = true;
          allowedRunTargets = [ "plain" ];
        });
        plain = member "plain" (docker { });
        # Not attached; the "eager" members it runs attach through their own option.
        detached = member "detached" (docker {
          allowedRunTargets = [ "eager" ];
        });
        eager = member "eager" (docker {
          docker.enable = true;
        });
      };
    };
  # A rebuild that removed every configuration but the coordinator.
  run-target-goblins-updated = mkGoblins {
    goblins.coordinator = mkGoblin (
      base
      // {
        allowedRunTargets = [ ];
        env.TARGET_MARKER = "updated";
      }
    );
  };
  run-targets-manifest =
    let
      manifest = mkGoblins {
        scopes.work = mkScope { };
        goblins = {
          claude = mkClaudeGoblin {
            pkg = claudeProbe;
            allowedRunTargets = [
              "claude"
              "codex"
            ];
          };
          codex = mkCodexGoblin {
            pkg = codexProbe;
            allowedRunTargets = [ ];
          };
          shell = mkGoblin (base // { allowedScopes = [ "work" ]; });
        };
      };
    in
    pkgs.runCommand "goblins-run-targets-manifest" { nativeBuildInputs = [ pkgs.jq ]; } ''
      # Resolved on top-level configurations (omitted means none) and never
      # carried by scope variants.
      jq -e '.api == 2 and .helper_api == 6
        and .goblins.claude.allowed_run_targets == ["claude", "codex"]
        and .goblins.codex.allowed_run_targets == []
        and .goblins.shell.allowed_run_targets == []
        and (.goblins.shell.scopes.work | has("allowed_run_targets") | not)' ${manifest.config}
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
          pkgs.diffutils
        ];
      }
      ''
        # Keep installation verification outside the incremental Cargo loop.
        # Check exact installed contents for all three shells, not just syntax.
        for entry in \
          'bash share/bash-completion/completions/goblins' \
          'fish share/fish/vendor_completions.d/goblins.fish' \
          'zsh share/zsh/site-functions/_goblins'; do
          read -r shell relative <<< "$entry"
          ${configured}/bin/goblins completions "$shell" > generated
          cmp generated "${configured}/$relative"
        done
        bash -n ${configured}/share/bash-completion/completions/goblins
        zsh -n ${configured}/share/zsh/site-functions/_goblins
        fish --no-config -c 'source ${configured}/share/fish/vendor_completions.d/goblins.fish; complete -C "goblins run "' > candidates
        grep -q fishy candidates
        grep -q utility candidates
        # Options that take a value, such as --raid, precede the configuration.
        fish --no-config -c 'source ${configured}/share/fish/vendor_completions.d/goblins.fish; complete -C "goblins run --raid crew fi"' > after-raid
        grep -q fishy after-raid
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
    assert (mkGoblin base).goblin.allowed_run_targets == [ ];
    assert !(mkGoblin base).goblin.bypass_permissions;
    assert !(mkClaudeGoblin { }).goblin.bypass_permissions;
    assert (mkCodexGoblin { bypassPermissions = true; }).goblin.bypass_permissions;
    assert (mkGoblin base).goblin.resume_command == null;
    assert (mkGoblin base).goblin.on_attach == null;
    assert
      (
        let
          goblin =
            (mkGoblin (
              base
              // {
                onAttach.command = [
                  "client"
                  "{session_id}"
                ];
                onDetach = [
                  "stop"
                  "{attach.pid}"
                ];
              }
            )).goblin;
        in
        {
          inherit (goblin) on_attach on_detach;
        }
      ) == {
        on_attach = {
          command = [
            "client"
            "{session_id}"
          ];
          timeout = 30;
          wait_for = [ ];
        };
        on_detach = [
          "stop"
          "{attach.pid}"
        ];
      };
    assert
      (mkGoblin (
        base
        // {
          resumeCommand = [
            "--resume"
            "{session_id}"
            "{{literal}}"
          ];
        }
      )).goblin.resume_command == [
        "--resume"
        "{session_id}"
        "{{literal}}"
      ];
    # Braces in args are literal in the default resume command, not placeholders.
    assert
      (mkCodexGoblin {
        args = [
          "-c"
          "mcp_servers.x={command=\"y\"}"
        ];
      }).goblin.resume_command == [
        "-c"
        "mcp_servers.x={{command=\"y\"}}"
        "--thread"
        "{session_id}"
      ];
    # A Codex goblin's args are App Server options, and it starts a client at
    # each attach on the conversation its adapter recorded.
    assert
      builtins.all
        (args: !(builtins.tryEval (mkCodexGoblin { inherit args; }).goblin.build_spec.drvPath).success)
        [
          [ "prompt" ]
          [ "resume" ]
          [ "-c" ]
          [ "--sandbox" "read-only" ]
          [ "--ask-for-approval" "never" ]
          [ "--no-alt-screen" ]
        ];
    assert
      (mkCodexGoblin {
        args = [
          "-p"
          "work"
          "--enable=plugins"
          "-cmodel=\"x\""
        ];
      }).goblin.args == [
        "-p"
        "work"
        "--enable=plugins"
        "-cmodel=\"x\""
      ];
    assert
      (
        let
          command = (mkCodexGoblin { }).goblin.on_attach.command;
        in
        pkgs.lib.hasSuffix "/bin/goblins-codex-attach" (builtins.head command)
        && builtins.tail command == [ "{session_id}" ]
      );
    assert
      (mkClaudeGoblin {
        args = [
          "--mcp-config"
          "{\"mcpServers\":{}}"
        ];
      }).goblin.resume_command == [
        "--mcp-config"
        "{{\"mcpServers\":{{}}}}"
        "--resume"
        "{session_id}"
      ];
    assert
      (mkClaudeGoblin { allowedRunTargets = [ "codex" ]; }).goblin.allowed_run_targets == [ "codex" ];
    assert (mkCodexGoblin { allowedRunTargets = [ ]; }).goblin.allowed_run_targets == [ ];
    # Not a scope default: a scope variant keeps the goblin's own value.
    assert
      ((mkGoblin (base // { allowedRunTargets = [ "x" ]; })).withScopeDefaults { })
      .goblin.allowed_run_targets == [ "x" ];
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
    assert (mkGoblin (base // { allowNix = true; })).goblin.nix;
    # Without Docker, a scope may mix goblins with and without Nix.
    assert
      !rejected {
        scopes.work = mkScope { };
        goblins = {
          plain = mkGoblin (base // { scope = "work"; });
          nix = mkGoblin (
            base
            // {
              scope = "work";
              allowNix = true;
            }
          );
        };
      };
    assert !(mkGoblin base).goblin.nix;
    assert !((mkGoblin base).goblin.env ? NIX_CONFIG);
    assert pkgs.lib.hasInfix "flakes" (mkGoblin (base // { allowNix = true; })).goblin.env.NIX_CONFIG;
    assert
      (mkGoblin (
        base
        // {
          allowNix = true;
          env.NIX_CONFIG = "own";
        }
      )).goblin.env.NIX_CONFIG == "own";
    assert !(mkGoblin base).goblin.docker.enabled;
    assert (mkGoblin (base // { docker.enable = true; })).goblin.docker.enabled;
    assert (mkGoblin base).goblin.network;
    assert !(mkGoblin (base // { allowedDomains = [ ]; })).goblin.network;
    assert builtins.all
      (options: !(builtins.tryEval (mkCodexGoblin options).goblin.build_spec.drvPath).success)
      [
        { codexConfigDir = null; }
        { bypassPermissions = "yes"; }
        { codexConfigDir = "relative"; }
        { env.CODEX_HOME = "/different"; }
        { injectedFiles."codex/config.toml" = "override"; }
        { injectedFiles."codex/skills/goblins-spawn/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-messaging/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-packages/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-docker/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-raids/SKILL.md" = "override"; }
        { injectedFiles."codex/skills/goblins-devshell/SKILL.md" = "override"; }
        { devShellInjectedFiles."codex/skills/goblins-devshell/SKILL.md" = "override"; }
      ];
    assert builtins.all
      (options: !(builtins.tryEval (mkClaudeGoblin options).goblin.build_spec.drvPath).success)
      [
        { claudeConfigDir = null; }
        { bypassPermissions = "yes"; }
        { claudeConfigDir = "relative"; }
        { env.CLAUDE_CONFIG_DIR = "/different"; }
        { injectedFiles."claude-code/managed-settings.json" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-spawn/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-messaging/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-packages/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-docker/SKILL.md" = "override"; }
        { injectedFiles."claude-code/.claude/skills/goblins-raids/SKILL.md" = "override"; }
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
