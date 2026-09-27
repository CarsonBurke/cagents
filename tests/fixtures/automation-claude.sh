#!/bin/sh
set -eu
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}
      id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      printf 'prompt\n' >> "$HOME/../prompts"
      printf '{"type":"assistant","uuid":"automation-message","message":{"id":"automation-message","role":"assistant","content":[{"type":"text","text":"SCHEDULED_RESULT"}]}}\n'
      printf '{"type":"result","uuid":"automation-result","subtype":"success","is_error":false,"session_id":"%s"}\n' "$session"
      ;;
  esac
done
