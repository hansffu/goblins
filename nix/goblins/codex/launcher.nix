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
in
pkgs.writeShellScriptBin binName ''
  set -e
  export CODEX_HOME=${directory}
  codex_executable=${lib.escapeShellArg "${pkg}/bin/${binName}"}
  version=$("$codex_executable" --version)
  version=''${version##* }
  if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ $(printf '%s\n' 0.162.1 "$version" | ${pkgs.coreutils}/bin/sort -V | ${pkgs.coreutils}/bin/head -n1) != 0.162.1 ]]; then
    printf 'Goblins requires Codex 0.162.1 or newer (found %s).\n' "$version" >&2
    exit 1
  fi
  mcp_overrides=()
  ${lib.optionalString filterUnavailableMcp (
    lib.replaceStrings [ "\${coreutils}" "\${jq}" ] [ "${pkgs.coreutils}" "${pkgs.jq}" ] (
      builtins.readFile ./mcp.sh
    )
  )}
  # Native utility/exec subcommands do not accept --remote. Keep them usable.
  skip_value=false
  resume=false
  for argument in "$@"; do
    if [[ "$skip_value" == true ]]; then skip_value=false; continue; fi
    case "$argument" in
      -c|--config|-m|--model|-p|--profile|-s|--sandbox|-a|--ask-for-approval|-C|--cd|--add-dir|--local-provider|--enable|--disable)
        skip_value=true ;;
      --help|-h|--version|-V|exec|e|review|login|logout|mcp|plugin|app-server|remote-control|completion|update|doctor|sandbox|debug|apply|queue|archive|delete|migrate-rollouts|unarchive|cloud|exec-server|features|help)
        exec "$codex_executable" --sandbox danger-full-access${approval} --enable hooks "''${mcp_overrides[@]}" "$@" ;;
      resume) resume=true; break ;;
      --|-i|--image) break ;;
      -*) ;;
      *) break ;;
    esac
  done
  # Codex refuses permission overrides when resuming a remote thread. The
  # adapter sets the resumed thread's sandbox and approval policy instead,
  # since a thread keeps the policy it was saved with.
  permission_args=(--sandbox danger-full-access${approval})
  if [[ "$resume" == true ]]; then
    permission_args=()
    for argument in "$@"; do
      case "$argument" in
        --) break ;;
        -s|--sandbox|--sandbox=*|-s?*|-a|--ask-for-approval|--ask-for-approval=*|-a?*)
          printf 'Goblins: %s cannot be combined with resume; Goblins sets the sandbox and approval policy of the resumed thread.\n' "$argument" >&2
          exit 2 ;;
      esac
    done
  fi
  # /tmp is private to this sandbox, including for parent and child goblins.
  # Keep the server socket outside the mounted/shared Codex configuration.
  export GOBLINS_CODEX_RUNTIME=/tmp/goblins-codex
  ${pkgs.coreutils}/bin/mkdir -m 700 "$GOBLINS_CODEX_RUNTIME"
  # Remote thread creation forwards only selected native config fields. Apply
  # CLI configuration and the selected profile to the server as well. Preserve
  # argument boundaries and literal shell text; never eval configuration.
  server_overrides=()
  profile=
  launch_args=("$@")
  for ((i=0; i<''${#launch_args[@]}; i++)); do
    case "''${launch_args[i]}" in
      --) break ;;
      -c|--config|--enable|--disable)
        option=''${launch_args[i]}
        if ((i+1 < ''${#launch_args[@]})); then
          ((i+=1))
          server_overrides+=("$option" "''${launch_args[i]}")
        fi ;;
      --config=*|--enable=*|--disable=*) server_overrides+=("''${launch_args[i]}") ;;
      -c?*) server_overrides+=("''${launch_args[i]}") ;;
      -p|--profile)
        if ((i+1 < ''${#launch_args[@]})); then ((i+=1)); profile=''${launch_args[i]}; fi ;;
      --profile=*) profile=''${launch_args[i]#--profile=} ;;
      -p?*) profile=''${launch_args[i]#-p} ;;
    esac
  done
  profile_overrides=()
  if [[ -n "$profile" ]]; then
    /run/goblins/bin/goblins-codex profile-args "$profile" >"$GOBLINS_CODEX_RUNTIME/profile-args"
    while IFS= read -r -d $'\0' argument; do profile_overrides+=(-c "$argument"); done <"$GOBLINS_CODEX_RUNTIME/profile-args"
    ${pkgs.coreutils}/bin/rm "$GOBLINS_CODEX_RUNTIME/profile-args"
  fi
  socket="$GOBLINS_CODEX_RUNTIME/app-server.sock"
  native_socket="$GOBLINS_CODEX_RUNTIME/native.sock"
  # Bash replaces stdin for asynchronous jobs before applying self-duplication.
  # Save the terminal explicitly so the native client keeps reading real input.
  exec 9<&0
  server_pid= adapter_pid= native_pid=
  cleanup() {
    trap - EXIT
    for pid in "$native_pid" "$adapter_pid" "$server_pid"; do
      if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
    done
    # Some servers drain work on TERM. Bound that grace period so terminal exit
    # and sandbox teardown cannot leave helpers or wait indefinitely.
    for ((attempt=0; attempt<100; attempt++)); do
      alive=false
      for pid in "$native_pid" "$adapter_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then alive=true; fi
      done
      if [[ "$alive" == false ]]; then break; fi
      ${pkgs.coreutils}/bin/sleep 0.05
    done
    for pid in "$native_pid" "$adapter_pid" "$server_pid"; do
      if [[ -n "$pid" ]]; then kill -KILL "$pid" 2>/dev/null || true; fi
    done
    for pid in "$native_pid" "$adapter_pid" "$server_pid"; do
      if [[ -n "$pid" ]]; then wait "$pid" 2>/dev/null || true; fi
    done
  }
  trap cleanup EXIT
  trap 'exit 143' TERM
  trap 'exit 130' INT
  # Goblins owns sandbox and package policy; hooks and MCP config remain native.
  "$codex_executable" app-server --listen "unix://$socket" \
    "''${profile_overrides[@]}" "''${server_overrides[@]}" \
    -c 'sandbox_mode="danger-full-access"'${lib.optionalString bypassPermissions " -c 'approval_policy=\"never\"'"} \
    --enable hooks "''${mcp_overrides[@]}" \
    9<&- </dev/null >"$GOBLINS_CODEX_RUNTIME/app-server.log" 2>&1 &
  server_pid=$!
  ready=false
  for ((attempt=0; attempt<200; attempt++)); do
    if [[ -S "$socket" ]]; then ready=true; break; fi
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    ${pkgs.coreutils}/bin/sleep 0.05
  done
  if [[ "$ready" != true ]]; then
    printf 'Goblins Codex App Server failed to start; see %s/app-server.log.\n' "$GOBLINS_CODEX_RUNTIME" >&2
    exit 1
  fi
  /run/goblins/bin/goblins-codex run --socket "$socket" --native-socket "$native_socket"${lib.optionalString bypassPermissions " --bypass-permissions"} \
    9<&- </dev/null >"$GOBLINS_CODEX_RUNTIME/adapter.log" 2>&1 &
  adapter_pid=$!
  ready=false
  for ((attempt=0; attempt<200; attempt++)); do
    if [[ -S "$native_socket" ]]; then ready=true; break; fi
    if ! kill -0 "$adapter_pid" 2>/dev/null; then break; fi
    ${pkgs.coreutils}/bin/sleep 0.05
  done
  if [[ "$ready" != true ]]; then
    printf 'Goblins Codex adapter failed to start; see %s/adapter.log.\n' "$GOBLINS_CODEX_RUNTIME" >&2
    exit 1
  fi
  "$codex_executable" --remote "unix://$native_socket" \
    "''${permission_args[@]}" --enable hooks \
    "''${mcp_overrides[@]}" "$@" <&9 9<&- &
  native_pid=$!
  # Preserve native exit status; EXIT tears down both sandbox-local helpers.
  set +e
  wait "$native_pid"
  result=$?
  native_pid=
  exit "$result"
''
