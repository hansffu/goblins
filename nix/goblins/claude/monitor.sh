# The plugin monitor: Claude Code turns each line into a notification that
# starts a turn at the next idle point, without touching the composer. The
# daemon's notifier decides when (and retries); each line names the message it
# is about, for the Goblins mod's row, never its body. The Stop hook relies on
# this process being alive, so it records its PID and lives exactly as long
# as the notifier.
runtime=/tmp/goblins-claude
prompt='Check your Goblins inbox and process the next item.'
{ [[ -d $runtime ]] || mkdir -m 700 "$runtime"; } 2>/dev/null || true
printf '%s\n' "$$" >"$runtime/monitor.pid.$$" && mv -f "$runtime/monitor.pid.$$" "$runtime/monitor.pid"

wake() {
  local id
  id=$(timeout 3 /run/goblins/bin/goblin inbox status 2>/dev/null |
    jq -r '(.head[0]? // .claim // "") | strings | select(test("^[A-Za-z0-9_-]{1,128}$"))' 2>/dev/null) || id=''
  printf '%s [goblins-inbox-wake%s]\n' "$prompt" "${id:+ $id}"
}

/run/goblins/bin/goblin integration watch --epoch "$GOBLINS_INTEGRATION_EPOCH" --delivery notification |
  while IFS= read -r _; do
    wake
  done
