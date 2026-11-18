#!/bin/bash
# Native fixture scene for sends cagents declines before they reach Claude.
# Run inside a ready native-evidence session, or set CAGENTS_EVIDENCE_REMOTE
# to drive one over plain SSH:
#
#   CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/3012-notices \
#     tests/fixtures/declined-send-scene.sh wide
#
# Start the side at 1500x1000 for wide, or 900x1000 for narrow. A Claude chat
# finishes one turn; the next send is declined with CROWDED_INPUT, which
# answers only that send; sending again goes through; then a send is
# declined with UNAVAILABLE_INPUT, which describes the conversation. The
# refusals come from the debug-only input refusal fixture
# (docs/chat-composer-fixture.md). No model or network request is made.
set -euo pipefail
scene=${1:?usage: declined-send-scene.sh wide|narrow}
case $scene in wide|narrow) ;; *) exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
project=$CAGENTS_EVIDENCE_TASK/fixture/project
[ -z "${CAGENTS_EVIDENCE_REMOTE:-}" ] || project='{fixture}/project'

point=move
if "$ne" drive --help 2>&1 | rg '^ +hover ' >/dev/null; then point=hover; fi
shot() {
    "$ne" drive "$point" 1,1 >/dev/null
    sleep 1
    "$ne" shot "$scene-$1"
}
locate() {
    "$ne" drive observe 2>/dev/null |
        sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        head -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
refuse_next() {
    "$ne" env sh -c 'printf "%s\n" "$1" >"$HOME/.cagents-fixture-refuse-input"' sh "$1"
}
send() {
    local at
    # The composer's + is the lowest text run that is only a +; the input
    # starts just right of it.
    at=$("$ne" drive observe 2>/dev/null |
        sed -nE 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+) +\+$/\1 \2 \3 \4/p' |
        sort -n -k2 | tail -1 | awk '{printf "%d,%d", $1 + 200, $2 + $4 / 2}')
    [ -n "$at" ] || { echo 'Could not locate the native chat composer.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
    [ -z "$1" ] || "$ne" drive type "$1" >/dev/null
    "$ne" drive key Return >/dev/null
    sleep 3
}

"$ne" env sh -c 'mkdir -p "$HOME/.claude"; echo 1 >"$HOME/.claude/fixture-first-turn-seconds"; rm -f "$HOME/.cagents-fixture-refuse-input"'
"$ne" cli project open "$project" >/dev/null
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$project" --prompt 'Fixture: summarize the open review comments.' >/dev/null
opened=
for attempt in 1 2 3 4 5 6 7 8; do
    sleep 3
    row=$(locate 'New channel|summarize the open review')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null
    # The first turn finishes a second after it starts.
    if "$ne" drive wait Finished --timeout 15 >/dev/null 2>&1; then
        opened=1
        break
    fi
done
[ -n "$opened" ] || { echo 'Fixture conversation did not open.' >&2; exit 1; }

refuse_next CROWDED_INPUT
send 'Also list the files they touch.'
shot crowded
# The declined text stays in the composer; send it again.
send ''
shot sent-again
refuse_next UNAVAILABLE_INPUT
send 'And who wrote them.'
shot unavailable
