#!/bin/sh
# Offline native-chat fixture. The first turn stays busy until interrupted,
# allowing the UI to exercise both asynchronous delivery priorities. Later
# turns complete immediately so the same session also covers the idle composer.
set -eu

if [ "${1:-}" = --version ]; then
    printf '%s\n' '2.1.286 (Claude Code fixture)'
    exit 0
fi
if [ "${1:-}" = auth ]; then
    printf '%s\n' '{"loggedIn":true,"email":"fixture@example.test","subscriptionType":"pro"}'
    exit 0
fi

session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
    case "$1" in
        --session-id|--resume) shift; session=$1 ;;
        --session-id=*|--resume=*) session=${1#*=} ;;
    esac
    shift
done

# Native chat restores from the provider's exact on-disk transcript after a
# daemon restart. Mirror that contract rather than emitting an ephemeral stream.
umask 077
config=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
project=$(pwd -P | jq -Rr 'gsub("[^a-zA-Z0-9]"; "-")')
history=$config/projects/$project/$session.jsonl
mkdir -p "$(dirname "$history")"
parent=null
turn=0
if [ -f "$history" ]; then
    parent=$(jq -sr 'last.uuid // null' "$history")
    # Count the prompts handled, not assistant records: a failed turn
    # writes none and a delivered one writes two.
    turn=$(jq -s '[.[] | select(.type == "user" and (.origin.kind // "") != "task-notification"
        and ((.message.content | type) == "string" or any(.message.content[]; .type != "tool_result")))] | length' "$history")
fi
persist_record() {
    record=$(printf '%s' "$1" | jq -c --arg session "$session" --arg parent "$parent" \
        '. + {sessionId:$session,parentUuid:(if $parent == "null" then null else $parent end)}')
    printf '%s\n' "$record" >> "$history"
    parent=$(printf '%s' "$record" | jq -r '.uuid')
}

# Optional tool-heavy transcript for compact activity/disclosure acceptance.
# Every final frame is recorded through the same history path as live frames.
emit_record() {
    persist_record "$1"
    printf '%s\n' "$1"
}
verbosity_turn() {
    input=$(jq -cn '{path:"src/app.rs",prompt:("Internal skill and protocol framing.\n" * 80)}')
    first=$(printf '%s' "$input" | jq -Rs '.[0:30]')
    rest=$(printf '%s' "$input" | jq -Rs '.[30:]')
    jq -cn --arg id "verbosity-inspect-$turn" '{type:"stream_event",event:{type:"message_start",message:{id:$id,role:"assistant"}}}'
    jq -cn --arg id "inspect-$turn" '{type:"stream_event",event:{type:"content_block_start",index:0,content_block:{type:"tool_use",id:$id,name:"mcp__fixture__inspect",input:{}}}}'
    for delta in "$first" "$rest"; do
        jq -cn --argjson delta "$delta" '{type:"stream_event",event:{type:"content_block_delta",index:0,delta:{type:"input_json_delta",partial_json:$delta}}}'
        # Leave each partial state observable by the native UI.
        sleep 1
    done
    jq -cn '{type:"stream_event",event:{type:"content_block_stop",index:0}}'
    emit_record "$(jq -cn --arg turn "$turn" --argjson input "$input" '{type:"assistant",uuid:("verbosity-inspect-"+$turn),message:{id:("verbosity-inspect-"+$turn),role:"assistant",content:[{type:"tool_use",id:("inspect-"+$turn),name:"mcp__fixture__inspect",input:$input}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("verbosity-inspect-result-"+$turn),message:{role:"user",content:[{type:"tool_result",tool_use_id:("inspect-"+$turn),content:"Complete document\nInternal successful protocol framing"}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("verbosity-skill-"+$turn),message:{id:("verbosity-skill-"+$turn),role:"assistant",content:[{type:"tool_use",id:("skill-"+$turn),name:"Skill",input:{skill:"writing-copy",args:("Internal skill instructions. " * 80)}}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("verbosity-skill-result-"+$turn),message:{role:"user",content:[{type:"tool_result",tool_use_id:("skill-"+$turn),content:"Internal skill instructions loaded"}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("verbosity-denied-"+$turn),message:{id:("verbosity-denied-"+$turn),role:"assistant",content:[{type:"tool_use",id:("denied-"+$turn),name:"mcp__fixture__inspect",input:{path:"private.txt",prompt:"Internal framing"}}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("verbosity-denied-result-"+$turn),message:{role:"user",content:[{type:"tool_result",tool_use_id:("denied-"+$turn),is_error:true,content:"Attempted read\nPermission denied"}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("verbosity-shell-"+$turn),message:{id:("verbosity-shell-"+$turn),role:"assistant",content:[{type:"tool_use",id:("shell-"+$turn),name:"Bash",input:{command:"cat <<EOF\nheredoc payload stays in detail\nEOF"}}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("verbosity-shell-result-"+$turn),message:{role:"user",content:[{type:"tool_result",tool_use_id:("shell-"+$turn),content:"Shell fixture complete"}]}}')"
}

