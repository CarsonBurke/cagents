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
resumed=
# The app's account check starts the provider without keeping a session. It
# is not a conversation, so the launch and stop switches leave it alone.
probe=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --no-session-persistence) probe=true ;;
        --session-id) shift; session=$1 ;;
        --session-id=*) session=${1#*=} ;;
        --resume) shift; session=$1; resumed=$1 ;;
        --resume=*) session=${1#*=}; resumed=$session ;;
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
# Optional launch record: while fixture-launches exists, each provider start
# appends the conversation it resumed, or `new`, so a check can tell a
# resume of the same conversation from a second start.
if [ "$probe" = false ] && [ -f "$config/fixture-launches" ]; then
    printf '%s\n' "${resumed:-new}" >> "$config/fixture-launches"
fi
# Optional resume failure: while fixture-fail-resume exists, the next resumed
# start exits before it answers, as a provider that cannot start would. The
# file is removed first, so the start after it resumes normally.
if [ "$probe" = false ] && [ -n "$resumed" ] && [ -e "$config/fixture-fail-resume" ]; then
    rm -f "$config/fixture-fail-resume"
    printf '%s\n' 'fixture: the resumed provider could not start' >&2
    exit 1
fi
project=$(pwd -P | jq -Rr 'gsub("[^a-zA-Z0-9]"; "-")')
history=$config/projects/$project/$session.jsonl
mkdir -p "$(dirname "$history")"
parent=null
turn=0
records='[]'
if [ -f "$history" ]; then
    # As Claude Code does, skip a line that isn't a record, such as one a
    # crash cut short, rather than refusing the whole transcript.
    records=$(jq -cnR '[inputs | fromjson? | objects]' "$history")
    parent=$(printf '%s' "$records" | jq -r 'last.uuid // null')
    # Count the prompts handled, not assistant records: a failed turn
    # writes none and a delivered one writes two.
    turn=$(printf '%s' "$records" | jq '[.[] | select(.type == "user" and (.origin.kind // "") != "task-notification"
        and (.isMeta // .isCompactSummary // .repeatsCompactSummary // false) != true
        and ((.message.content | type) == "string" or ((.message.content | type) == "array"
            and any(.message.content[]; .type != "tool_result"))))] | length')
    # A line cut short has no newline, and the next record must not join it.
    # Counted in bytes: a crash can leave NULs, which a shell string drops.
    if [ -s "$history" ] && [ "$(tail -c 1 -- "$history" | wc -l)" -eq 0 ]; then
        printf '\n' >>"$history"
    fi
fi
# Resume from an earlier main record to reproduce a provider branch without
# removing anything the person previously saw. Keep the full turn counter so
# later generated record IDs remain distinct on every branch.
if [ "$probe" = false ] && [ -n "$resumed" ] && [ -f "$config/fixture-resume-parent" ]; then
    resume_parent=$(cat "$config/fixture-resume-parent")
    if ! printf '%s' "$records" | jq -e --arg uuid "$resume_parent"         'any(.[]; .uuid == $uuid and (.isSidechain // false) != true and (.teamName // "") == "")' >/dev/null; then
        printf '%s\n' 'Invalid fixture-resume-parent: expected an existing main record UUID' >&2
        exit 2
    fi
    parent=$resume_parent
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

# `/compact` replaces the context with a summary as Claude Code does. The
# boundary starts a new parent chain, linked back to the turns it summarised
# only by logicalParentUuid; the wire folds the summary's transcript flags
# into isSynthetic. After the summary, the prompt is recorded as a command
# run in the terminal, which is the record that counts the turn on resume.
compact_turn() {
    boundary=fixture-compact-$turn
    jq -cn --arg uuid "$boundary" --arg session "$session" \
        '{type:"system",subtype:"compact_boundary",uuid:$uuid,session_id:$session,compact_metadata:{trigger:"manual",pre_tokens:48000}}'
    jq -cn --arg uuid "$boundary" --arg session "$session" --arg previous "$parent" \
        '{type:"system",subtype:"compact_boundary",uuid:$uuid,parentUuid:null,logicalParentUuid:(if $previous == "null" then null else $previous end),sessionId:$session,content:"Conversation compacted",compactMetadata:{trigger:"manual",preTokens:48000}}' >> "$history"
    parent=$boundary
    summary="This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.

Summary:
1. The submission logs were compared with the last successful run.
2. The failure is in the scoring step; nothing has been changed yet."
    persist_record "$(jq -cn --arg turn "$turn" --arg text "$summary" '{type:"user",uuid:("fixture-compact-summary-"+$turn),isCompactSummary:true,isVisibleInTranscriptOnly:true,message:{role:"user",content:$text}}')"
    jq -cn --arg turn "$turn" --arg text "$summary" \
        '{type:"user",uuid:("fixture-compact-summary-"+$turn),isSynthetic:true,message:{role:"user",content:$text}}'
    # After the summary the transcript records the command as one run
    # directly in the terminal, with its output.
    persist_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("fixture-compact-caveat-"+$turn),isMeta:true,message:{role:"user",content:"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands. DO NOT respond to these messages or otherwise consider them in your response unless the user explicitly asks you to.</local-command-caveat>"}}')"
    persist_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("fixture-compact-command-"+$turn),message:{role:"user",content:"<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>"}}')"
    persist_record "$(jq -cn --arg turn "$turn" '{type:"user",uuid:("fixture-compact-output-"+$turn),repeatsCompactSummary:true,message:{role:"user",content:"<local-command-stdout>Compacted </local-command-stdout>"}}')"
    jq -cn --arg session "$session" --arg turn "$turn" \
        '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
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

