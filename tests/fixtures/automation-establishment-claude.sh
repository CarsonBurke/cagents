#!/bin/sh
set -eu
root=${CLAUDE_ESTABLISHMENT_FIXTURE_ROOT:?fixture evidence directory is required}
printf 'spawn\n' >> "$root/spawns"
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
n=0
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}
      id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      printf 'prompt\n' >> "$root/prompts"
      printf '{"type":"assistant","uuid":"establishment-message-%s","message":{"id":"establishment-message-%s","role":"assistant","content":[{"type":"text","text":"ESTABLISHMENT_RESULT_%s"}]}}\n' "$n" "$n" "$n"
      printf '{"type":"result","uuid":"establishment-result-%s","subtype":"success","is_error":false,"session_id":"%s"}\n' "$n" "$session"
      ;;
  esac
done
