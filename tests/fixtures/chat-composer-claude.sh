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
task_scene=
task_scene_file=$config/fixture-agent-task-receipt.json
if [ -e "$task_scene_file" ] || [ -L "$task_scene_file" ]; then
    if [ ! -f "$task_scene_file" ] || [ -L "$task_scene_file" ] ||
        ! task_scene=$(jq -ers 'select(length == 1) | .[0]
            | select(type == "object" and keys == ["situation", "status"])
            | select(.situation == "text" or .situation == "task-only")
            | select(.status == "completed" or .status == "failed")
            | .situation + "-" + .status' "$task_scene_file" 2>/dev/null); then
        printf '%s\n' 'Invalid fixture-agent-task-receipt.json' >&2
        exit 2
    fi
fi
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

# A fixed live receipt precedes its launch acknowledgement, without a timer.
# The main result ends the turn; only the receipt/ACK can settle its Agent.
task_receipt_turn() {
    agent_tool=fixture-agent-tool-$turn
    agent_task=fixture-agent-task-$turn
    agent_status=${task_scene##*-}
    emit_record "$(jq -cn --arg turn "$turn" --arg tool "$agent_tool" '{type:"assistant",uuid:("fixture-agent-launch-"+$turn),message:{id:("fixture-agent-launch-"+$turn),role:"assistant",content:[{type:"tool_use",id:$tool,name:"Agent",input:{description:"Check receipt settlement",run_in_background:true}}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" --arg tool "$agent_tool" --arg task "$agent_task" '{type:"system",subtype:"task_started",uuid:("fixture-agent-start-"+$turn),task_id:$task,tool_use_id:$tool,task_type:"local_agent",description:"Check receipt settlement"}')"
    case $task_scene in
        text-*)
            emit_record "$(jq -cn --arg turn "$turn" --arg tool "$agent_tool" --arg task "$agent_task" --arg status "$agent_status" '{type:"user",uuid:("fixture-agent-notice-"+$turn),origin:{kind:"task-notification"},message:{role:"user",content:("<task-notification>\n<task-id>"+$task+"</task-id>\n<tool-use-id>"+$tool+"</tool-use-id>\n<status>"+$status+"</status>\n<summary>Offline Agent task "+$status+"</summary>\n<result>Offline report.</result>\n</task-notification>")}}')"
            ;;
        task-only-*)
            emit_record "$(jq -cn --arg turn "$turn" --arg task "$agent_task" --arg status "$agent_status" '{type:"system",subtype:"task_notification",uuid:("fixture-agent-notice-"+$turn),task_id:$task,status:$status,summary:("Offline Agent task "+$status)}')"
            ;;
    esac
    emit_record "$(jq -cn --arg turn "$turn" --arg tool "$agent_tool" --arg task "$agent_task" '{type:"user",uuid:("fixture-agent-ack-"+$turn),tool_use_result:{status:"async_launched",agentId:$task},message:{role:"user",content:[{type:"tool_result",tool_use_id:$tool,content:"Launched"}]}}')"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("fixture-agent-answer-"+$turn),message:{id:("fixture-agent-answer-"+$turn),role:"assistant",content:[{type:"text",text:"The offline task receipt has arrived."}]}}')"
    emit_record "$(jq -cn --arg session "$session" --arg turn "$turn" '{type:"result",uuid:("fixture-agent-result-"+$turn),subtype:"success",is_error:false,session_id:$session}')"
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

