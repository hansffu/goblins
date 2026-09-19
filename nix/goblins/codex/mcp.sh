# Read Codex's effective MCP configuration without starting MCP servers. Keep
# profile/config overrides consistent with the eventual native invocation.
mcp_overrides=()
scope_args=()
launch_args=("$@")
probe_dir=$PWD
for ((i=0; i<${#launch_args[@]}; i++)); do
  case "${launch_args[i]}" in
    --) break ;;
    -c|--config|-p|--profile|--enable|--disable|-C|--cd)
      option=${launch_args[i]}
      if ((i+1 < ${#launch_args[@]})); then
        ((i+=1))
        if [[ "$option" == -C || "$option" == --cd ]]; then
          probe_dir=${launch_args[i]}
        else
          scope_args+=("$option" "${launch_args[i]}")
        fi
      fi
      ;;
    --config=*|--profile=*|--enable=*|--disable=*) scope_args+=("${launch_args[i]}") ;;
    --cd=*) probe_dir=${launch_args[i]#--cd=} ;;
  esac
done
if mcp_list=$(cd -- "$probe_dir" && "${coreutils}/bin/timeout" 8 "$codex_executable" "${scope_args[@]}" mcp list --json 2>/dev/null); then
  while IFS= read -r row; do
    executable=$("${jq}/bin/jq" -r '.transport.command' <<< "$row")
    available=false
    if [[ "$executable" == */* ]]; then
      working_dir=$("${jq}/bin/jq" -r '.transport.cwd // "."' <<< "$row")
      if [[ "$executable" == /* ]]; then
        [[ -x "$executable" ]] && available=true
      else
        (cd -- "$probe_dir" && cd -- "$working_dir" && [[ -x "$executable" ]]) && available=true
      fi
    elif (cd -- "$probe_dir" && PATH=$("${jq}/bin/jq" -r --arg path "$PATH" '.transport.env.PATH // $path' <<< "$row") command -v -- "$executable") >/dev/null 2>&1; then
      available=true
    fi
    if [[ "$available" == false ]]; then
      name=$("${jq}/bin/jq" -r '.name' <<< "$row")
      # Native -c splits on dots and does not parse quoted TOML key names.
      if [[ "$name" == *.* ]]; then
        printf 'goblins: MCP server %q is unavailable; its dotted name cannot be overridden by native Codex -c\n' "$name" >&2
        continue
      fi
      printf 'goblins: skipping MCP server %q: executable %q is unavailable in this sandbox\n' "$name" "$executable" >&2
      mcp_overrides+=(-c "mcp_servers.$name.enabled=false")
    fi
  done < <("${jq}/bin/jq" -c '.[] | select(.enabled and .transport.type == "stdio")' <<< "$mcp_list")
else
  printf 'goblins: could not inspect MCP configuration; continuing with native settings\n' >&2
fi
