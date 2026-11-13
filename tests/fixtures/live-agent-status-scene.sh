#!/bin/bash
# Reproducible native status-row evidence, using the offline Claude fixture.
# Run inside a ready native-evidence session on fw16. Native-evidence pair
# steps can invoke this from its tool checkout:
#   CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/live-agent-status-scene.sh"
# Use a fresh fixture on each side, and --width 1200. Optionally set
# CAGENTS_EVIDENCE_NARROW_WIDTH=900 to capture ordinary states at both widths
# within one session. Tooltip captures stay at the initial wide width.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ne=${CAGENTS_EVIDENCE_COMMAND:-$here/../../scripts/native-evidence}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
prefix=${1:-}
narrow_width=${CAGENTS_EVIDENCE_NARROW_WIDTH:-}
if [ -n "$narrow_width" ] && ! [[ $narrow_width =~ ^[1-9][0-9]*$ ]]; then
    printf '%s\n' 'CAGENTS_EVIDENCE_NARROW_WIDTH must be a positive logical pixel width' >&2
    exit 2
fi

# The build host may have an older OCR-only harness. Use its advertised
# pointer command and visible captions instead of accessibility-only names.
pointer_command=move
if "$ne" drive --help 2>&1 | grep -w hover >/dev/null; then
    pointer_command=hover
fi
point() {
    "$ne" drive "$pointer_command" "$@"
}

# The centre of the last visible OCR text run matching an extended regex.
locate() {
    "$ne" drive observe 2>/dev/null | sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        tail -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
# OCR can combine the counts and the failure button into one text run.
# The failure control is last, so its visible trailing edge is inside it.
locate_failure() {
    "$ne" drive observe 2>/dev/null | sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        tail -1 | awk '{printf "%d,%d", $1 + $3 - 12, $2 + $4 / 2}'
}
shot() {
    point 1,1 >/dev/null
    sleep 1
    "$ne" shot "$prefix$1"
    if [ -n "$narrow_width" ]; then
        # Only the task's private compositor receives these commands. The
        # no-heal drive/shot paths preserve this deliberate narrow column.
        "$ne" nested niri msg action set-column-width "$narrow_width" >/dev/null
        sleep 1
        point 1,1 >/dev/null
        # The image keeps the original framebuffer. Preserve the observed
        # native window size alongside it instead of inferring it from pixels.
        "$ne" drive --json observe >"$CAGENTS_EVIDENCE_TASK/layout-${SIDE:-unknown}-narrow-$prefix$1.json"
        "$ne" shot "narrow-$prefix$1"
        # An absolute percentage restores the full viewport deterministically;
        # maximize-column would toggle its state on repeated captures.
        "$ne" nested niri msg action set-column-width '100%' >/dev/null
        sleep 1
    fi
}

"$ne" env /usr/bin/python3 "$here/live-agent-status-seed.py" "$fixture"
"$ne" cli project open "$fixture/project"
sleep 2
# The command palette opens the same Resume action without depending on
# the icon-only new-tab control's accessibility name.
"$ne" drive key ctrl+shift+p
"$ne" drive type 'Resume conversation'
"$ne" drive key Return
"$ne" drive wait 'Fixture: current agent status' --timeout 30
"$ne" drive click 'Fixture: current agent status'
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
    point "$at"
    sleep 2
    "$ne" shot "${prefix}agent-failure-tooltip"
    "$ne" drive click "$at"
    sleep 2
    shot agent-failure-selected
    shot agent-failure-expanded
else
    at=$(locate_failure '1 failed')
    [ -n "$at" ] || { printf '%s\n' 'Could not find the single-failure status control' >&2; exit 1; }
    point "$at"
    sleep 2
    "$ne" shot "${prefix}agent-failure-tooltip"
    "$ne" drive click "$at"
    "$ne" drive wait 'test archive is missing' --timeout 15
    shot agent-failure-selected
    "$ne" drive click 'Details'
    "$ne" drive wait 'Restore the test archive' --timeout 15
    shot agent-failure-expanded
fi

# A new prompt retires earlier failures. Retry rollback and fail it afresh,
# then add two tasks sharing a generic agent name. Their descriptions must
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
    point "$at"
    sleep 2
    "$ne" shot "${prefix}agent-multiple-tooltip"
    "$ne" drive click "$at"
    shot agent-multiple-chooser
    shot agent-multiple-selected
    shot agent-multiple-expanded
else
    at=$(locate_failure '3 failed')
    [ -n "$at" ] || { printf '%s\n' 'Could not find the multiple-failure status control' >&2; exit 1; }
    point "$at"
    sleep 2
    "$ne" shot "${prefix}agent-multiple-tooltip"
    "$ne" drive click "$at"
    "$ne" drive wait 'Search subagents' --timeout 15
    shot agent-multiple-chooser
    at=$(locate 'Audit recovery after')
    [ -n "$at" ] || { printf '%s\n' 'Could not find the recovery task in the Failed chooser' >&2; exit 1; }
    "$ne" drive click "$at"
    "$ne" drive wait 'recovery snapshot was incomplete' --timeout 15
    shot agent-multiple-selected
    "$ne" drive click 'Details'
    "$ne" drive wait 'Regenerate the fixture snapshot' --timeout 15
    shot agent-multiple-expanded
fi

# Retry the other checks successfully and fail the recovery audit afresh
# in this turn. Its full single-failure label must fade within one line at
# the narrow viewport instead of using an ellipsis.
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

# End only the current workers. Earlier failures remain historical reports;
# this new accepted prompt must not imply that their checks were repaired.
"$ne" env /bin/cp "$here/live-agent-status-settled.jsonl" "$fixture/home/.claude/fixture-frames.jsonl"
composer=$(locate 'Message')
[ -n "$composer" ] || { printf '%s\n' 'Could not find the native chat composer' >&2; exit 1; }
"$ne" drive click "$composer"
"$ne" drive type 'Finish the current worker checks'
"$ne" drive key Return
"$ne" drive wait 'no current workers remain' --timeout 45
sleep 3
shot agent-settled