# Optional subagent tabs: one named working agent, a waiting one, a failed
# one and two finished agents that share a type, so a check can see each
# state's mark, the working-first order and how a shared name is told apart.
subagents_turn() {
    agent() {
        emit_record "$(jq -cn --arg turn "$turn" --arg id "$1" --argjson input "$2" '{type:"assistant",uuid:("fixture-"+$id+"-"+$turn),message:{id:("fixture-"+$id+"-"+$turn),role:"assistant",content:[{type:"tool_use",id:($id+"-"+$turn),name:"Agent",input:$input}]}}')"
    }
    # A background agent runs as a task, which Stop ends by its task ID.
    started() {
        emit_record "$(jq -cn --arg turn "$turn" --arg id "$1" --arg description "$2" '{type:"system",subtype:"task_started",uuid:("fixture-"+$id+"-start-"+$turn),task_id:("fixture-task-"+$id+"-"+$turn),tool_use_id:($id+"-"+$turn),task_type:"local_agent",description:$description}')"
        background_agents="$background_agents $1-$turn"
    }
    settle() {
        emit_record "$(jq -cn --arg turn "$turn" --arg id "$1" --arg result "$2" --argjson error "$3" '{type:"user",uuid:("fixture-"+$id+"-result-"+$turn),message:{role:"user",content:[{type:"tool_result",tool_use_id:($id+"-"+$turn),is_error:$error,content:$result}]}}')"
    }
    agent explore-logs '{"subagent_type":"Explore","description":"Read the failing submission logs"}'
    settle explore-logs 'The submission timed out while scoring.' false
    agent scout '{"name":"Scout","subagent_type":"Explore","description":"Map the submission pipeline","run_in_background":true}'
    started scout 'Map the submission pipeline'
    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("fixture-scout-answer-"+$turn),parent_tool_use_id:("scout-"+$turn),message:{id:("fixture-scout-answer-"+$turn),role:"assistant",content:[{type:"text",text:"Reading the submission script and its scoring step."}]}}')"
    agent planner '{"name":"Planner","subagent_type":"Plan","description":"Plan a retry that fits the time limit","run_in_background":true}'
    started planner 'Plan a retry that fits the time limit'
    jq -cn --arg turn "$turn" '{type:"stream_event",parent_tool_use_id:("planner-"+$turn),event:{type:"error",error:{type:"rate_limit_error",message:"Waiting for the rate limit to reset"}}}'
    agent builder '{"name":"Builder","subagent_type":"general-purpose","description":"Rebuild the submission archive"}'
    settle builder 'The archive step failed: disk quota exceeded.' true
    agent explore-history '{"subagent_type":"Explore","description":"Compare with the last successful version"}'
    settle explore-history 'The last good run used a smaller model.' false
}

# Claude Code reports each background agent that a Stop ended.
background_agents=
stop_agent() {
    emit_record "$(jq -cn --arg tool "$1" '{type:"system",subtype:"task_notification",uuid:("fixture-"+$tool+"-stopped"),task_id:("fixture-task-"+$tool),tool_use_id:$tool,status:"stopped",summary:"Agent stopped"}')"
    remaining=
    for known in $background_agents; do
        [ "$known" = "$1" ] || remaining="$remaining $known"
    done
    background_agents=$remaining
}

