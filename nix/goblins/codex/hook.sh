# The hook checks metadata only. The agent claims and reads message bodies via
# goblins inbox next. Neither a hook nor terminal output counts as delivery.
input=$(jq -ce '{event: .hook_event_name, active: (.stop_hook_active // false)}')
event=$(jq -r '.event' <<< "$input")
if [[ "$event" == SessionStart ]]; then
  jq -nc '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: "You are a Goblins sandbox agent. Use the goblins skill at /etc/codex/skills/goblins/SKILL.md for daemon inbox messaging and creating child goblins. Check goblins inbox status for initial work. Agent messages must go through the daemon."}}'
elif [[ "$event" == Stop ]]; then
  # Respect Codex continuation-loop protection. The continuation instruction
  # asks the agent to drain work sequentially, with one explicit claim at a time.
  if [[ $(jq -r '.active' <<< "$input") == true ]]; then
    printf '{}\n'
    exit 0
  fi
  if ! status=$(/run/goblins/bin/goblins inbox status); then
    jq -nc '{systemMessage: "Goblins inbox check failed. Check goblins inbox status manually; no inbox item was consumed."}'
    exit 0
  fi
  if jq -e '.queued > 0 or .claim != null' <<< "$status" > /dev/null; then
    jq -nc '{decision: "block", reason: "Check your Goblins daemon inbox with goblins inbox next. Process its item and explicitly reply or complete it, then check again until empty. Use a fresh operation key for each new fetch; preserve the key when retrying an uncertain operation."}'
  else
    printf '{}\n'
  fi
else
  printf '{}\n'
fi