# Optional provider-produced image for image preview/viewer acceptance: each
# turn returns this PNG as a base64 block. Native launches drop CAGENTS_*
# variables, so the provider home's fixture-image.png is the default.
fixture_image=${CAGENTS_FIXTURE_IMAGE:-$HOME/fixture-image.png}
image_turn() {
    emit_record "$(base64 -w0 "$fixture_image" | jq -Rsc --arg turn "$turn" '{type:"assistant",uuid:("fixture-image-"+$turn),message:{id:("fixture-image-"+$turn),role:"assistant",content:[{type:"text",text:"Here is the chart from the failed run."},{type:"image",source:{type:"base64",media_type:"image/png",data:.}}]}}')"
}

# Optional behaviors. The daemon removes CAGENTS_* variables from a
# provider's environment, so each is also enabled by a file in the config
# directory. The variables still work when this script is run directly.
fixture_switch() {
    [ "$1" = 1 ] || [ -e "$config/$2" ]
}
fast_fixture=0
if fixture_switch "${CAGENTS_FIXTURE_FAST_MODE:-0}" fixture-fast-mode; then fast_fixture=1; fi
verbosity=0
if fixture_switch "${CAGENTS_FIXTURE_VERBOSITY:-0}" fixture-verbosity; then verbosity=1; fi