# Optional scripted tool calls: each line of fixture-tools.jsonl is
# {"name":..., "input":{...}, "result":"...", "is_error":bool}, emitted as a
# tool call and its result on every turn, so a rendering check can choose the
# exact calls and outputs it needs. A spec with "stream": SECONDS first
# streams its input as Claude does, in input_json_delta fragments that many
# seconds apart, so the native UI's partial states can be observed.
tools_file=$config/fixture-tools.jsonl
tools_turn() {
    call=0
    while IFS= read -r spec; do
        [ -n "$spec" ] || continue
        call=$((call + 1))
        id="tool-$turn-$call"
        delay=$(printf '%s' "$spec" | jq -r '.stream // empty')
        if [ -n "$delay" ]; then
            jq -cn --arg id "fixture-$id" '{type:"stream_event",event:{type:"message_start",message:{id:$id,role:"assistant"}}}'
            printf '%s' "$spec" | jq -c --arg id "$id" '{type:"stream_event",event:{type:"content_block_start",index:0,content_block:{type:"tool_use",id:$id,name:.name,input:{}}}}'
            printf '%s' "$spec" | jq -c '.input | tojson | [range(0; length; 12) as $at | .[$at:$at + 12]] | .[]' |
                while IFS= read -r delta; do
                    jq -cn --argjson delta "$delta" '{type:"stream_event",event:{type:"content_block_delta",index:0,delta:{type:"input_json_delta",partial_json:$delta}}}'
                    sleep "$delay"
                done
            jq -cn '{type:"stream_event",event:{type:"content_block_stop",index:0}}'
            jq -cn '{type:"stream_event",event:{type:"message_stop"}}'
        fi
        emit_record "$(printf '%s' "$spec" | jq -c --arg id "$id" '{type:"assistant",uuid:("fixture-"+$id),message:{id:("fixture-"+$id),role:"assistant",content:[{type:"tool_use",id:$id,name:.name,input:.input}]}}')"
        emit_record "$(printf '%s' "$spec" | jq -c --arg id "$id" '{type:"user",uuid:("fixture-"+$id+"-result"),message:{role:"user",content:[{type:"tool_result",tool_use_id:$id,is_error:(.is_error // false),content:(.result // "")}]}}')"
    done < "$tools_file"
}

# Optional raw stream: each line of fixture-frames.jsonl is one stream-json
# frame, emitted in order on every turn before its answer, with "@TURN@" in any
# string replaced by the turn number so IDs stay unique. Assistant and user
# frames are also recorded in the synthetic history, as Claude records them;
# system frames, such as a background task's lifecycle, are live only. A frame
# with "_history": true is only recorded, as Claude records a queued
# notification. A frame with "_live": true is only emitted, as a subagent's
# frames are, which Claude records in the subagent's own transcript. A line
# {"_sleep": SECONDS} pauses, so a running state can be observed. A line
# {"_end": true} ends the turn there: the frames supply its closing result,
# such as a failed one, and the fixture adds no answer of its own. A line
# {"_exit": STATUS} ends the provider there, as a crash does, mid-turn and
# without a result; with "_once": true each such line does so only once,
# which fixture-frames.exited records by line number, so the resumed
# provider goes on.
frames_file=$config/fixture-frames.jsonl
frames_ended=0
frames_turn() {
    frames_ended=0
    frame_line=0
    while IFS= read -r spec; do
        frame_line=$((frame_line + 1))
        [ -n "$spec" ] || continue
        delay=$(printf '%s' "$spec" | jq -r '._sleep // empty')
        if [ -n "$delay" ]; then
            sleep "$delay"
            continue
        fi
        if [ "$(printf '%s' "$spec" | jq -r '._end // false')" = true ]; then
            frames_ended=1
            return
        fi
        status=$(printf '%s' "$spec" | jq -r '._exit // empty')
        if [ -n "$status" ]; then
            case $status in
                [0-9] | [1-9][0-9] | 1[0-9][0-9] | 2[0-4][0-9] | 25[0-5]) ;;
                *)
                    printf '%s\n' "fixture: line $frame_line of $frames_file: _exit must be a status from 0 to 255, not $status" >&2
                    exit 2
                    ;;
            esac
            if [ "$(printf '%s' "$spec" | jq -r '._once // false')" = true ]; then
                # The lines that have exited once, by number.
                ! grep -qxF "$frame_line" "$config/fixture-frames.exited" 2>/dev/null || continue
                printf '%s\n' "$frame_line" >>"$config/fixture-frames.exited"
            fi
            printf '%s\n' "fixture: the provider exited with status $status mid-turn" >&2
            exit "$status"
        fi
        frame=$(printf '%s' "$spec" | jq -c --arg turn "$turn" 'walk(if type == "string" then gsub("@TURN@"; $turn) else . end)')
        if [ "$(printf '%s' "$frame" | jq -r '._history // false')" = true ]; then
            printf '%s' "$frame" | jq -c --arg session "$session" 'del(._history) + {sessionId:$session}' >> "$history"
            continue
        fi
        if [ "$(printf '%s' "$frame" | jq -r '._live // false')" = true ]; then
            printf '%s\n' "$frame" | jq -c 'del(._live)'
            continue
        fi
        case $(printf '%s' "$frame" | jq -r '.type') in
            assistant|user) emit_record "$frame" ;;
            *) printf '%s\n' "$frame" ;;
        esac
    done < "$frames_file"
}

