# The plugin monitor: Claude Code turns each line into a notification that
# starts a turn at the next idle point, without touching the composer.
# goblins-claude decides when (and retries); each line names the message it
# is about, for the Goblins mod's row, never its body. The Stop hook relies on
# this process being alive, so it records its PID and lives exactly as long
# as the watch, which ends when a newer launch of Claude starts its own.
runtime=/tmp/goblins-claude
{ [[ -d $runtime ]] || mkdir -m 700 "$runtime"; } 2>/dev/null || true
printf '%s\n' "$$" >"$runtime/monitor.pid.$$" && mv -f "$runtime/monitor.pid.$$" "$runtime/monitor.pid"
goblins-claude watch
