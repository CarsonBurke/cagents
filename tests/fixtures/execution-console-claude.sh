#!/bin/sh
set -eu
printf 'spawn\n' >> "$HOME/../spawns"
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
n=0
complete() {
  printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"CONSOLE_RESULT_%s"}],"usage":{"input_tokens":7,"output_tokens":3}}}\n' "$n" "$n" "$n"
  printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$n" "$session"
}
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      printf 'prompt\n' >> "$HOME/../prompts"
      if [ "$n" -eq 1 ]; then
        printf '{"type":"control_request","request_id":"console-approval","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-1","input":{"command":"echo owned-fixture"}}}\n'
      else
        complete
      fi
      ;;
    *'"type":"control_response"'*)
      case "$frame" in *'"behavior":"allow"'*) complete ;; *) exit 19 ;; esac
      ;;
  esac
done
