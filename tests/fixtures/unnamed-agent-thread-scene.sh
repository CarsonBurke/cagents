#!/bin/bash
# Native evidence for #3095: a Codex agent whose thread ID was never saved,
# reopened after its daemon is gone. Uses the offline `tool-rows` Codex peer.
# Copy this script and unnamed-agent-thread.py together to an evidence task's
# tools directory, with CAGENTS_EVIDENCE_TASK and CAGENTS_EVIDENCE_TOOL set.
#   seed  (session started with --codex tool-rows) runs one agent turn; then
#         stop, `forget`, and save-seed.
#   forget  clears the saved thread IDs, with the session stopped.
#   show  opens the agent's channel and captures its card.
set -euo pipefail
if [ "${CAGENTS_AGENT_AUTHORITY+x}" ]; then
    printf '%s\n' 'unnamed-agent-thread-scene: run on the native-evidence host through plain SSH; inherited agent authority was not modified' >&2
    exit 77
fi
scene=${1:?usage: unnamed-agent-thread-scene.sh seed|forget|show}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
: "${CAGENTS_EVIDENCE_TOOL:?name scripts/native-evidence on the host}"
here=$(cd -- "$(dirname -- "$0")" && pwd)
fixture=$CAGENTS_EVIDENCE_TASK/fixture
scene_state=$fixture/state/unnamed-agent-thread-scene
mkdir -p "$scene_state"
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
    ;;
show)
    evidence drive key ctrl+shift+p >/dev/null
    evidence drive wait 'Search commands' --timeout 10 >/dev/null
    evidence drive type '# Design review' >/dev/null
    evidence drive key Return >/dev/null
    sleep 8
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot agent-card
    ;;
*) printf '%s\n' 'usage: unnamed-agent-thread-scene.sh seed|forget|show' >&2; exit 2 ;;
esac