busy=false
fast=off
model=haiku
if [ "$fast_fixture" = 1 ]; then model=opus; fi
while IFS= read -r frame; do
    kind=$(printf '%s' "$frame" | jq -r '.type')
    case $kind in
        control_request)
            id=$(printf '%s' "$frame" | jq -r '.request_id')
            subtype=$(printf '%s' "$frame" | jq -r '.request.subtype')
            metadata='{}'
            if [ "$subtype" = initialize ]; then
                metadata='{"claude_code_version":"2.1.286","current_permission_mode":"default","models":[{"value":"haiku","displayName":"Claude Haiku"}]}'
                if [ "$fast_fixture" = 1 ]; then
                    metadata=$(jq -cn --arg fast "$fast" '{claude_code_version:"2.1.286",current_permission_mode:"default",model:"opus",fast_mode_state:$fast,models:[{value:"opus",displayName:"Claude Opus",supportedEffortLevels:["high"]}]}')
                fi
            elif [ "$subtype" = apply_flag_settings ] && [ "$fast_fixture" = 1 ]; then
                requested=$(printf '%s' "$frame" | jq -r '.request.settings.fastMode // false')
                if [ "$requested" = true ]; then fast=on; else fast=off; fi
                metadata=$(jq -cn --arg fast "$fast" '{fast_mode_state:$fast}')
            fi
            jq -cn --arg id "$id" --argjson metadata "$metadata" \
                '{type:"control_response",response:{subtype:"success",request_id:$id,response:$metadata}}'
            case $subtype in
                cancel_async_message)
                    uuid=$(printf '%s' "$frame" | jq -r '.request.message_uuid')
                    jq -cn --arg uuid "$uuid" \
                        '{type:"command_lifecycle",command_uuid:$uuid,state:"cancelled"}'
                    ;;
                interrupt)
                    busy=false
                    jq -cn --arg session "$session" --arg turn "$turn" \
                        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
                    ;;
            esac
            ;;
        user)
            uuid=$(printf '%s' "$frame" | jq -r '.uuid')
            if [ "$busy" = true ]; then
                jq -cn --arg uuid "$uuid" \
                    '{type:"command_lifecycle",command_uuid:$uuid,state:"queued"}'
            else
                turn=$((turn + 1))
                persist_record "$frame"
                printf '%s\n' "$frame"
                jq -cn --arg uuid "$uuid" \
                    '{type:"command_lifecycle",command_uuid:$uuid,state:"started"}'
                jq -cn --arg session "$session" --arg model "$model" \
                    '{type:"system",subtype:"init",session_id:$session,claude_code_version:"2.1.286",model:$model}'
                # Optional provider failure for a prompt with an image, settled
                # before any reply as an expired login does.
                if [ -s "$config/fixture-image-prompt-error" ] &&
                    printf '%s' "$frame" | jq -e '.message.content | arrays | any(.type == "image")' >/dev/null; then
                    jq -cn --arg session "$session" --arg turn "$turn" --rawfile error "$config/fixture-image-prompt-error" \
                        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"error_during_execution",is_error:true,errors:[$error | sub("\\s+$"; "")],session_id:$session}'
                    continue
                fi
                # Optional provider failure after a reply, a background command's
                # completion notice and the work it prompted.
                if [ -s "$config/fixture-delivery-error" ]; then
                    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("fixture-delivery-reply-"+$turn),message:{id:("fixture-delivery-reply-"+$turn),role:"assistant",content:[{type:"text",text:"I started the test suite in the background."}]}}')"
                    emit_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("fixture-delivery-notice-"+$turn),origin:{kind:"task-notification"},message:{role:"user",content:("<task-notification>\n<task-id>fixture-"+$turn+"</task-id>\n<status>completed</status>\n<summary>Background command \"Run tests\" completed (exit code 0)</summary>\n</task-notification>")}}')"
                    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("fixture-delivery-followup-"+$turn),message:{id:("fixture-delivery-followup-"+$turn),role:"assistant",content:[{type:"text",text:"The tests finished. Reviewing their output now."}]}}')"
                    jq -cn --arg session "$session" --arg turn "$turn" --rawfile error "$config/fixture-delivery-error" \
                        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"error_during_execution",is_error:true,errors:[$error | sub("\\s+$"; "")],session_id:$session}'
                    continue
                fi
                if [ "$verbosity" = 1 ]; then verbosity_turn; fi
                if [ -f "$fixture_image" ]; then image_turn; fi
                # A Markdown file in the config directory can stand in for the
                # answer, so rendering and long-transcript checks need no live
                # provider.
                reply="I’m checking the submission logs and comparing the failed run with the last successful version."
                if [ -f "$config/fixture-reply.md" ]; then reply=$(cat "$config/fixture-reply.md"); fi
                assistant=$(jq -cn --arg turn "$turn" --arg text "$reply" \
                    '{type:"assistant",uuid:("fixture-message-"+$turn),message:{id:("fixture-message-"+$turn),role:"assistant",content:[{type:"text",text:$text}],usage:{input_tokens:4200,output_tokens:180}}}')
                persist_record "$assistant"
                printf '%s\n' "$assistant"
                if [ "$turn" -eq 1 ]; then
                    busy=true
                else
                    jq -cn --arg session "$session" --arg turn "$turn" \
                        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
                fi
            fi
            ;;
    esac
done
