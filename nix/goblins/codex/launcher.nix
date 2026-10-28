{ pkgs, runtimeDirectory }:
{
  pkg,
  binName,
  bypassPermissions,
  codexConfigDir,
  filterUnavailableMcp,
}:
let
  inherit (pkgs) lib;
  directory = runtimeDirectory codexConfigDir;
  # Codex's own command sandbox cannot run inside a goblin (no namespaces),
  # so Goblins is the sandbox either way. Only the approval policy follows
  # bypassPermissions; otherwise Codex's own configuration chooses it.
  approval = lib.optionalString bypassPermissions " --ask-for-approval never";
  setup = ''
    export CODEX_HOME=${directory}
    codex_executable=${lib.escapeShellArg "${pkg}/bin/${binName}"}
  '';
  mcp = ''
    mcp_overrides=()
    ${lib.optionalString filterUnavailableMcp (
      lib.replaceStrings [ "\${coreutils}" "\${jq}" ] [ "${pkgs.coreutils}" "${pkgs.jq}" ] (
        builtins.readFile ./mcp.sh
      )
    )}
  '';
  # /tmp is private to this sandbox, also from the goblins it shares a
  # workspace with. Keep the server socket outside the shared Codex configuration.
  runtime = "/tmp/goblins-codex";
  socket = "${runtime}/app-server.sock";
  # The goblin's main command: the App Server and the adapter on its own
  # connection, for the goblin's whole life. Its arguments are server options
  # only, and --thread from resumeCommand.
  server = pkgs.writeShellScriptBin "goblins-codex-server" ''
    set -e
    ${setup}
    version=$("$codex_executable" --version)
    version=''${version##* }
    if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ $(printf '%s\n' 0.163.0 "$version" | ${pkgs.coreutils}/bin/sort -V | ${pkgs.coreutils}/bin/head -n1) != 0.163.0 ]]; then
      printf 'Goblins requires Codex 0.163.0 or newer (found %s).\n' "$version" >&2
      exit 1
    fi
    ${mcp}
    # Native profile and -c configuration apply to the server, which creates
    # the threads. Preserve argument boundaries and literal shell text; never
    # eval configuration.
    server_overrides=()
    profile=
    thread=()
    launch_args=("$@")
    for ((i=0; i<''${#launch_args[@]}; i++)); do
      case "''${launch_args[i]}" in
        -c|--config|--enable|--disable|-p|--profile|--thread)
          option=''${launch_args[i]}
          if ((i+1 >= ''${#launch_args[@]})); then
            printf 'Goblins: %s needs a value.\n' "$option" >&2
            exit 2
          fi
          ((i+=1))
          case "$option" in
            -p|--profile) profile=''${launch_args[i]} ;;
            --thread) thread=(--thread "''${launch_args[i]}") ;;
            *) server_overrides+=("$option" "''${launch_args[i]}") ;;
          esac ;;
        --config=*|--enable=*|--disable=*|-c?*) server_overrides+=("''${launch_args[i]}") ;;
        --profile=*) profile=''${launch_args[i]#--profile=} ;;
        -p?*) profile=''${launch_args[i]#-p} ;;
        *)
          printf 'Goblins: %s is not a Codex server option (-c, --enable, --disable, -p).\n' "''${launch_args[i]}" >&2
          exit 2 ;;
      esac
    done
    export GOBLINS_CODEX_RUNTIME=${runtime}
    ${pkgs.coreutils}/bin/mkdir -m 700 "$GOBLINS_CODEX_RUNTIME"
    profile_overrides=()
    if [[ -n "$profile" ]]; then
      /run/goblins/bin/goblins-codex profile-args "$profile" >"$GOBLINS_CODEX_RUNTIME/profile-args"
      while IFS= read -r -d $'\0' argument; do profile_overrides+=(-c "$argument"); done <"$GOBLINS_CODEX_RUNTIME/profile-args"
      ${pkgs.coreutils}/bin/rm "$GOBLINS_CODEX_RUNTIME/profile-args"
    fi
    server_pid= adapter_pid=
    cleanup() {
      trap - EXIT
      for pid in "$adapter_pid" "$server_pid"; do
        if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
      done
      # Some servers drain work on TERM. Bound that grace period so sandbox
      # teardown cannot wait indefinitely.
      for ((attempt=0; attempt<100; attempt++)); do
        alive=false
        for pid in "$adapter_pid" "$server_pid"; do
          if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then alive=true; fi
        done
        if [[ "$alive" == false ]]; then break; fi
        ${pkgs.coreutils}/bin/sleep 0.05
      done
      for pid in "$adapter_pid" "$server_pid"; do
        if [[ -n "$pid" ]]; then kill -KILL "$pid" 2>/dev/null || true; fi
      done
      for pid in "$adapter_pid" "$server_pid"; do
        if [[ -n "$pid" ]]; then wait "$pid" 2>/dev/null || true; fi
      done
    }
    trap cleanup EXIT
    trap 'exit 143' TERM
    trap 'exit 130' INT
    # Goblins owns sandbox and package policy; hooks and MCP config remain native.
    "$codex_executable" app-server --listen "unix://${socket}" \
      "''${profile_overrides[@]}" "''${server_overrides[@]}" \
      -c 'sandbox_mode="danger-full-access"'${lib.optionalString bypassPermissions " -c 'approval_policy=\"never\"'"} \
      --enable hooks "''${mcp_overrides[@]}" </dev/null &
    server_pid=$!
    ready=false
    for ((attempt=0; attempt<200; attempt++)); do
      if [[ -S ${socket} ]]; then ready=true; break; fi
      if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
      ${pkgs.coreutils}/bin/sleep 0.05
    done
    if [[ "$ready" != true ]]; then
      printf 'Goblins: the Codex App Server failed to start.\n' >&2
      exit 1
    fi
    /run/goblins/bin/goblins-codex run --socket ${socket} "''${thread[@]}"${lib.optionalString bypassPermissions " --bypass-permissions"} </dev/null &
    adapter_pid=$!
    # Either one ending ends the goblin: without the adapter, its conversation
    # was not created or loaded, or nobody delivers its inbox.
    set +e
    wait -n "$server_pid" "$adapter_pid"
    result=$?
    if ! kill -0 "$adapter_pid" 2>/dev/null; then
      printf 'Goblins: the Codex adapter stopped (%s); stopping the goblin.\n' "$result" >&2
      exit $((result == 0 ? 1 : result))
    fi
    exit "$result"
  '';
  # onAttach: the native terminal, a client of the goblin's server.
  client = pkgs.writeShellScriptBin "goblins-codex-attach" ''
    ${setup}
    exec "$codex_executable" --remote unix://${socket} resume "$1"
  '';
  # `codex` in the sandbox runs its own engine with the goblin's sandbox and
  # approval settings. It never connects to the goblin's server.
  wrapper = pkgs.writeShellScriptBin binName ''
    ${setup}
    ${mcp}
    exec "$codex_executable" --sandbox danger-full-access${approval} --enable hooks "''${mcp_overrides[@]}" "$@"
  '';
in
pkgs.symlinkJoin {
  name = "goblins-codex-launcher";
  paths = [
    server
    client
    wrapper
  ];
}