# Optional real command: every turn runs the first line of
# fixture-run-command as a Bash call, in a child of this provider process as
# Claude's Bash tool does, and reports its output and exit code. Checks of
# where a provider's commands run, and of a command the system stops, use it.
run_file=$config/fixture-run-command
run_turn() {
    id="run-$turn"
    command=$(head -n 1 "$run_file")
    emit_record "$(jq -cn --arg id "$id" --arg command "$command" '{type:"assistant",uuid:("fixture-"+$id),message:{id:("fixture-"+$id),role:"assistant",content:[{type:"tool_use",id:$id,name:"Bash",input:{command:$command,description:"Run the fixture command"}}]}}')"
    status=0
    output=$(sh -c "$command" 2>&1 </dev/null) || status=$?
    emit_record "$(jq -cn --arg id "$id" --arg output "$output" --argjson status "$status" '{type:"user",uuid:("fixture-"+$id+"-result"),message:{role:"user",content:[{type:"tool_result",tool_use_id:$id,is_error:($status != 0),content:(if $status == 0 then $output else ($output + (if $output == "" then "" else "\n" end) + "Exit code \($status)") end)}]}}')"
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
subagents=0
if fixture_switch "${CAGENTS_FIXTURE_SUBAGENTS:-0}" fixture-subagents; then subagents=1; fi
# The switch file may hold the delay between deltas in seconds.
stream_reply=0
stream_delay=0.25
if fixture_switch "${CAGENTS_FIXTURE_STREAM_REPLY:-0}" fixture-stream-reply; then
    stream_reply=1
    if [ -s "$config/fixture-stream-reply" ]; then stream_delay=$(head -n 1 "$config/fixture-stream-reply"); fi
fi

# Optional slow start: while fixture-slow-start exists, a conversation's
# provider waits before it reads anything, as a cold start does: the file's
# first line in seconds, or 5. The account check is not held up.
if [ "$probe" = false ] && [ -e "$config/fixture-slow-start" ]; then
    start_delay=5
    if [ -s "$config/fixture-slow-start" ]; then start_delay=$(head -n 1 "$config/fixture-slow-start"); fi
    case $start_delay in
        '' | *[!0-9.]* | *.*.* | .)
            printf '%s\n' "fixture: fixture-slow-start must hold seconds, such as 5 or 1.5, not '$start_delay'" >&2
            exit 2
            ;;
    esac
    sleep "$start_delay"
fi

# Keep the delayed text writer self-contained: native evidence and Rust tests
# copy this shell file alone. The awaited helper owns stdout only while the
# shell waits; incoming frames return to the existing control/history handlers.
stream_directory=
stream_pid=
stream_active=false
cleanup_stream() {
    if [ -n "$stream_pid" ]; then
        kill -TERM "$stream_pid" 2>/dev/null || :
        wait "$stream_pid" 2>/dev/null || :
        stream_pid=
    fi
    if [ -n "$stream_directory" ]; then
        rm -rf -- "$stream_directory"
        stream_directory=
    fi
}
trap cleanup_stream EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