# Optional scripted tool calls: each line of fixture-tools.jsonl is
# {"name":..., "input":{...}, "result":"...", "is_error":bool}, emitted as a
# tool call and its result on every turn, so a rendering check can choose the
# exact calls and outputs it needs.
tools_file=$config/fixture-tools.jsonl
tools_turn() {
    call=0
    while IFS= read -r spec; do
        [ -n "$spec" ] || continue
        call=$((call + 1))
        id="tool-$turn-$call"
        emit_record "$(printf '%s' "$spec" | jq -c --arg id "$id" '{type:"assistant",uuid:("fixture-"+$id),message:{id:("fixture-"+$id),role:"assistant",content:[{type:"tool_use",id:$id,name:.name,input:.input}]}}')"
        emit_record "$(printf '%s' "$spec" | jq -c --arg id "$id" '{type:"user",uuid:("fixture-"+$id+"-result"),message:{role:"user",content:[{type:"tool_result",tool_use_id:$id,is_error:(.is_error // false),content:(.result // "")}]}}')"
    done < "$tools_file"
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

# Optional scene: a turn blocked on a long command with subagents running, so
# queued messages wait and Send now has something to move aside. The frames and
# their order follow tests/fixtures/claude-send-now/, recorded from Claude Code
# 2.1.296. The file's first line is how many subagents to run (default 1); a
# fixture-send-now-abort file beside it has the turn writing a reply instead of
# waiting on a command, the path where Send now stops the turn. Subagents and
# commands run until stopped. No model or real tool runs: this does not exercise
# when the provider chooses each path, or how long it takes.
subagent_wait=0
subagents=1
version=2.1.286
capabilities='[]'
if fixture_switch "${CAGENTS_FIXTURE_SUBAGENT_WAIT:-0}" fixture-subagent-wait; then
    subagent_wait=1
    version=2.1.296
    capabilities='["interrupt_send_now_v1"]'
    if [ -s "$config/fixture-subagent-wait" ]; then subagents=$(head -n 1 "$config/fixture-subagent-wait"); fi
    scene=$(mktemp -d)
    : > "$scene/waiting"
    trap 'rm -rf "$scene"; [ -z "${reporter:-}" ] || kill "$reporter" 2>/dev/null || :' EXIT
fi
scene_subagents_start() {
    i=1
    while [ "$i" -le "$subagents" ]; do
        jq -cn --arg i "$i" '{type:"assistant",uuid:("scene-agent-"+$i),parent_tool_use_id:null,message:{id:("scene-agent-"+$i),role:"assistant",content:[{type:"tool_use",id:("toolu_scene_agent_"+$i),name:"Agent",input:{description:("Check area "+$i),prompt:"Fixture subagent",subagent_type:"general-purpose",run_in_background:true}}]}}'
        jq -cn --arg i "$i" '{type:"system",subtype:"task_started",task_id:("ascene"+$i),tool_use_id:("toolu_scene_agent_"+$i),description:("Check area "+$i),subagent_type:"general-purpose",is_backgrounded:true,task_type:"local_agent"}'
        jq -cn --arg i "$i" '{type:"user",uuid:("scene-agent-result-"+$i),parent_tool_use_id:null,message:{role:"user",content:[{tool_use_id:("toolu_scene_agent_"+$i),type:"tool_result",content:[{type:"text",text:("Async agent launched successfully.\nagentId: ascene"+$i)}]}]},tool_use_result:{isAsync:true,status:"async_launched",agentId:("ascene"+$i),description:("Check area "+$i)}}'
        : > "$scene/child-$i"
        i=$((i + 1))
    done
    # Each subagent keeps reporting whatever the main turn is doing.
    (
        step=0
        while kill -0 "$$" 2>/dev/null; do
            sleep 2
            step=$((step + 1))
            i=1
            while [ "$i" -le "$subagents" ]; do
                if [ -e "$scene/child-$i" ]; then
                    jq -cn --arg i "$i" --arg step "$step" '{type:"assistant",uuid:("scene-child-"+$i+"-"+$step),parent_tool_use_id:("toolu_scene_agent_"+$i),message:{id:("scene-child-"+$i+"-"+$step),role:"assistant",content:[{type:"text",text:("Checked file "+$step+" of area "+$i+".")}]}}'
                fi
                i=$((i + 1))
            done
        done
    ) &
    reporter=$!
}
scene_stop_child() {
    jq -cn --arg i "$1" '{type:"system",subtype:"task_updated",task_id:("ascene"+$i),patch:{status:"killed"}}'
    jq -cn --arg i "$1" '{type:"system",subtype:"task_notification",task_id:("ascene"+$i),tool_use_id:("toolu_scene_agent_"+$i),status:"stopped",summary:"Stopped"}'
    rm -f "$scene/child-$1"
}
scene_stop_children() {
    for child in "$scene"/child-*; do
        [ -e "$child" ] || continue
        scene_stop_child "${child##*/child-}"
    done
}
# Everything waiting goes in together, and the turn it joined answers it.
scene_deliver_waiting() {
    uuids=''
    while IFS= read -r waiting; do
        turn=$((turn + 1))
        persist_record "$waiting"
        printf '%s\n' "$waiting"
        uuid=$(printf '%s' "$waiting" | jq -r '.uuid')
        uuids="$uuids $uuid"
        jq -cn --arg uuid "$uuid" '{type:"command_lifecycle",command_uuid:$uuid,state:"started"}'
    done < "$scene/waiting"
    : > "$scene/waiting"
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("scene-answer-"+$turn),message:{id:("scene-answer-"+$turn),role:"assistant",content:[{type:"text",text:"Got your message. The subagents are still checking their areas, and I will report when they finish."}]}}')"
    for uuid in $uuids; do
        jq -cn --arg uuid "$uuid" '{type:"command_lifecycle",command_uuid:$uuid,state:"completed"}'
    done
    jq -cn --arg session "$session" --arg turn "$turn" \
        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,stop_reason:"end_turn",session_id:$session}'
    busy=false
}
scene_send_now() {
    if [ -e "$config/fixture-send-now-abort" ]; then
        emit_record "$(jq -cn '{type:"assistant",uuid:"scene-cut-off",message:{id:"scene-cut-off",role:"assistant",content:[{type:"text",text:"Here is the plan so f"}]}}')"
        jq -cn '{type:"user",uuid:"scene-interrupted",parent_tool_use_id:null,message:{role:"user",content:[{type:"text",text:"[Request interrupted by user]"}]}}'
        jq -cn --arg session "$session" \
            '{type:"result",uuid:"scene-stopped-result",subtype:"error_during_execution",is_error:true,errors:["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],session_id:$session}'
        jq -cn --arg uuid "$scene_turn" '{type:"command_lifecycle",command_uuid:$uuid,state:"cancelled"}'
    else
        jq -cn '{type:"system",subtype:"task_updated",task_id:"bscene",patch:{is_backgrounded:true}}'
        jq -cn '{type:"user",uuid:"scene-moved",parent_tool_use_id:null,message:{role:"user",content:[{tool_use_id:"toolu_scene_bash",type:"tool_result",content:"Command was moved to the background (ID: bscene) so that a message that arrived while it was running can reach you; it was not interrupted.",is_error:false}]},tool_use_result:{stdout:"",stderr:"",interrupted:false,backgroundTaskId:"bscene",backgroundedToDeliverMessage:true}}'
    fi
    scene_deliver_waiting
}

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
                metadata=$(jq -cn --arg version "$version" '{claude_code_version:$version,current_permission_mode:"default",capabilities:["ui_surface_v1"],models:[{value:"haiku",displayName:"Claude Haiku"}]}')
                if [ "$fast_fixture" = 1 ]; then
                    metadata=$(jq -cn --arg fast "$fast" '{claude_code_version:"2.1.286",current_permission_mode:"default",model:"opus",fast_mode_state:$fast,models:[{value:"opus",displayName:"Claude Opus",supportedEffortLevels:["high"]}]}')
                fi
            elif [ "$subtype" = apply_flag_settings ] && [ "$fast_fixture" = 1 ]; then
                requested=$(printf '%s' "$frame" | jq -r '.request.settings.fastMode // false')
                if [ "$requested" = true ]; then fast=on; else fast=off; fi
                metadata=$(jq -cn --arg fast "$fast" '{fast_mode_state:$fast}')
            fi
            send_now=false
            if [ "$subagent_wait" = 1 ] && [ "$subtype" = interrupt ] &&
                [ "$(printf '%s' "$frame" | jq -r '.request.send_now == true')" = true ]; then
                send_now=true
                answer=nothing_waiting
                if [ "$busy" = true ] && [ -s "$scene/waiting" ]; then
                    answer=delivering
                    if [ -e "$config/fixture-send-now-abort" ]; then answer=stopped; fi
                fi
                metadata=$(jq -cn --arg answer "$answer" '{still_queued:[],send_now:$answer}')
                # The answer may be slow to come back.
                if [ -s "$config/fixture-send-now-delay" ]; then sleep "$(cat "$config/fixture-send-now-delay")"; fi
            fi
            jq -cn --arg id "$id" --argjson metadata "$metadata" \
                '{type:"control_response",response:{subtype:"success",request_id:$id,response:$metadata}}'
            case $subtype in
                cancel_async_message)
                    uuid=$(printf '%s' "$frame" | jq -r '.request.message_uuid')
                    jq -cn --arg uuid "$uuid" \
                        '{type:"command_lifecycle",command_uuid:$uuid,state:"cancelled"}'
                    if [ "$subagent_wait" = 1 ]; then
                        jq -c --arg uuid "$uuid" 'select(.uuid != $uuid)' "$scene/waiting" > "$scene/kept"
                        mv "$scene/kept" "$scene/waiting"
                    fi
                    ;;
                stop_task)
                    task=$(printf '%s' "$frame" | jq -r '.request.task_id')
                    case $task in
                        bscene)
                            jq -cn '{type:"system",subtype:"task_updated",task_id:"bscene",patch:{status:"killed"}}'
                            jq -cn '{type:"system",subtype:"task_notification",task_id:"bscene",tool_use_id:"toolu_scene_bash",status:"stopped",summary:"Background command stopped"}'
                            ;;
                        ascene*) scene_stop_child "${task#ascene}" ;;
                    esac
                    ;;
                interrupt)
                    if [ "$send_now" = true ]; then
                        if [ "$answer" != nothing_waiting ]; then scene_send_now; fi
                        continue
                    fi
                    if [ "$subagent_wait" = 1 ]; then
                        # A plain interrupt stops running subagents even with
                        # the turn over, but not a command already moved aside.
                        scene_stop_children
                        if [ "$busy" = false ]; then
                            jq -cn '{type:"user",uuid:"scene-rejected",parent_tool_use_id:null,message:{role:"user",content:[{type:"text",text:"[Request interrupted by user for tool use]"}]}}'
                            continue
                        fi
                        jq -cn '{type:"user",uuid:"scene-interrupted",parent_tool_use_id:null,message:{role:"user",content:[{type:"text",text:"[Request interrupted by user]"}]}}'
                        jq -cn --arg session "$session" \
                            '{type:"result",uuid:"scene-interrupted-result",subtype:"error_during_execution",is_error:true,errors:["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],session_id:$session}'
                        busy=false
                        if [ -s "$scene/waiting" ]; then scene_deliver_waiting; fi
                        continue
                    fi
                    busy=false
                    jq -cn --arg session "$session" --arg turn "$turn" \
                        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
                    ;;
            esac
            ;;
        user)
            uuid=$(printf '%s' "$frame" | jq -r '.uuid')
            if [ "$busy" = true ]; then
                # Admission can lag a message's arrival; Send now must wait for it.
                if [ "$subagent_wait" = 1 ] && [ -s "$config/fixture-admission-delay" ]; then
                    sleep "$(cat "$config/fixture-admission-delay")"
                fi
                jq -cn --arg uuid "$uuid" \
                    '{type:"command_lifecycle",command_uuid:$uuid,state:"queued"}'
                if [ "$subagent_wait" = 1 ]; then printf '%s\n' "$frame" >> "$scene/waiting"; fi
            else
                turn=$((turn + 1))
                persist_record "$frame"
                printf '%s\n' "$frame"
                jq -cn --arg uuid "$uuid" \
                    '{type:"command_lifecycle",command_uuid:$uuid,state:"started"}'
                jq -cn --arg session "$session" --arg model "$model" --arg version "$version" --argjson capabilities "$capabilities" \
                    '{type:"system",subtype:"init",session_id:$session,claude_code_version:$version,capabilities:$capabilities,model:$model}'
                if [ -n "$task_scene" ]; then
                    task_receipt_turn
                    busy=false
                    continue
                fi
                if [ "$subagent_wait" = 1 ] && [ "$turn" -eq 1 ]; then
                    scene_turn=$uuid
                    scene_subagents_start
                    if [ -e "$config/fixture-subagent-idle" ]; then
                        emit_record "$(jq -cn '{type:"assistant",uuid:"scene-idle",message:{id:"scene-idle",role:"assistant",content:[{type:"text",text:"The subagents are checking each area. I will report when they finish."}]}}')"
                        jq -cn --arg session "$session" \
                            '{type:"result",uuid:"scene-idle-result",subtype:"success",is_error:false,stop_reason:"end_turn",session_id:$session}'
                        continue
                    fi
                    if [ -e "$config/fixture-send-now-abort" ]; then
                        jq -cn '{type:"stream_event",event:{type:"message_start",message:{id:"scene-cut-off",role:"assistant"}}}'
                        jq -cn '{type:"stream_event",event:{type:"content_block_start",index:0,content_block:{type:"text",text:""}}}'
                        jq -cn '{type:"stream_event",event:{type:"content_block_delta",index:0,delta:{type:"text_delta",text:"Here is the plan so f"}}}'
                    else
                        jq -cn '{type:"assistant",uuid:"scene-bash",parent_tool_use_id:null,message:{id:"scene-bash",role:"assistant",content:[{type:"tool_use",id:"toolu_scene_bash",name:"Bash",input:{command:"until [ -e subagents-done ]; do sleep 1; done",description:"Wait for the subagents to finish"}}]}}'
                        jq -cn '{type:"system",subtype:"task_started",task_id:"bscene",tool_use_id:"toolu_scene_bash",description:"Wait for the subagents to finish",is_backgrounded:false,task_type:"local_bash"}'
                    fi
                    busy=true
                    continue
                fi
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
                if [ -f "$tools_file" ]; then tools_turn; fi
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
