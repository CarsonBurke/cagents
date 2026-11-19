#!/bin/bash
# A Claude lead conversation whose eight background agents start helpers of
# their own and settle every way an agent can, for native evidence of how
# many agents the footer and the agent tabs say are running, and of how each
# agent's row, report and tab say how it stands.
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md),
# inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/nested-agents-scene.sh
#   scripts/native-evidence stop
#
# A synthetic Claude agent (tests/fixtures/chat-composer-claude.sh) replays
# nested-agents-scene/, frame shapes recorded from Claude Code 2.1.325:
#
#   launch  the lead starts a background soak test and eight background
#           agents, one with a long name. Review starts a second opinion,
#           which starts a spot check; Audit forks a review skill; Trace
#           starts a log reader, which starts a grep. Claude Code forwards
#           each helper's conversation under the call that started it.
#   settle  those helpers, Trace, Rollback and the long-named Upgrade finish;
#           Migrate fails and Index is stopped. Review stops while a check
#           it started still runs, and Deploy while a helper it started
#           still runs. The soak test ends, but its notification never
#           arrives; only the background level says so.
#
# Shots: agents-running after launch, agents-settled after settle, each with
# the side's prefix as native-evidence adds it.
set -u

here=$(cd "$(dirname "$0")" && pwd)
frames=$here/nested-agents-scene
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

log() { printf '== %s %s\n' "$(date +%T)" "$*"; }
shot() {
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    "$ne" shot "$1" >/dev/null && log "shot $1" || log "shot failed: $1"
}

mkdir -p "$config"
cp "$frames/launch.jsonl" "$config/fixture-frames.jsonl"
: >"$config/fixture-reply.md"
"$ne" cli project open "$fixture/project" >/dev/null || log "project open failed"
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$fixture/project" --prompt 'Review the release.' >/dev/null ||
    log "spawn failed"
# Open the conversation from its channel's row once its first turn has said
# the reviewers are running.
composer=
for _ in 1 2 3 4 5 6 7 8; do
    sleep 4
    "$ne" drive move 1,1 >/dev/null 2>&1
    sleep 1
    row=$("$ne" drive observe 2>/dev/null | grep -E 'New channel|Review the release' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null 2>&1
    sleep 3
    observed=$("$ne" drive observe 2>/dev/null)
    # The screen is read by OCR, which splits and misreads words.
    printf '%s\n' "$observed" | grep -qiE 'soak|reviewers' || continue
    composer=$(printf '%s\n' "$observed" | grep -E 'Message' | tail -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$composer" ] || composer=$(printf '%s\n' "$observed" |
        sed -nE 's/.*output=([0-9]+)x([0-9]+).*/\1 \2/p' | awk '{printf "%d,%d", $1 * 0.65, $2 - 67}')
    break
done
if [ -z "$composer" ]; then
    log "the conversation did not open"
    exit 1
fi
shot agents-running

cp "$frames/settle.jsonl" "$config/fixture-frames.jsonl"
"$ne" drive click "$composer" >/dev/null 2>&1
"$ne" drive type 'How are the reviews going?' >/dev/null 2>&1
"$ne" drive key Return >/dev/null 2>&1
"$ne" drive wait 'waiting on its check' --timeout 25 >/dev/null 2>&1 || log "settle reply not seen"
sleep 3
shot agents-settled
log done