next_frame() {
    if [ -n "$stream_directory" ] && [ -s "$stream_directory/pending" ]; then
        if IFS= read -r frame < "$stream_directory/pending"; then
            tail -n +2 "$stream_directory/pending" > "$stream_directory/pending.next"
            mv "$stream_directory/pending.next" "$stream_directory/pending"
            return 0
        fi
        # A raw read can include the start of the following frame. Join that
        # prefix to stdin rather than losing it or parsing an incomplete JSON.
        stream_input_prefix=$frame
        : > "$stream_directory/pending"
        IFS= read -r frame || return 1
        frame=$stream_input_prefix$frame
        return 0
    fi
    IFS= read -r frame
}

stream_step() {
    /usr/bin/python3 - "$stream_directory" "$stream_delay" "$$" 3<&0 <<'PYTHON' &
import ctypes
import json
import os
from pathlib import Path
import selectors
import signal
import sys
import time

root = Path(sys.argv[1])
delay = float(sys.argv[2])
if not 0 <= delay < float("inf"):
    raise ValueError("fixture-stream-reply must hold finite nonnegative seconds")
parent = int(sys.argv[3])
# The helper creates no children. Even an uncatchable shell death must not
# leave an orphan scheduling frames after its owning fixture has gone away.
if ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGTERM, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "set fixture helper parent-death signal")
if os.getppid() != parent:
    sys.exit(143)
state_file = root / "state.json"
state = json.loads(state_file.read_text()) if state_file.exists() else {
    "offset": 0, "next_at": time.monotonic()}
text = (root / "reply").read_text()
pending = root / "pending"
buffer = pending.read_bytes()
pending.write_bytes(b"")
selector = selectors.DefaultSelector()
selector.register(3, selectors.EVENT_READ)
input_open = True
status = 0
try:
    while True:
        if b"\n" in buffer:
            line = buffer.split(b"\n", 1)[0]
            value = json.loads(line)
            # Replay the entire buffered input, including this frame, through
            # the shell. Only interrupt cancels the remaining scheduled text.
            status = 10 if (value.get("type") == "control_request" and
                            value.get("request", {}).get("subtype") == "interrupt") else 11
            break
        now = time.monotonic()
        timeout = max(0, state["next_at"] - now)
        ready = selector.select(timeout) if input_open else []
        if ready:
            data = os.read(3, 65536)
            if data:
                buffer += data
            else:
                selector.unregister(3)
                input_open = False
            continue
        if not input_open and timeout:
            time.sleep(timeout)
        if state["offset"] >= len(text):
            break
        delta = text[state["offset"]:state["offset"] + 16384]
        print(json.dumps({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": delta}}},
            ensure_ascii=False, separators=(",", ":")), flush=True)
        with (root / "prefix").open("a") as prefix:
            prefix.write(delta)
        state["offset"] += len(delta)
        state["next_at"] = time.monotonic() + delay
finally:
    selector.close()
    pending.write_bytes(buffer)
    state_file.write_text(json.dumps(state))
sys.exit(status)
PYTHON
    stream_pid=$!
    if wait "$stream_pid"; then stream_status=0; else stream_status=$?; fi
    stream_pid=
    return "$stream_status"
}

finish_reply() {
    assistant=$(jq -cn --arg turn "$turn" --rawfile text "$reply" \
        --arg default "I’m checking the submission logs and comparing the failed run with the last successful version." \
        '{type:"assistant",uuid:("fixture-message-"+$turn),message:{id:("fixture-message-"+$turn),role:"assistant",content:[{type:"text",text:(if $text == "" then $default else $text end)}],usage:{input_tokens:4200,output_tokens:180}}}')
    persist_record "$assistant"
    printf '%s\n' "$assistant"
}

advance_stream() {
    if stream_step; then stream_status=0; else stream_status=$?; fi
    case $stream_status in
        11) return ;; # Let the existing handler serve the yielded input first.
        0|10) ;;
        *) exit "$stream_status" ;;
    esac
    stream_active=false
    jq -cn '{type:"stream_event",event:{type:"content_block_stop",index:0}}'
    if [ "$stream_status" -eq 10 ]; then
        reply=$stream_directory/prefix
        if [ -s "$reply" ]; then finish_reply; fi
        # The pending interrupt is still handled by the unchanged dispatcher.
        return
    fi
    reply=$stream_directory/reply
    finish_reply
    if [ "$turn" -eq 1 ]; then
        busy=true
    else
        busy=false
        jq -cn --arg session "$session" --arg turn "$turn" \
            '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
    fi
}

