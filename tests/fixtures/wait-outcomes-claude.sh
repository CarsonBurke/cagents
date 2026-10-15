#!/bin/sh
set -eu
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
n=0
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      case "$frame" in
        *'"subtype":"interrupt"'*)
          printf '{"type":"result","uuid":"interrupted-%s","subtype":"success","is_error":false,"session_id":"%s"}\n' "$n" "$session"
          ;;
      esac
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      case "$frame" in
        *CLI_WAIT_BUSY*) ;;
        *CLI_WAIT_FAIL*)
          printf '{"type":"result","uuid":"failed-%s","subtype":"error_during_execution","is_error":true,"session_id":"%s","errors":["fixture execution failed"]}\n' "$n" "$session"
          ;;
        *)
          printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"Fixture success"}]}}\n' "$n" "$n"
          printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s"}\n' "$n" "$session"
          ;;
      esac
      ;;
  esac
done
