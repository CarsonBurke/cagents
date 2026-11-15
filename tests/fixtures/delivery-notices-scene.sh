#!/bin/bash
# Native fixture transcript for background command, monitor, report and peer
# deliveries. Run inside a ready native-evidence session, or set
# CAGENTS_EVIDENCE_REMOTE=fw16 to drive one over plain SSH.
#
#   CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/2756-notices \
#     tests/fixtures/delivery-notices-scene.sh wide
#
# Start the native-evidence side at 1500x1000 for wide, or 900x1000 for narrow.
# The invented Claude stream is identical on both revisions and is recorded in
# its private provider history. No model or network request is made.
set -euo pipefail
scene=${1:?usage: delivery-notices-scene.sh wide|narrow|prepare}
case $scene in wide|narrow|prepare) ;; *) exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
frames=$(cat "$here/delivery-notices-scene/frames.jsonl")
"$ne" env sh -c 'mkdir -p "$CAGENTS_EVIDENCE_FIXTURE/home/.claude"; printf "%s\n" "$1" > "$CAGENTS_EVIDENCE_FIXTURE/home/.claude/fixture-frames.jsonl"' sh "$frames"
[ "$scene" != prepare ] || exit 0
project=$CAGENTS_EVIDENCE_TASK/fixture/project
[ -z "${CAGENTS_EVIDENCE_REMOTE:-}" ] || project='{fixture}/project'
"$ne" cli project open "$project" >/dev/null
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$project" --prompt 'Fixture: inspect the background GPU job notices.' >/dev/null
# The channel row is available before the provider finishes. Locate through
# its text and use the first match to avoid the hover tooltip's duplicate.
opened=
for attempt in 1 2 3 4 5 6 7 8; do
    sleep 3
    observed=$("$ne" drive observe 2>/dev/null)
    row=$(printf '%s\n' "$observed" | awk '/New channel|inspect the background GPU/ { if (match($0, /[0-9]+, *[0-9]+ +[0-9]+x[0-9]+/)) { print substr($0, RSTART, RLENGTH); exit } }' |
        tr ',x' '  ' | awk '{printf "%d,%d", $1+$3/2, $2+$4/2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null
    sleep 2
    if "$ne" drive observe 2>/dev/null | rg -q 'background success|GPU jobs finishing'; then
        opened=1
        break
    fi
done
[ -n "$opened" ] || { printf '%s\n' 'Fixture conversation did not open.' >&2; exit 1; }
"$ne" drive move 1,1 >/dev/null
sleep 2
"$ne" shot "$scene-delivery-notices"
printf '%s\n' 'Scene ready. Expand the command/report rows and capture details with native-evidence shot.'
