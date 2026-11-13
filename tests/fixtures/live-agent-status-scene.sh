#!/bin/bash
# Reproducible native status-row evidence, using the offline Claude fixture.
# Run inside a ready native-evidence session on fw16. Native-evidence pair
# steps can invoke this from its tool checkout:
#   CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/live-agent-status-scene.sh"
# Use a fresh fixture on each side, and --width 1200 (or 900 for narrow).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
prefix=${1:-}

# OCR is needed only for a field or passive text without an accessibility name.
locate() {
    "$ne" drive observe 2>/dev/null | sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        tail -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
shot() {
    "$ne" drive hover 1,1 >/dev/null
    sleep 1
    "$ne" shot "$prefix$1"
}

"$ne" env /usr/bin/python3 "$here/live-agent-status-seed.py" "$fixture"
"$ne" cli project open "$fixture/project"
sleep 2
"$ne" drive click 'New tab'
"$ne" drive click 'Resume conversation'
"$ne" drive wait 'Fixture: current agent status' --timeout 30
"$ne" drive click 'Resume Fixture: current agent status'
"$ne" drive wait 'Twelve launch records' --timeout 30
sleep 2
# The loaded history is part of the evidence, before a new prompt confirms
# the live phase. An unknown historical run must not look live forever.
shot agent-history
composer=$(locate 'Message')
[ -n "$composer" ] || { printf '%s\n' 'Could not find the native chat composer' >&2; exit 1; }
"$ne" drive click "$composer"
"$ne" drive type 'Continue the fixture release checks.'
"$ne" drive key Return
"$ne" drive wait 'obsolete work was stopped' --timeout 45
sleep 3
shot agent-status

# The old row has no failure control. Capture that state in the same place;
# the new row exposes its task and diagnostic on hover and opens its transcript.
if [ "${SIDE:-}" = before ]; then
    at=$(locate 'subagents.*working|subagents.*failed')
    [ -n "$at" ] || { printf '%s\n' 'Could not find the Before status row' >&2; exit 1; }
    "$ne" drive hover "$at"
    sleep 2
    "$ne" shot "${prefix}agent-failure-tooltip"
    "$ne" drive click "$at"
    sleep 2
    shot agent-failure-selected
    shot agent-failure-expanded
else
    "$ne" drive hover 'Open failed agent: Check installer rollback'
    sleep 2
    "$ne" shot "${prefix}agent-failure-tooltip"
    "$ne" drive click 'Open failed agent: Check installer rollback'
    "$ne" drive wait 'test archive is missing' --timeout 15
    shot agent-failure-selected
    "$ne" drive click 'Details'
    "$ne" drive wait 'Restore the test archive' --timeout 15
    shot agent-failure-expanded
fi

# Add two tasks that share a generic agent name, so their descriptions must
# distinguish the choices. The long task also exercises the narrow fade.
if [ "${SIDE:-}" != before ]; then
    "$ne" drive click 'Conversation'
fi
"$ne" env /bin/cp "$here/live-agent-status-multiple.jsonl" "$fixture/home/.claude/fixture-frames.jsonl"
composer=$(locate 'Message')
[ -n "$composer" ] || { printf '%s\n' 'Could not find the native chat composer' >&2; exit 1; }
"$ne" drive click "$composer"
"$ne" drive type 'Check the remaining release tasks.'
"$ne" drive key Return
"$ne" drive wait 'share the same generic name' --timeout 45
sleep 3
shot agent-multiple-status
if [ "${SIDE:-}" = before ]; then
    at=$(locate 'subagents.*working|subagents.*failed')
    [ -n "$at" ] || { printf '%s\n' 'Could not find the Before status row' >&2; exit 1; }
    "$ne" drive hover "$at"
    sleep 2
    "$ne" shot "${prefix}agent-multiple-tooltip"
    "$ne" drive click "$at"
    shot agent-multiple-chooser
    shot agent-multiple-selected
    shot agent-multiple-expanded
else
    "$ne" drive hover 'Choose from 3 failed agents'
    sleep 2
    "$ne" shot "${prefix}agent-multiple-tooltip"
    "$ne" drive click 'Choose from 3 failed agents'
    "$ne" drive wait 'Search subagents' --timeout 15
    shot agent-multiple-chooser
    "$ne" drive click 'toolu_recovery_failure'
    "$ne" drive wait 'recovery snapshot was incomplete' --timeout 15
    shot agent-multiple-selected
    "$ne" drive click 'Details'
    "$ne" drive wait 'Regenerate the fixture snapshot' --timeout 15
    shot agent-multiple-expanded
fi

# Leave the long task as the single failure so its full status label must
# fade within one line at the narrow viewport instead of using an ellipsis.
if [ "${SIDE:-}" != before ]; then
    "$ne" drive click 'Conversation'
fi
"$ne" env /bin/cp "$here/live-agent-status-long.jsonl" "$fixture/home/.claude/fixture-frames.jsonl"
composer=$(locate 'Message')
[ -n "$composer" ] || { printf '%s\n' 'Could not find the native chat composer' >&2; exit 1; }
"$ne" drive click "$composer"
"$ne" drive type 'Retry the rollback and signature checks.'
"$ne" drive key Return
"$ne" drive wait 'long recovery audit still needs attention' --timeout 45
sleep 3
shot agent-long-failure
