#!/bin/sh
# A provider that streams its answer and runs tools with large outputs, the
# shape a console has to reduce to what the agent said and one line per step.
set -eu
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
delta() {
  printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"%s"}}}\n' "$1"
  sleep 0.4
}
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      printf '{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1","role":"assistant"}}}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}\n'
      delta 'Looking at '
      delta 'the failing '
      delta 'test.'
      printf '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}\n'
      printf '{"type":"assistant","uuid":"m1","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Looking at the failing test."}]}}\n'
      printf '{"type":"assistant","uuid":"m2","message":{"id":"m2","role":"assistant","content":[{"type":"tool_use","id":"bash-1","name":"Bash","input":{"command":"cargo test --locked"}}]}}\n'
      sleep 0.4
      printf '{"type":"user","uuid":"m3","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bash-1","is_error":true,"content":"PAYLOAD_OUTPUT\\nExit code 101"}]}}\n'
      printf '{"type":"assistant","uuid":"m4","message":{"id":"m4","role":"assistant","content":[{"type":"tool_use","id":"agent-1","name":"Agent","input":{"prompt":"PAYLOAD_INPUT"}}]}}\n'
      sleep 0.4
      printf '{"type":"user","uuid":"m5","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"agent-1","content":"agentId: PAYLOAD_OUTPUT"}]}}\n'
      printf '{"type":"assistant","uuid":"m6","message":{"id":"m6","role":"assistant","content":[{"type":"text","text":"The second paragraph follows."}],"usage":{"input_tokens":7,"output_tokens":3}}}\n'
      printf '{"type":"result","uuid":"r1","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$session"
      ;;
  esac
done
