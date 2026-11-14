#!/usr/bin/env bash
# usage: ask_review.sh <request-file> <reply-out-file>
set -u
goblins send speedup-reviewer --file "$1" >/dev/null || exit 1
while true; do
  q=$(goblins inbox status 2>/dev/null | python3 -c "import json,sys;print(json.load(sys.stdin)['queued'])" 2>/dev/null || echo 0)
  [ "$q" != "0" ] && break
  sleep 20
done
goblins inbox next 2>/dev/null | tail -n1 > "$2.json"
python3 - "$2.json" "$2" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
open(sys.argv[2],'w').write(f"FROM {d['message'].get('sender_name')}\n"+d['message']['body'].replace('\\n','\n'))
import subprocess
subprocess.run(["goblins","inbox","complete","--claim-generation",str(d['claim_generation']),d['message']['id']],capture_output=True)
PY
cat "$2"
