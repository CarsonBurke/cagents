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
#           agents, one with a long name, all of Claude Code's default
#           general-purpose type but Trace, an Explore agent, whose row
#           names its type. Review starts a second opinion,
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
#
# With the argument `strip`, the scene stops halfway through settling, while
# Trace and Rollback have finished and the rest still work, for the strip of
# agent tabs above the composer (use --height 680, so the lead's transcript
# scrolls): strip-main with the lead's tab shown, strip-selected with Audit
# open, and strip-jump, scrolled up as the rest of the turn arrives, with the
# jump to the new activity. A second argument names the layout, such as
# `narrow` for a window of --width 1080, whose pane the inspector narrows to
# about 420 pixels, and is added to each shot's name.
set -u
. "$(dirname -- "$(readlink -f -- "${BASH_SOURCE[0]}")")/lib/fixture-home.sh"
require_fixture_home HOME "${CAGENTS_EVIDENCE_TASK:+$CAGENTS_EVIDENCE_TASK/fixture/home}"
require_fixture_home CLAUDE_CONFIG_DIR "$CAGENTS_EVIDENCE_TASK/fixture/home/.claude"
mode=${1:-}
layout=${2:+-$2}

here=$(cd "$(dirname "$0")" && pwd)
frames=$here/nested-agents-scene
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

log() { printf '== %s %s\n' "$(date +%T)" "$*"; }
# The centre of the lowest place the screen shows a match of the extended
# regular expression PATTERN, such as a tab in the strip above the composer
# rather than a transcript row of the same name. The screen is read by OCR,
# which can run words together.
lowest() {
    "$ne" drive observe 2>/dev/null | grep -E -- "$1" |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        sort -k2 -n | tail -1 | awk 'NF == 4 {printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
shot() {
    "$ne" drive hover 1,1 >/dev/null 2>&1
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
# the reviewers are running. A busy build host can take a minute to show it.
composer=
for _ in $(seq 15); do
    sleep 4
    "$ne" drive hover 1,1 >/dev/null 2>&1
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
    # Where the hint cannot be read, the composer's input sits at a fixed place
    # in the window, whose size observe reports on its "window" line (an
    # older driver reported it as "output="). The point left of centre and
    # above the footer is inside the input, clear of the Stop control.
    [ -n "$composer" ] || composer=$(printf '%s\n' "$observed" |
        sed -nE -e 's/.*output=([0-9]+)x([0-9]+).*/\1 \2/p' \
            -e "s/^ *window .* ([0-9]+)x([0-9]+)$/\1 \2/p" | head -1 |
        awk '{printf "%d,%d", $1 * 0.45, $2 - 86}')
    # A read of the screen can come back empty; look again.
    [ -z "$composer" ] || break
done
if [ -z "$composer" ]; then
    log "the conversation did not open; the screen read:"
    "$ne" drive observe 2>&1 | sed 's/^/   /'
    exit 1
fi
shot agents-running

if [ "$mode" = strip ]; then
    # Settle up to Rollback's finishing, then wait for the scene's release.
    rm -f "$config/fixture-release.rest"
    { head -n 19 "$frames/settle.jsonl"; echo '{"_await": "rest"}'; tail -n +20 "$frames/settle.jsonl"; } \
        >"$config/fixture-frames.jsonl"
    "$ne" drive click "$composer" >/dev/null 2>&1
    "$ne" drive type 'How are the reviews going?' >/dev/null 2>&1
    "$ne" drive key Return >/dev/null 2>&1
    sleep 4
    shot strip-main"$layout"
    tab=$(lowest 'Audit ?retry')
    if [ -n "$tab" ]; then "$ne" drive click "$tab" >/dev/null 2>&1; else log "no Audit tab to open"; fi
    sleep 2
    shot strip-selected"$layout"
    # The lead's tab, named Conversation before it was Main.
    tab=$(lowest Main)
    [ -n "$tab" ] || tab=$(lowest Conversation)
    if [ -n "$tab" ]; then "$ne" drive click "$tab" >/dev/null 2>&1; else log "no lead tab to return to"; fi
    sleep 2
    # Scroll up over the transcript, then let the turn go on below.
    transcript=$(printf '%s' "$composer" | awk -F, '{printf "%d,%d", $1, $2 / 3}')
    "$ne" drive scroll --at "$transcript" v -15 >/dev/null 2>&1 || log "scroll failed"
    sleep 1
    : >"$config/fixture-release.rest"
    sleep 4
    shot strip-jump"$layout"
    log done
    exit 0
fi

cp "$frames/settle.jsonl" "$config/fixture-frames.jsonl"
"$ne" drive click "$composer" >/dev/null 2>&1
"$ne" drive type 'How are the reviews going?' >/dev/null 2>&1
"$ne" drive key Return >/dev/null 2>&1
"$ne" drive wait 'waiting on its check' --timeout 25 >/dev/null 2>&1 || log "settle reply not seen"
sleep 3
shot agents-settled
log done
