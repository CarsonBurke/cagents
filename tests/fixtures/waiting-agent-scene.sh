#!/bin/bash
# A Claude conversation whose background agent, "Review restart safety",
# reports that it is waiting on its own background work, for native evidence
# of how the app reads an agent's text notices.
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md),
# inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/waiting-agent-scene.sh states   # or stop, or narrow
#   scripts/native-evidence stop
#
# Each scene starts a synthetic Claude agent (tests/fixtures/chat-composer-claude.sh)
# whose first turn starts the agent in the background and ends, opens its
# conversation, then sends one prompt per step. Before each prompt it writes
# the step's frames from waiting-agent-scene/ as the provider's
# fixture-frames.jsonl, so that turn delivers the step's notice:
#
#   waiting    the agent is waiting on its own background work (Claude Code
#              2.1.317's wording; 2.1.296's is recorded in claude-*-waiting-*)
#   resumed    a notice without a status: the agent was resumed
#   finished   a repeated waiting notice, then the final one
#   failed     a failure notice after the final one
#   elsewhere  a second agent is resumed by another agent
#
# Scenes, each shooting with the side's prefix as native-evidence does:
#
#   states  waiting-card, waiting-tab, resumed-working, finished,
#           failed-after-finished and resumed-elsewhere
#   stop    waiting-tab, then Stop on that tab: stopped-tab and
#           stopped-conversation. The fixture answers the stop as Claude
#           Code does for a waiting agent: a killed patch and no notice.
#   narrow  narrow-waiting-card and narrow-waiting-tab
#
# A step that does not show its reply within 25 seconds is logged and the
# scene goes on, so a missing state is visible in the log and the shot.
set -u

scene=${1:?usage: waiting-agent-scene.sh states|stop|narrow}
here=$(cd "$(dirname "$0")" && pwd)
frames=$here/waiting-agent-scene
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

log() { printf '== %s %s %s\n' "$(date +%T)" "$scene" "$*"; }
# The centre of the last observed text run whose text matches the extended
# regular expression $1, as X,Y.
locate() {
    "$ne" drive observe 2>/dev/null | grep -E -- "$1" | tail -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
click() {
    local at
    at=$(locate "$1")
    if [ -z "$at" ]; then
        log "not found: $1"
        return 1
    fi
    "$ne" drive click "$at" >/dev/null 2>&1
    sleep 2
}
shot() {
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    "$ne" shot "$1" >/dev/null && log "shot $1" || log "shot failed: $1"
}
# step FRAMES PROMPT REPLY: send PROMPT on a turn that delivers FRAMES.
step() {
    cp "$frames/$1.jsonl" "$config/fixture-frames.jsonl"
    printf '%s\n' "$3" >"$config/fixture-reply.md"
    "$ne" drive click "$composer" >/dev/null 2>&1
    "$ne" drive type "$2" >/dev/null 2>&1
    "$ne" drive key Return >/dev/null 2>&1
    # Text recognition can split a line, so wait on the reply's last words.
    "$ne" drive wait "$(printf '%s' "$3" | awk '{print $(NF - 1), $NF}')" --timeout 25 >/dev/null 2>&1 || log "reply not seen: $3"
    sleep 3
}

mkdir -p "$config"
cp "$frames/launch.jsonl" "$config/fixture-frames.jsonl"
: >"$config/fixture-reply.md"
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$fixture/project" --prompt 'Check the retry limits.' >/dev/null ||
    log "spawn failed"
# The agent's conversation opens from its channel's row in the sidebar, once
# its first turn has said the reviewer is running. The row's own tooltip
# repeats its name, so the first match is the row.
composer=
for _ in 1 2 3 4 5 6 7 8; do
    sleep 4
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    row=$("$ne" drive observe 2>/dev/null | grep -E 'New channel|Check the retry' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null 2>&1
    sleep 3
    observed=$("$ne" drive observe 2>/dev/null)
    printf '%s\n' "$observed" | grep -q 'reviewer is running' || continue
    # The composer's placeholder. A narrow window can cut it short; the
    # composer then is still the wide field along the window's foot.
    composer=$(printf '%s\n' "$observed" | grep -E 'Message' | tail -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$composer" ] || composer=$(printf '%s\n' "$observed" |
        sed -nE 's/.*output=([0-9]+)x([0-9]+).*/\1 \2/p' | awk '{printf "%d,%d", $1 * 0.65, $2 - 67}')
    break
done
if [ -z "$composer" ]; then
    log "the agent's conversation did not open"
    exit 1
fi

prefix=
[ "$scene" = narrow ] && prefix=narrow-
step waiting 'How is the review going?' 'Noted the interim report.'
[ "$scene" = stop ] || shot "${prefix}waiting-card"
click ' reviewer$'
shot "${prefix}waiting-tab"
case $scene in
    narrow) ;;
    stop)
        click ' Stop$'
        "$ne" drive wait 'Stopped' --timeout 15 >/dev/null 2>&1 || log "Stopped not seen"
        shot stopped-tab
        click 'Conversation'
        shot stopped-conversation
        ;;
    states)
        click 'Conversation'
        step resumed 'Is it moving again?' 'The reviewer picked the work back up.'
        shot resumed-working
        step finished 'Any result yet?' 'The reviewer reported back.'
        shot finished
        step failed 'Anything changed?' 'The reviewer corrected its report.'
        shot failed-after-finished
        step elsewhere 'Trace the retry path.' 'The trace went to another agent.'
        shot resumed-elsewhere
        ;;
    *) log "unknown scene"; exit 2 ;;
esac
log done