busy=false
fast=off
model=haiku
if [ "$fast_fixture" = 1 ]; then model=opus; fi
while next_frame; do
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
                # Optional advertised commands and skills: a JSON object with
                # `commands` ([{name, description}]) and `skills` ([name]).
                if [ -f "$config/fixture-commands.json" ]; then
                    metadata=$(jq -cn --argjson metadata "$metadata" --slurpfile extra "$config/fixture-commands.json" '$metadata + $extra[0]')
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
                stop_task)
                    task=$(printf '%s' "$frame" | jq -r '.request.task_id')
                    case $task in
                        fixture-task-*) stop_agent "${task#fixture-task-}" ;;
                    esac
                    ;;
                interrupt)
                    busy=false
                    for tool in $background_agents; do stop_agent "$tool"; done
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
            elif [ "$(printf '%s' "$frame" | jq -r '.message.content | if type == "array" then map(.text // "") | join("") else . end')" = /compact ]; then
                turn=$((turn + 1))
                compact_turn
            else
                turn=$((turn + 1))
                persist_record "$frame"
                printf '%s\n' "$frame"
                jq -cn --arg uuid "$uuid" \
                    '{type:"command_lifecycle",command_uuid:$uuid,state:"started"}'
                jq -cn --arg session "$session" --arg model "$model" \
                    '{type:"system",subtype:"init",session_id:$session,claude_code_version:"2.1.286",model:$model}'
                if [ -n "$task_scene" ]; then
                    task_receipt_turn
                    busy=false
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
                # Optional provider death mid-turn, as when the system runs
                # out of memory: the next prompt gets the start of an answer,
                # then the provider kills itself with SIGKILL. The switch is
                # consumed first, so a resumed provider answers normally.
                if [ "$probe" = false ] && [ -e "$config/fixture-stop-on-prompt" ]; then
                    rm -f "$config/fixture-stop-on-prompt"
                    emit_record "$(jq -cn --arg turn "$turn" '{type:"assistant",uuid:("fixture-stopping-"+$turn),message:{id:("fixture-stopping-"+$turn),role:"assistant",content:[{type:"text",text:"Starting the full evaluation run now; this step needs a lot of memory."}]}}')"
                    kill -KILL $$
                fi
                if [ "$verbosity" = 1 ]; then verbosity_turn; fi
                if [ "$subagents" = 1 ]; then subagents_turn; fi
                if [ -f "$tools_file" ]; then tools_turn; fi
                if [ -f "$frames_file" ]; then
                    frames_turn
                    if [ "$frames_ended" = 1 ]; then continue; fi
                fi
                if [ -s "$run_file" ]; then run_turn; fi
                if [ -f "$fixture_image" ]; then image_turn; fi
                # A Markdown file in the config directory can stand in for the
                # answer, so rendering and long-transcript checks need no live
                # provider.
                # jq reads the file itself: an answer over 128 KiB exceeds the
                # kernel's limit for a single command-line argument.
                reply=$config/fixture-reply.md
                [ -f "$reply" ] || reply=/dev/null
                # Optionally stream the answer as Claude does, in text deltas
                # before the final record, so a long answer grows across cuts.
                if [ "$stream_reply" = 1 ] && [ -s "$reply" ]; then
                    if [ -z "$stream_directory" ]; then
                        stream_directory=$(mktemp -d "$config/.fixture-stream.XXXXXX")
                        : > "$stream_directory/pending"
                    fi
                    cp "$reply" "$stream_directory/reply"
                    : > "$stream_directory/prefix"
                    rm -f "$stream_directory/state.json"
                    busy=true
                    stream_active=true
                    jq -cn --arg id "fixture-message-$turn" '{type:"stream_event",event:{type:"message_start",message:{id:$id,role:"assistant"}}}'
                    jq -cn '{type:"stream_event",event:{type:"content_block_start",index:0,content_block:{type:"text",text:""}}}'
                else
                    finish_reply
                    if [ "$turn" -eq 1 ]; then
                        busy=true
                    else
                        jq -cn --arg session "$session" --arg turn "$turn" \
                            '{type:"result",uuid:("fixture-result-"+$turn),subtype:"success",is_error:false,session_id:$session}'
                    fi
                fi
            fi
            ;;
    esac
    if [ "$stream_active" = true ]; then advance_stream; fi
done
