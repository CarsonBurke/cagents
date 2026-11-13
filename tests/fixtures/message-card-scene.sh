#!/bin/bash
# A Claude conversation that starts an agent in the background and then
# messages it with Claude Code's own SendMessage tool, for native evidence of
# how the app draws a message from one agent to another.
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md),
# inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/message-card-scene.sh cards   # or narrow
#   scripts/native-evidence stop
#
# From a cagents session, give `scripts/native-evidence --remote fw16 pair` a
# steps file that runs it as `CAGENTS_EVIDENCE_TASK=$task
# "$tool/tests/fixtures/message-card-scene.sh" cards`.
#
# The conversation's first turn replays message-card-scene/frames.jsonl, as
# Claude Code 2.1 streams it: an Agent call started in the background
# ("Relocate the repo", agent ID a1892b93e88999a7d), then four SendMessage
# calls and the JSON each returned:
#
#   moved        a short message to that agent, queued for its next step
#   steps        a long message to it, which the card folds
#   unreachable  a message to a name no agent has, which Claude refuses
#   earlier      a message that resumed an agent this conversation does not
#                list, as one an earlier transcript started
#
# Scenes, each shooting with the side's prefix as native-evidence does:
#
#   cards   message-cards (the started agent and the first messages),
#           message-hover (the pointer on the long message, its tooltip
#           open), message-menu (that message's context menu) and
#           message-failed (the refused message and the one to an unlisted
#           agent)
#   narrow  narrow-message-cards and narrow-message-failed, for a session
#           started at --width 900, the narrowest window the app allows
#
# A state that does not appear within 30 seconds is logged and the scene goes
# on, so a missing state is visible in the log and the shot.
set -u

scene=${1:?usage: message-card-scene.sh cards|narrow}
here=$(cd "$(dirname "$0")" && pwd)
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
# Click the control labelled $1, which the harness finds through the app's
# accessibility tree as well as the screen. A fold's button is only on the
# screen, whose reading may split its label, so name the part read whole.
click() {
    if ! "$ne" drive click "$1" >/dev/null 2>&1; then
        log "not found: $1"
        return 1
    fi
    sleep 2
}
shot() {
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    "$ne" shot "$1" >/dev/null && log "shot $1" || log "shot failed: $1"
}
# Scroll the transcript under the pointer at $1 by $2 wheel steps (negative
# scrolls up), then let it settle.
scroll() {
    "$ne" drive scroll --at "$1" v -- "$2" >/dev/null 2>&1 || log "scroll failed"
    sleep 2
}

mkdir -p "$config"
cp "$here/message-card-scene/frames.jsonl" "$config/fixture-frames.jsonl"
printf '%s\n' 'I told the relocation agent where the repo went.' >"$config/fixture-reply.md"
# A fresh fixture has no project open yet; opening one again is harmless.
"$ne" cli project open "$fixture/project" >/dev/null 2>&1 || log "project open failed"
sleep 2
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$fixture/project" --prompt 'The t3code repo moved; tell the agent working in it.' >/dev/null ||
    log "spawn failed"
# The conversation opens from its channel's row in the sidebar once its turn
# has answered.
opened=
for _ in 1 2 3 4 5 6 7 8; do
    sleep 4
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    row=$("$ne" drive observe 2>/dev/null | grep -E 'New channel|t3code repo moved' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null 2>&1
    sleep 3
    "$ne" drive observe 2>/dev/null | grep -q 'where the repo went' || continue
    opened=1
    break
done
if [ -z "$opened" ]; then
    log "the conversation did not open"
    exit 1
fi

prefix=
[ "$scene" = narrow ] && prefix=narrow-
case $scene in
    cards | narrow) ;;
    *) log "unknown scene"; exit 2 ;;
esac
# The transcript opens at its end; the first message is above it.
transcript=$(locate 'where the repo went')
scroll "$transcript" -6
shot "${prefix}message-cards"
if [ "$scene" = cards ]; then
    # The long message, by the start of its text.
    long=$(locate 'The checkout moved')
    if [ -n "$long" ]; then
        "$ne" drive move "$long" >/dev/null 2>&1 || log "hover failed"
        sleep 3
        "$ne" shot message-hover >/dev/null && log "shot message-hover" || log "shot failed: message-hover"
        "$ne" drive click --button right "$long" >/dev/null 2>&1 || log "right click failed"
        sleep 2
        "$ne" shot message-menu >/dev/null && log "shot message-menu" || log "shot failed: message-menu"
        "$ne" drive key Escape >/dev/null 2>&1 || :
        sleep 1
    else
        log "not found: the long message"
    fi
fi
scroll "$transcript" 30
shot "${prefix}message-failed"
log done
