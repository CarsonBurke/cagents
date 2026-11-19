#!/bin/bash
# Native evidence for #3095: a Codex agent whose thread ID was never saved,
# reopened after its daemon is gone. Uses the offline `tool-rows` Codex peer.
# Copy this script and unnamed-agent-thread.py together to an evidence task's
# tools directory, with CAGENTS_EVIDENCE_TASK and CAGENTS_EVIDENCE_TOOL set.
#   seed  (session started with --codex tool-rows) runs one agent turn; then
#         stop, `forget`, and save-seed.
#   forget  clears the saved thread IDs, with the session stopped, and
#         records the listener each of those panes owns for `survive`.
#   survive  (after forget, before start) starts a stand-in for a Codex
#         provider that outlived its daemon, serving each forgotten pane's
#         listener. The app asks about it as it restores the card, so it must
#         exist first. It runs outside the fixture's runtime directory, so
#         stop leaves it: `survivors` lists those still running, and `end`
#         ends them.
#   show [SHOT]  opens the agent's channel and captures its card as SHOT
#         (default agent-card).
set -euo pipefail
if [ "${CAGENTS_AGENT_AUTHORITY+x}" ]; then
    printf '%s\n' 'unnamed-agent-thread-scene: run on the native-evidence host through plain SSH; inherited agent authority was not modified' >&2
    exit 77
fi
scene=${1:?usage: unnamed-agent-thread-scene.sh seed|forget|survive|survivors|end|show}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
: "${CAGENTS_EVIDENCE_TOOL:?name scripts/native-evidence on the host}"
here=$(cd -- "$(dirname -- "$0")" && pwd)
fixture=$CAGENTS_EVIDENCE_TASK/fixture
scene_state=$fixture/state/unnamed-agent-thread-scene
mkdir -p "$scene_state"
# Outside the fixture, which each start restores from its seed.
listeners=$CAGENTS_EVIDENCE_TASK/unnamed-agent-thread-listeners
survivors=$CAGENTS_EVIDENCE_TASK/unnamed-agent-thread-survivors
evidence() { "$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" "$@"; }
case "$scene" in
seed)
    evidence cli project open "$fixture/project" >/dev/null
    evidence cli --json channel spawn --repository "$fixture/project" --title 'Design review' >"$scene_state/channel.json"
    channel=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["channel"]["id"])' "$scene_state/channel.json")
    # The channel stays closed, so no window learns the agent's thread.
    evidence cli --json agent spawn --provider codex --surface chat \
        --repository "$fixture/project" --channel "$channel" --wait --timeout 120 \
        'Review the fixture design.' >"$scene_state/agent.json"
    ;;
forget)
    evidence env python3 "$here/unnamed-agent-thread.py" forget >"$scene_state/forgotten.json"
    evidence env python3 "$here/unnamed-agent-thread.py" listeners >"$listeners"
    ;;
survive)
    : >"$survivors"
    while read -r socket; do
        # The census proves a provider by its listener argument and the
        # pane's marker in it; it never connects. Not a process group leader,
        # setsid starts a session in place, so `$!` is the stand-in itself.
        setsid python3 -c 'import time; time.sleep(1800)' \
            --listen "unix://$socket" </dev/null >/dev/null 2>&1 &
        printf '%s %s\n' "$!" "$(cut -d' ' -f22 "/proc/$!/stat")" >>"$survivors"
    done <"$listeners"
    ;;
survivors|end)
    # Only the exact processes `survive` started: same ID, same start time.
    while read -r pid start; do
        [ "$(cut -d' ' -f22 "/proc/$pid/stat" 2>/dev/null)" = "$start" ] || continue
        if [ "$scene" = end ]; then kill "$pid"; else printf '%s\n' "$pid"; fi
    done <"$survivors"
    ;;
show)
    evidence drive key ctrl+shift+p >/dev/null
    evidence drive wait 'Search commands' --timeout 10 >/dev/null
    evidence drive type '# Design review' >/dev/null
    evidence drive key Return >/dev/null
    sleep 8
    evidence drive hover 1,1 >/dev/null
    sleep 1
    evidence shot "${2:-agent-card}"
    ;;
*) printf '%s\n' 'usage: unnamed-agent-thread-scene.sh seed|forget|survive|survivors|end|show' >&2; exit 2 ;;
esac
