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
    turn=$(jq -s '[.[] | select(.type == "assistant")] | length' "$history")
fi
persist_record() {
    record=$(printf '%s' "$1" | jq -c --arg session "$session" --arg parent "$parent" \
        '. + {sessionId:$session,parentUuid:(if $parent == "null" then null else $parent end)}')
    printf '%s\n' "$record" >> "$history"
    parent=$(printf '%s' "$record" | jq -r '.uuid')
}

busy=false
while IFS= read -r frame; do
    kind=$(printf '%s' "$frame" | jq -r '.type')
    case $kind in
        control_request)
            id=$(printf '%s' "$frame" | jq -r '.request_id')
            subtype=$(printf '%s' "$frame" | jq -r '.request.subtype')
            metadata='{}'
            if [ "$subtype" = initialize ]; then
                metadata='{"claude_code_version":"2.1.286","current_permission_mode":"default","models":[{"value":"haiku","displayName":"Claude Haiku"}]}'
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
                jq -cn --arg session "$session" \
                    '{type:"system",subtype:"init",session_id:$session,claude_code_version:"2.1.286",model:"haiku"}'
                assistant=$(jq -cn --arg turn "$turn" \
                    '{type:"assistant",uuid:("fixture-message-"+$turn),message:{id:("fixture-message-"+$turn),role:"assistant",content:[{type:"text",text:"I’m checking the submission logs and comparing the failed run with the last successful version."}],usage:{input_tokens:4200,output_tokens:180}}}')
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
