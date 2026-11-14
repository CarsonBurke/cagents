#!/bin/bash
# Native footer evidence using only the isolated Claude and Codex peers.
# Start at 1500x900 or 900x900 with --codex tool-rows. A pair steps file runs:
#   CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/chat-footer-scene.sh" wide
# Use narrow instead of wide for a second pair started at --width 900.
# Prepare a new task once: start/stop a side, run this as prepare while stopped,
# then save-seed. This lets startup probes see the same catalogs on both sides.
# This scene deliberately starts each conversation through the native draft
# and changes reasoning through the native controls, rather than seeding a
# rendered snapshot. Its provider replies and catalog are invented/offline.
set -euo pipefail
scene=${1:?usage: chat-footer-scene.sh wide|narrow|prepare}
case $scene in wide|narrow|prepare) ;; *) exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
ne=${CAGENTS_EVIDENCE_COMMAND:-$here/../../scripts/native-evidence}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
if [ "$scene" = prepare ] && ! "$ne" status | rg '^session: none$' >/dev/null; then
    echo 'Stop the native session before preparing its footer catalog seed.' >&2
    exit 2
fi

# All files remain beneath the native-evidence fixture HOME. The Claude list
# is a recorded initialize response; the Codex list has the app-server shape
# consumed by Model::from_codex and names an explicitly offline model.
"$ne" env mkdir -p "$fixture/home/.claude" "$fixture/home/.codex"
"$ne" env cp "$here/../../fixtures/accounts/claude-initialize-models.json" \
    "$fixture/home/.claude/fixture-models.json"
"$ne" env sh -c 'printf "%s\n" "$1" >"$HOME/.codex/fixture-models.json"' sh \
    '{"data":[{"id":"offline-mini","model":"offline-mini","displayName":"Offline Mini","isDefault":true,"defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]}],"nextCursor":null}'
"$ne" env sh -c 'printf "%s\n" "The offline footer fixture is ready." >"$HOME/.claude/fixture-reply.md"'
"$ne" env sh -c 'printf "%s\n" "$1" "$2" >"$HOME/.claude/fixture-frames.jsonl"' sh \
    '{"type":"assistant","uuid":"footer-start-@TURN@","message":{"id":"footer-start-@TURN@","role":"assistant","content":[{"type":"text","text":"Checking the offline footer."}]}}' \
    '{"_sleep":20}'
if [ "$scene" = prepare ]; then
    # A bootstrap session may have cached the peer's original catalog. Remove
    # only its private account cache before saving the prepared fixture seed.
    "$ne" env python3 -c 'import os; from pathlib import Path; [p.unlink() for p in Path(os.environ["XDG_CACHE_HOME"]).rglob("account-usage.json")]'
    exit 0
fi

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
        tail -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
composer() {
    local at
    at=$(locate 'Message')
    if [ -z "$at" ]; then
        # In the old footer the permission label is just below the editor.
        at=$("$ne" drive observe 2>/dev/null |
            sed -nE '/Ask before|Read.only/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
            tail -1 | awk '{printf "%d,%d", $1 + 70, $2 + $4 / 2 - 45}')
    fi
    [ -n "$at" ] || { echo 'Could not locate the native chat composer.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
}
prompt() {
    composer
    "$ne" drive type "$1"
    "$ne" drive key Return
}

"$ne" cli project open "$fixture/project" >/dev/null
sleep 2
"$ne" drive key ctrl+shift+g
sleep 2
for provider in claude codex; do
    "$ne" drive key ctrl+t
    sleep 2
    if [ "$provider" = claude ]; then
        # A fresh fixture opens the inspector by default; keep the footer's
        # conversation pane available at both requested viewport widths.
        "$ne" drive key ctrl+shift+i
    fi
    "$ne" drive key ctrl+shift+m
    "$ne" drive type "$provider"
    if [ "$provider" = claude ]; then
        if [ "${SIDE:-after}" = before ]; then alias='Default (recommended)';
        else alias='Default (Opus 5.5)'; fi
        "$ne" drive wait "$alias" --timeout 20
        "$ne" drive click "$alias"
    else
        "$ne" drive wait 'Default model' --timeout 20
        "$ne" drive click 'Default model'
    fi
    sleep 2
    shot "$provider-draft"

    prompt "Show the $provider footer using the offline provider default."
    if [ "$provider" = claude ]; then
        "$ne" drive wait 'Checking the offline footer' --timeout 30
    else
        "$ne" drive wait 'Offline Mini' --timeout 30
    fi
    shot "$provider-running"
    "$ne" drive key ctrl+shift+m
    sleep 1
    shot "$provider-model-menu"

    # Model changes in Codex stage the next turn. Let this turn finish, choose
    # the effort, then submit a follow-up so the peer confirms the selection.
    if [ "$provider" = claude ]; then sleep 20; else sleep 7; fi

    if [ "${SIDE:-after}" = before ]; then
        "$ne" drive key Escape
        "$ne" drive click 'Reasoning'
        "$ne" drive click 'high'
    else
        "$ne" drive click 'High'
    fi
    sleep 2
    prompt "Confirm the $provider footer with high effort."
    sleep 2
    shot "$provider-high-effort"
done
