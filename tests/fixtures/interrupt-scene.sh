#!/bin/bash
# One Claude conversation whose turns end three ways, for native evidence of
# how the app tells a turn the person stopped, a call that failed inside a
# turn that went on, and a turn that really failed apart, each shot as its
# turn ends:
#
#   stopped-call    the person stops a turn while its Bash call runs; Claude
#                   Code refuses the call, records the stop as a user
#                   message and ends the turn with an error result
#   failed-call     an Agent call Claude refuses past its limit of agents
#                   running at once, after which the turn answers normally;
#                   failed-call-hover and failed-call-open show its row's
#                   reason on hover and its call opened, where the row
#                   reads as an agent that couldn't start
#   request-failed  a turn that ends in an API error
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md),
# inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/interrupt-scene.sh
#   scripts/native-evidence stop
#
# From a cagents session, give `scripts/native-evidence --remote fw16 pair` a
# steps file that runs it as `CAGENTS_EVIDENCE_TASK=$task
# "$tool/tests/fixtures/interrupt-scene.sh"`.
#
# Both sides of a pair run the tool's synthetic Claude, which stops the way
# Claude Code 2.1.317 does. A state that does not appear within its wait is
# logged and the scene goes on, so a missing state is visible in the log and
# the shot.
set -u

here=$(cd "$(dirname "$0")" && pwd)
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

log() { printf '== %s %s\n' "$(date +%T)" "$*"; }
shot() {
    "$ne" drive hover 1,1 >/dev/null 2>&1
    sleep 1
    "$ne" shot "$1" >/dev/null && log "shot $1" || log "shot failed: $1"
}
# Whether the window shows text matching the extended regular expression $1.
shows() { "$ne" drive observe 2>/dev/null | grep -qE -- "$1"; }
# Wait up to $2 seconds for text matching $1.
await() {
    for _ in $(seq "$2"); do
        shows "$1" && return 0
        sleep 1
    done
    log "did not appear: $1"
    return 1
}
# The middle of a run of text matching $1, as X,Y, from observation $2: the
# first, or with $3 set to tail, the last.
place() {
    printf '%s\n' "$2" | grep -E -- "$1" | "${3:-head}" -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
# Start the conversation with prompt $1 and open it from its channel's row,
# which is named by the prompt's start, waiting for text matching $2. The
# screen reading splits a line into runs at whichever spaces it likes, so
# patterns name short runs.
converse() {
    "$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --prompt "$1" >/dev/null || log "spawn failed: $1"
    for _ in $(seq 20); do
        sleep 3
        "$ne" drive hover 1,1 >/dev/null 2>&1
        row=$(place "New channel|${1:0:12}" "$("$ne" drive observe 2>/dev/null)")
        [ -n "$row" ] || continue
        "$ne" drive click "$row" >/dev/null 2>&1
        sleep 2
        shows "$2" && return 0
    done
    log "the conversation did not open: $1"
    return 1
}
# Send prompt $1 from the composer and wait for text matching $2.
say() {
    local observed composer
    observed=$("$ne" drive observe 2>/dev/null)
    composer=$(place 'Message' "$observed" tail)
    # Where the hint cannot be read, the composer's input sits at a fixed place
    # in the window, whose size observe reports on its "window" line (an
    # older driver reported it as "output="). The point left of centre and
    # above the footer is inside the input, clear of the Stop control.
    [ -n "$composer" ] || composer=$(printf '%s\n' "$observed" |
        sed -nE -e 's/.*output=([0-9]+)x([0-9]+).*/\1 \2/p' \
            -e "s/^ *window .* ([0-9]+)x([0-9]+)$/\1 \2/p" | head -1 |
        awk '{printf "%d,%d", $1 * 0.45, $2 - 86}')
    "$ne" drive click "$composer" >/dev/null 2>&1
    "$ne" drive type "$1" >/dev/null 2>&1 || log "typing failed: $1"
    "$ne" drive key Return >/dev/null 2>&1
    await "$2" 30
}

mkdir -p "$config"
# Every first turn ends by itself shortly after its answer, unless a frame
# holds it.
printf '1\n' >"$config/fixture-first-turn-seconds"
"$ne" cli project open "$fixture/project" >/dev/null 2>&1 || log "project open failed"
sleep 2

# A call still running when the person stops the turn.
cat >"$config/fixture-frames.jsonl" <<'FRAMES'
{"type":"assistant","uuid":"tests-@TURN@","parent_tool_use_id":null,"message":{"id":"tests-@TURN@","role":"assistant","content":[{"type":"tool_use","id":"toolu_tests_@TURN@","name":"Bash","input":{"command":"cargo test --workspace","description":"Run the full test suite"}}]}}
{"_sleep":600}
FRAMES
printf '%s\n' 'The tests pass.' >"$config/fixture-reply.md"
if converse 'Run the full test suite before I merge.' 'cargo test|test suite|Run the full'; then
    sleep 3
    if ! "$ne" drive click 'Cancel active turn' >/dev/null 2>&1; then
        log "no stop control; pressing Escape in the composer"
        "$ne" drive key Escape >/dev/null 2>&1
    fi
    sleep 4
    shot stopped-call
else
    shot did-not-open
    exit 1
fi

# A call that fails inside a turn that goes on.
cat >"$config/fixture-frames.jsonl" <<'FRAMES'
{"type":"assistant","uuid":"review-@TURN@","parent_tool_use_id":null,"message":{"id":"review-@TURN@","role":"assistant","content":[{"type":"tool_use","id":"toolu_review_@TURN@","name":"Agent","input":{"description":"Review the change","prompt":"Review the change on this branch.","subagent_type":"general-purpose"}}]}}
{"type":"user","uuid":"review-result-@TURN@","parent_tool_use_id":null,"tool_use_result":"Error: Concurrent subagent limit reached.","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_review_@TURN@","is_error":true,"content":"Concurrent subagent limit reached. You can run 20 subagents at once. Do not retry. If the user wants more concurrent subagents, ask them to increase CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS."}]}}
FRAMES
printf '%s\n' 'I reviewed the change myself instead: it handles the empty list and the tests cover it.' >"$config/fixture-reply.md"
say 'Have a reviewer look over this change.' 'myself instead|handles the|reviewed the' &&
    sleep 3 && shot failed-call
# The refused agent's row gives its reason on hover, and pressing it shows
# the call in full; it is put back before the next turn.
refused=$(place "Couldn" "$("$ne" drive observe 2>/dev/null)")
if [ -n "$refused" ]; then
    "$ne" drive hover "$refused" >/dev/null 2>&1 || log "hover failed"
    sleep 3
    "$ne" shot failed-call-hover >/dev/null && log "shot failed-call-hover"
    "$ne" drive click "$refused" >/dev/null 2>&1 || log "opening the row failed"
    sleep 2
    shot failed-call-open
    "$ne" drive click "$refused" >/dev/null 2>&1 || log "closing the row failed"
    sleep 1
else
    log "no refused agent row"
fi

# A turn that fails.
cat >"$config/fixture-frames.jsonl" <<'FRAMES'
{"type":"assistant","uuid":"build-@TURN@","parent_tool_use_id":null,"message":{"id":"build-@TURN@","role":"assistant","content":[{"type":"text","text":"Checking the build now."}]}}
{"type":"result","uuid":"build-result-@TURN@","subtype":"success","is_error":true,"result":"API Error: 500 {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Internal server error\"}}"}
{"_end":true}
FRAMES
say 'Check that the build still works.' 'Checking the|build now' &&
    sleep 4 && shot request-failed
rm -f "$config/fixture-frames.jsonl"
