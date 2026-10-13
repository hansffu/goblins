# Hooks exchange lifecycle metadata only; the model fetches message bodies.
input=$(jq -ce '{event: .hook_event_name, active: (.stop_hook_active // false),
  tool: (.tool_name // ""), notification: (.notification_type // "")}')
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
