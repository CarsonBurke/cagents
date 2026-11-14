#!/bin/bash
# Native evidence for refused shared-view migration. No live provider is used.
# Copy this script and seed-refusal-registry.py together to an evidence task's
# tools directory. Run with CAGENTS_EVIDENCE_TASK and CAGENTS_EVIDENCE_TOOL set.
# `seed` creates the saved chat; stop and save-seed afterward. In each later
# session `refuse` closes only the UI, arms the real registry's fixture failure,
# and relaunches. `retry` removes it and exercises the notice action.
set -euo pipefail
if [ "${CAGENTS_AGENT_AUTHORITY+x}" ]; then
    printf '%s\n' 'seed-refusal-scene: run on the native-evidence host through plain SSH; inherited agent authority was not modified' >&2
    exit 77
fi
scene=${1:?usage: seed-refusal-scene.sh seed|refuse|notice|retry}
layout=${2:-wide}
case "$layout" in wide|narrow) ;; *) exit 2 ;; esac
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
: "${CAGENTS_EVIDENCE_TOOL:?name scripts/native-evidence on the host}"
here=$(cd -- "$(dirname -- "$0")" && pwd)
fixture=$CAGENTS_EVIDENCE_TASK/fixture
[ -d "$fixture/bin" ] && [ -d "$fixture/project/.git" ] && [ -d "$fixture/home/.claude" ] || {
    printf '%s\n' 'seed-refusal-scene: start native-evidence first' >&2
    exit 2
}
scene_state=$fixture/state/seed-refusal-scene
mkdir -p "$scene_state"
evidence() { "$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" "$@"; }
shot() {
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}
open_channel() {
    evidence drive key ctrl+shift+p >/dev/null
    evidence drive wait 'Search commands' --timeout 10 >/dev/null
    evidence drive type '# Release planning' >/dev/null
    evidence drive key Return >/dev/null
    sleep 2
}
case "$scene" in
seed)
    cat >"$fixture/home/.claude/fixture-frames.jsonl" <<'FRAMES'
{"type":"assistant","uuid":"seed-ready-@TURN@","message":{"id":"seed-ready-@TURN@","role":"assistant","content":[{"type":"text","text":"The release checklist is ready. Keep the launch notes beside it."}]}}
{"type":"result","subtype":"success","is_error":false,"result":"The release checklist is ready."}
{"_end":true}
FRAMES
    evidence cli project open "$fixture/project" >/dev/null
    evidence cli --json channel spawn --repository "$fixture/project" --title 'Release planning' >"$scene_state/channel.json"
    channel=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["channel"]["id"])' "$scene_state/channel.json")
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --channel "$channel" \
        --prompt 'Prepare the release checklist.' >"$scene_state/agent.json"
    open_channel
    evidence drive wait 'launch notes' --timeout 45 >/dev/null
    # Click the composer by its visible placeholder; draft text remains unsent.
    evidence drive click 'Message' >/dev/null
    evidence drive type 'Draft launch notes: confirm the checklist with the team.' >/dev/null
    # Save the same unobstructed workspace on both wide and narrow desktops.
    evidence drive key ctrl+shift+i >/dev/null
    sleep 2
    shot seeded-draft
    ;;
refuse)
    channel=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["channel"]["id"])' "$scene_state/channel.json")
    open_channel
    # Only this private native app receives a regular window-close request.
    evidence nested niri msg action close-window >/dev/null
    sleep 4
    evidence env python3 "$here/seed-refusal-registry.py" arm "$channel" >"$scene_state/armed.json"
    # This command deliberately stays alive until stop; it does not hold a
    # build lock, and the evidence tool verifies all three recorded binaries.
    nohup "$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" relaunch \
        < /dev/null >"$CAGENTS_EVIDENCE_TASK/logs/seed-refusal-relaunch.log" 2>&1 &
    echo "$!" >"$scene_state/relaunch.pid"
    evidence drive wait 'Saved draft' --timeout 45 >/dev/null
    if [ -n "${NOTICE_CONTROL_POINT:-}" ]; then
        evidence drive click "$NOTICE_CONTROL_POINT" >/dev/null
        # Headless desktops can raise an unrelated missing-portal notice. Clear
        # transient notices through their existing control; seed refusals are
        # persistent and cannot be dismissed this way.
        if evidence drive --json observe | python3 -c 'import json,sys; sys.exit(0 if any("Dismiss" in run["text"] for run in json.load(sys.stdin)["runs"]) else 1)'; then
            width=$(cat "$CAGENTS_EVIDENCE_TASK/session/width")
            evidence drive click "$((width-96)),62" >/dev/null
        fi
        evidence drive key Escape >/dev/null
    fi
    shot "seed-refused-$layout"
    ;;
notice)
    # The application's existing persistent-notice popup holds the recovery.
    # Set the point after observing the native header at this viewport size.
    evidence drive move "${NOTICE_CONTROL_POINT:?name the native notices icon point}" >/dev/null
    evidence drive wait 'notices' --timeout 10 >/dev/null
    evidence drive click "$NOTICE_CONTROL_POINT" >/dev/null
    evidence drive wait 'Retry' --timeout 15 >/dev/null
    shot "seed-refused-notice-$layout"
    ;;
retry)
    evidence env python3 "$here/seed-refusal-registry.py" allow >"$scene_state/allowed.json"
    evidence drive click 'Retry' >/dev/null
    evidence drive wait 'Saved draft' --gone --timeout 45 >/dev/null
    shot "seed-retry-succeeded-$layout"
    ;;
*) printf '%s\n' 'usage: seed-refusal-scene.sh seed|refuse|notice|retry' >&2; exit 2 ;;
esac
