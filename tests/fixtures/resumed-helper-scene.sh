#!/bin/bash
# A Claude conversation whose background agent, "Outer", resumes a helper it
# ran earlier, for native evidence of which agents the conversation lists.
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md),
# inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/resumed-helper-scene.sh
#   scripts/native-evidence stop
#
# The turn replays claude-nested-agent-resumed-live.jsonl, recorded from
# Claude Code 2.1.325, without the helper's first run, as the conversation
# sees it when Claude Code has restarted since that run: Outer resumes its
# helper, "Inner", with SendMessage, and Claude Code forwards Inner's
# conversation under the Agent call that first started it. Both then finish.
# It shoots the side's prefix with "resumed-helper": the conversation once
# everything has finished.
set -u

here=$(cd "$(dirname "$0")" && pwd)
ne=$here/../../scripts/native-evidence
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
config=$CAGENTS_EVIDENCE_TASK/fixture/home/.claude

log() { printf '== %s %s\n' "$(date +%T)" "$*"; }

mkdir -p "$config"
# Skip from Outer's Agent call for Inner to just before its SendMessage. The
# fixture closes the turn with its own reply, and the recording's session
# belongs to another conversation.
jq -c 'select(.type != "result") | del(.session_id)' \
    "$here/claude-nested-agent-resumed-live.jsonl" | awk '
    /"name":"Agent"/ && /"parent_tool_use_id":"toolu_/ { skip = 1 }
    /"name":"SendMessage"/ { skip = 0 }
    !skip' >"$config/fixture-frames.jsonl"
"$ne" cli project open "$CAGENTS_EVIDENCE_TASK/fixture/project" >/dev/null || log "project open failed"
printf '%s\n' 'Outer has finished.' >"$config/fixture-reply.md"
"$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$CAGENTS_EVIDENCE_TASK/fixture/project" --prompt 'Run the outer agent.' >/dev/null ||
    log "spawn failed"
# The agent's conversation opens from its channel's row in the sidebar once
# its turn has replied.
for _ in 1 2 3 4 5 6 7 8; do
    sleep 4
    "$ne" drive hover 1,1 >/dev/null 2>&1
    sleep 1
    row=$("$ne" drive observe 2>/dev/null | grep -E 'New channel|Run the outer' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    "$ne" drive click "$row" >/dev/null 2>&1
    sleep 3
    "$ne" drive observe 2>/dev/null | grep -q 'Outer has finished' && break
done
"$ne" drive hover 1,1 >/dev/null 2>&1
sleep 2
"$ne" shot resumed-helper >/dev/null && log "shot resumed-helper" || log "shot failed"
