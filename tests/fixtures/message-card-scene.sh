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
#           open), message-menu (that message's context menu),
#           message-refused-hover (the refused message's tooltip) and
#           message-failed (the refused message and the one to an unlisted
#           agent)
#   narrow  narrow-message-cards and narrow-message-failed, for a session
#           started at --width 900, the narrowest window the app allows
#   long    long-message-cards and long-message-failed: the agent named by a
#           task longer than a row shows and a message that never confirmed,
#           for a session at --width 1200, whose side panels leave the chat
#           its narrowest
#   keys    the agent named by the long task, at --width 1200: long-name-hover
#           (the first message's tooltip, naming the agent whole), then the
#           rows by keyboard: keyboard-focus (the long
#           message focused), keyboard-menu (its menu from the Menu key),
#           keyboard-refused-open and keyboard-refused-closed (Enter twice
#           on the refused message, its detail - the message in full, then
#           its tool call - opening and closing while the row keeps focus)
#           and keyboard-enter (Enter on the started agent, which opens it)
#
# A state that does not appear within 30 seconds is logged and the scene goes
# on, so a missing state is visible in the log and the shot.
set -u

scene=${1:?usage: message-card-scene.sh cards|narrow|long|keys}
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
if [ "$scene" = long ]; then
    # The agent goes by a task longer than a row shows, and the first message
    # to it never hears back, so its row says it was not confirmed.
    sed -e 's/"Relocate the repo"/"Relocate the t3code checkout and every worktree"/g' \
        -e '/fixture-moved-@TURN@-result/d' \
        "$here/message-card-scene/frames.jsonl" >"$config/fixture-frames.jsonl"
elif [ "$scene" = keys ]; then
    # The same long name, every message confirmed.
    sed -e 's/"Relocate the repo"/"Relocate the t3code checkout and every worktree"/g' \
        "$here/message-card-scene/frames.jsonl" >"$config/fixture-frames.jsonl"
else
    cp "$here/message-card-scene/frames.jsonl" "$config/fixture-frames.jsonl"
fi
printf '%s\n' 'I told the relocation agent where the repo went.' >"$config/fixture-reply.md"
# The reply as the screen reading finds it: it splits a line into runs at
# whichever spaces it likes, so any of the reply's distinctive runs will do.
reply='where the repo|repo went|relocation agent|told the relocation'
# A fresh fixture has no project open yet; opening one again is harmless.
"$ne" cli project open "$fixture/project" >/dev/null 2>&1 || log "project open failed"
sleep 2
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$fixture/project" --prompt 'The t3code repo moved; tell the agent working in it.' >/dev/null ||
    log "spawn failed"
# The conversation opens from its channel's row in the sidebar once its turn
# has answered.
opened=
# A busy host can take a minute or two to answer the first turn.
for _ in $(seq 20); do
    sleep 4
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    row=$("$ne" drive observe 2>/dev/null | grep -E 'New channel|t3code repo moved' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null 2>&1
    sleep 3
    "$ne" drive observe 2>/dev/null | grep -qE "$reply" || continue
    opened=1
    break
done
if [ -z "$opened" ]; then
    # What the window showed instead, for whoever reads the log.
    "$ne" shot "${scene}-did-not-open" >/dev/null 2>&1
    log "the conversation did not open"
    exit 1
fi

prefix=
[ "$scene" = narrow ] && prefix=narrow-
[ "$scene" = long ] && prefix=long-
case $scene in
    cards | narrow | long | keys) ;;
    *) log "unknown scene"; exit 2 ;;
esac
# The transcript opens at its end; the first message is above it.
transcript=$(locate "$reply")
if [ "$scene" = keys ]; then
    key() {
        "$ne" drive key "$1" >/dev/null 2>&1 || log "key failed: $1"
        sleep 0.5
    }
    # The reply is plain text, so a click there leaves focus where the
    # window's traversal starts again: one step back is the tab strip's new
    # tab button, and the rows follow it after the conversation's tab and
    # its terminal button.
    # The first message's tooltip names the agent its row cuts short.
    first=$(locate 'The repo is now')
    if [ -n "$first" ]; then
        "$ne" drive move "$first" >/dev/null 2>&1
        sleep 3
        "$ne" shot long-name-hover >/dev/null && log "shot long-name-hover" ||
            log "shot failed: long-name-hover"
    else
        log "not found: the first message"
    fi
    "$ne" drive click "$transcript" >/dev/null 2>&1
    sleep 1
    key shift+Tab
    for _ in 1 2 3 4 5; do key Tab; done
    shot keyboard-focus
    key Menu
    shot keyboard-menu
    key Escape
    key Tab
    key Return
    shot keyboard-refused-open
    key Return
    shot keyboard-refused-closed
    for _ in 1 2 3; do key shift+Tab; done
    key Return
    sleep 2
    shot keyboard-enter
    log done
    exit 0
fi
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
    # The refused message, its reason in the tooltip.
    refused=$(locate 'release-notes')
    if [ -n "$refused" ]; then
        "$ne" drive move "$refused" >/dev/null 2>&1 || log "hover failed"
        sleep 3
        "$ne" shot message-refused-hover >/dev/null && log "shot message-refused-hover" ||
            log "shot failed: message-refused-hover"
    else
        log "not found: the refused message"
    fi
fi
scroll "$transcript" 30
shot "${prefix}message-failed"
log done
