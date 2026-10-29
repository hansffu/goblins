# The latest title in the transcript's last 4 MiB: a custom name (/rename)
# wins over Claude's own. These entries are Claude internals, so this is
# best-effort; it fails when no title can be read.
transcript_title() {
  local transcript=$1
  [[ -n "$transcript" && -r "$transcript" ]] || return 1
  { timeout 5 tail -c 4194304 -- "$transcript" 2>/dev/null || true; } |
    { grep -E '"type"[[:space:]]*:[[:space:]]*"(custom|ai)-title"' || true; } |
    jq -Rner 'reduce (inputs | fromjson? | objects) as $e ({};
        if $e.type == "custom-title" and ($e.customTitle | type) == "string" and $e.customTitle != ""
        then .custom = $e.customTitle
        elif $e.type == "ai-title" and ($e.aiTitle | type) == "string" and $e.aiTitle != ""
        then .ai = $e.aiTitle else . end)
      | .custom // .ai // empty'
}
# Record the conversation's id and name for `goblins recreate`, together,
# when they differ from the last values the daemon acknowledged. Runs
# detached from the hook, one at a time, always on the latest hook's input;
# a failed call is simply retried by the next hook.
runtime=/tmp/goblins-claude
report_variables() {
  local cache=$runtime/variables.json latest id name last_id='' last_name=''
  exec 8>"$runtime/lock"
  flock -w 30 8 || return 0
  latest=$(cat "$runtime/latest.json" 2>/dev/null) || return 0
  id=$(jq -r '.session_id // "" | strings' <<< "$latest")
  [[ -n "$id" ]] || return 0
  if [[ -r "$cache" ]]; then
    last_id=$(jq -r '.session_id // ""' "$cache" 2>/dev/null || true)
    last_name=$(jq -r '.session_name // ""' "$cache" 2>/dev/null || true)
  fi
  if ! name=$(jq -er '.session_title | strings | select(. != "")' <<< "$latest" 2>/dev/null) &&
    ! name=$(transcript_title "$(jq -r '.transcript_path // "" | strings' <<< "$latest")"); then
    # Keep the last known name only for the same conversation.
    if [[ "$id" == "$last_id" ]]; then name=$last_name; else name=''; fi
  fi
  # One line of text, within the daemon's 256-byte value limit.
  name=$(jq -rn --arg name "$name" '$name | gsub("[[:cntrl:]]"; " ") | .[:64]')
  if [[ "$id" == "$last_id" && "$name" == "$last_name" ]]; then return 0; fi
  if timeout 3 /run/goblins/bin/goblin variable set "session_id=$id" "session_name=$name"; then
    jq -nc --arg id "$id" --arg name "$name" '{session_id: $id, session_name: $name}' > "$cache.next" &&
      mv -f "$cache.next" "$cache"
  fi
}
if [[ ${1:-} == --report-variables ]]; then
  report_variables || true
  exit 0
fi
# Hooks exchange lifecycle metadata only; the model fetches message bodies.
raw=$(cat)
event=$(jq -r '.hook_event_name // ""' <<< "$raw")
case "$event" in
  SessionStart | UserPromptSubmit | Stop) ;;
  *)
    # Activity reports never speak to the model or change a decision.
    goblins-claude hook <<< "$raw" >/dev/null 2>&1 || true
    printf '{}\n'
    exit 0
    ;;
esac
# While the plugin monitor runs, a turn never continues from its Stop:
# goblins-claude settles ready and the monitor delivers what is pending (and
# retries it) as its notification, which starts the next turn. Without the
# monitor, Stop blocks the turn into the inbox as before.
monitor_alive() {
  local pid
  pid=$(cat "$runtime/monitor.pid" 2>/dev/null) && [[ $pid =~ ^[0-9]+$ ]] &&
    tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q goblins-claude-monitor
}
args=()
if [[ "$event" == Stop ]] && monitor_alive; then
  args+=(--monitor)
fi
# The hook itself only hands over its input: the inbox decision below must
# reach Claude within the hook's timeout, whatever the transcript or daemon.
if { [[ -d "$runtime" ]] || mkdir -m 700 "$runtime"; } && printf '%s' "$raw" > "$runtime/latest.json.$$" &&
  mv -f "$runtime/latest.json.$$" "$runtime/latest.json"; then
  # Its own session, so it outlives this hook and holds none of its pipes.
  setsid -f "$0" --report-variables </dev/null >/dev/null 2>&1 || true
fi
if ! decision=$(goblins-claude hook "${args[@]}" <<< "$raw"); then
  if [[ "$event" == SessionStart ]]; then
    printf 'Goblins SessionStart daemon update failed.\n' >&2
    printf '{}\n'
  else
    jq -nc '{systemMessage: "Goblins inbox integration check failed. Check goblin inbox status manually; no inbox item was consumed."}'
  fi
  exit 0
fi
printf '%s\n' "$decision"
