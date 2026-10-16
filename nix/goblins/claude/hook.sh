# Hooks exchange lifecycle metadata only; the model fetches message bodies.
raw=$(cat)
input=$(jq -ce '{event: .hook_event_name, active: (.stop_hook_active // false),
  tool: (.tool_name // ""), notification: (.notification_type // "")}' <<< "$raw")
event=$(jq -r '.event' <<< "$input")
args=(--event "$event")
if [[ $(jq -r .active <<< "$input") == true ]]; then args+=(--active); fi
tool=$(jq -r .tool <<< "$input")
notification=$(jq -r .notification <<< "$input")
if [[ -n "$tool" ]]; then args+=(--tool "$tool"); fi
if [[ -n "$notification" ]]; then args+=(--notification "$notification"); fi
case "$event" in
  SessionStart | UserPromptSubmit | Stop) ;;
  *)
    # Activity reports never speak to the model or change a decision.
    /run/goblins/bin/goblins integration hook "${args[@]}" >/dev/null 2>&1 || true
    printf '{}\n'
    exit 0
    ;;
esac
# The latest title in the transcript, read from its end: a custom name
# (/rename) wins over Claude's own. These entries are Claude internals, so
# this is best-effort; it fails when no title can be read.
transcript_title() {
  local transcript=$1 kind line
  [[ -n "$transcript" && -r "$transcript" ]] || return 1
  for kind in custom-title:customTitle ai-title:aiTitle; do
    line=$({ timeout 1 tac -- "$transcript" 2>/dev/null || true; } |
      grep -m1 -E "\"type\"[[:space:]]*:[[:space:]]*\"${kind%%:*}\"" || true)
    if [[ -n "$line" ]] && jq -er --arg field "${kind#*:}" '.[$field] | strings | select(. != "")' <<< "$line"; then
      return 0
    fi
  done
  return 1
}
# Record the conversation's id and name for `goblins recreate`, together,
# when they differ from the last values the daemon acknowledged. A failed
# call is simply retried on the next hook.
report_variables() {
  local cache=/tmp/goblins-claude-variables.json id name last_id='' last_name=''
  id=$(jq -r '.session_id // "" | strings' <<< "$raw")
  [[ -n "$id" ]] || return 0
  if [[ -r "$cache" ]]; then
    last_id=$(jq -r '.session_id // ""' "$cache" 2>/dev/null || true)
    last_name=$(jq -r '.session_name // ""' "$cache" 2>/dev/null || true)
  fi
  if ! name=$(jq -er '.session_title | strings | select(. != "")' <<< "$raw" 2>/dev/null) &&
    ! name=$(transcript_title "$(jq -r '.transcript_path // "" | strings' <<< "$raw")"); then
    # Keep the last known name only for the same conversation.
    if [[ "$id" == "$last_id" ]]; then name=$last_name; else name=''; fi
  fi
  # One line of text, within the daemon's 256-byte value limit.
  name=$(jq -rn --arg name "$name" '$name | gsub("[[:cntrl:]]"; " ") | .[:64]')
  if [[ "$id" == "$last_id" && "$name" == "$last_name" ]]; then return 0; fi
  if timeout 2 /run/goblins/bin/goblins variable set "session_id=$id" "session_name=$name" >/dev/null 2>&1; then
    jq -nc --arg id "$id" --arg name "$name" '{session_id: $id, session_name: $name}' > "$cache.next" &&
      mv -f "$cache.next" "$cache"
  fi
}
# Never changes the inbox decision or the hook's output.
report_variables || true
if ! decision=$(/run/goblins/bin/goblins integration hook "${args[@]}"); then
  if [[ "$event" == SessionStart ]]; then
    printf 'Goblins SessionStart daemon update failed.\n' >&2
    printf '{}\n'
  else
    jq -nc '{systemMessage: "Goblins inbox integration check failed. Check goblins inbox status manually; no inbox item was consumed."}'
  fi
  exit 0
fi
if [[ "$event" == Stop ]] && jq -e '.continue' <<< "$decision" >/dev/null; then
  jq -nc '{decision: "block", reason: "Check your Goblins daemon inbox with goblins inbox next. Process its item and explicitly reply or complete it, then check again until empty. Use a fresh operation key for each new fetch; preserve the key when retrying an uncertain operation."}'
else
  printf '{}\n'
fi
