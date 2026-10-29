# Read one inbox message for the Claude mod's views: messages.get on the
# sandbox endpoint, which the goblin CLI does not expose. Read-only by
# construction: it sends initialize and this one method, nothing else.
# Prints the Message as JSON; exits 1 on a daemon error, 2 when unreachable.
export LC_ALL=C # Content-Length counts bytes
socket=/run/goblins/request.sock
id=${1:?usage: goblins-claude-message MESSAGE_ID}
if ! message=$(jq -nr --arg id "$id" '$id | select(test("^[A-Za-z0-9_-]{1,128}$"))') ||
  [[ -z $message ]]; then
  printf 'goblins-claude-message: invalid message id\n' >&2
  exit 2
fi
coproc rpc { exec socat -t 5 -T 10 - "UNIX-CONNECT:$socket" 2>/dev/null; }
# shellcheck disable=SC2154 # coproc sets rpc_PID
trap 'kill "$rpc_PID" 2>/dev/null || true' EXIT
send() {
  printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1" >&"${rpc[1]}"
}
# One response frame: headers up to an empty line, then exactly Content-Length bytes.
receive() {
  local line length=''
  while IFS= read -r -t 5 line <&"${rpc[0]}"; do
    line=${line%$'\r'}
    [[ -n $line ]] || break
    if [[ ${line,,} =~ ^content-length:[[:space:]]*([0-9]+)$ ]]; then
      length=${BASH_REMATCH[1]}
    fi
  done
  [[ -n $length ]] && ((length <= 1048576)) || return 1
  IFS= read -r -N "$length" -t 5 frame <&"${rpc[0]}" && ((${#frame} == length))
}
call() {
  local id=$1 method=$2 params=$3
  send "$(jq -cn --argjson id "$id" --arg method "$method" --argjson params "$params" \
    '{jsonrpc: "2.0", id: $id, method: $method, params: $params}')" || return 2
  receive || return 2
  jq -e --argjson id "$id" '.id == $id' <<<"$frame" >/dev/null || return 2
  if jq -e 'has("error")' <<<"$frame" >/dev/null; then
    jq -c '.error' <<<"$frame" >&2
    return 1
  fi
  jq -c '.result' <<<"$frame"
}
status=0
init=$(call 0 initialize '{"api":1}') || status=$?
if ((status == 0)) && ! jq -e '.api == 1 and .role == "sandbox"' <<<"$init" >/dev/null; then
  printf 'goblins-claude-message: incompatible daemon\n' >&2
  exit 2
fi
((status == 0)) || exit "$status"
call 1 messages.get "$(jq -cn --arg id "$message" '{message: $id}')"
