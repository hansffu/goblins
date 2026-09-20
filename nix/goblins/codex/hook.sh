# Hooks exchange lifecycle metadata only; the model fetches message bodies.
input=$(jq -ce '{event: .hook_event_name, active: (.stop_hook_active // false)}')
event=$(jq -r '.event' <<< "$input")
args=(--event "$event")
if [[ $(jq -r .active <<< "$input") == true ]]; then args+=(--active); fi
if ! decision=$(/run/goblins/bin/goblins integration hook "${args[@]}"); then
  jq -nc '{systemMessage: "Goblins inbox integration check failed. Check goblins inbox status manually; no inbox item was consumed."}'
  exit 0
fi
if [[ "$event" == SessionStart ]]; then
  jq -nc '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: "You are a Goblins sandbox agent. Use the goblins skill at /etc/codex/skills/goblins/SKILL.md for daemon inbox messaging and creating child goblins. Check goblins inbox status for initial work. Agent messages must go through the daemon."}}'
elif [[ "$event" == Stop ]] && jq -e '.continue' <<< "$decision" >/dev/null; then
  jq -nc '{decision: "block", reason: "Check your Goblins daemon inbox with goblins inbox next. Process its item and explicitly reply or complete it, then check again until empty. Use a fresh operation key for each new fetch; preserve the key when retrying an uncertain operation."}'
else
  printf '{}\n'
fi
