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
"$ne" env sh -c 'printf "%s\n" 20 >"$HOME/.codex/fixture-turn-pause"'
"$ne" env sh -c 'printf "%s\n" "The offline footer fixture is ready." >"$HOME/.claude/fixture-reply.md"'
"$ne" env sh -c 'printf "%s\n" 1 >"$HOME/.claude/fixture-first-turn-seconds"'
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
        # Token figures can wrap the old footer and move its permission
        # caption to a second line. The composer's + stays beside the input.
        at=$("$ne" drive observe 2>/dev/null |
            sed -nE '/ +\+$/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
            tail -1 | awk '{printf "%d,%d", $1 + 75, $2 + $4 / 2}')
    fi
    [ -n "$at" ] || { echo 'Could not locate the native chat composer.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
}
reasoning() {
    local at
    # OCR may join the old control's caption to the following token counts;
    # its leading 25 pixels are still inside the Reasoning button.
    at=$("$ne" drive observe 2>/dev/null |
        sed -nE '/Reasoning/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
        tail -1 | awk '{printf "%d,%d", $1 + 25, $2 + $4 / 2}')
    [ -n "$at" ] || { echo 'Could not locate the old Reasoning control.' >&2; return 1; }
    "$ne" drive click "$at"
}
prompt() {
    composer
    "$ne" drive type "$1"
    "$ne" drive key Return
}
dismiss_inspector() {
    local observed
    observed=$("$ne" drive observe)
    if rg -F 'Source control' <<<"$observed" >/dev/null; then
        # Escape closes a narrow modal inspector. A remaining wide dock
        # uses its ordinary toggle. Scope transitions can show either.
        "$ne" drive key Escape
        observed=$("$ne" drive observe)
        if rg -F 'Source control' <<<"$observed" >/dev/null; then
            "$ne" drive key ctrl+shift+i
        fi
    fi
    "$ne" drive wait 'Source control' --gone --timeout 20
}
open_new_channel() {
    local at
    dismiss_inspector
    "$ne" drive key ctrl+shift+g
    "$ne" drive wait channel --timeout 30
    dismiss_inspector
    # Select the first new channel in the fixture's 260px sidebar. An
    # automatic worktree draft must not satisfy the empty-chat assertion.
    at=$("$ne" drive observe | rg -i 'new[[:space:]]*channel' |
        sed -nE 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
        awk '$1 < 260 && $2 >= 80 && !found {printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2; found = 1}')
    [ -n "$at" ] || { echo 'Could not locate the new fixture channel in the sidebar.' >&2; return 1; }
    "$ne" drive click "$at"
    dismiss_inspector
    "$ne" drive wait 'conversation appears' --timeout 30
}

dismiss_inspector
"$ne" cli project open "$fixture/project" >/dev/null
"$ne" drive wait main --timeout 30
"$ne" drive click main
for provider in claude codex; do
    # Separate channels keep the tested footer at the full conversation pane
    # width instead of adding the second provider as a neighbouring column.
    open_new_channel
    "$ne" drive key ctrl+shift+m
    "$ne" drive wait models --timeout 20
    "$ne" drive type "$provider"
    # A provider search puts its default first. Enter selects that native
    # top match, whether its catalog has been observed or not.
    "$ne" drive key Return
    "$ne" drive wait models --gone --timeout 20
    case $provider in claude) draft_title='Claude chat';; codex) draft_title='Codex chat';; esac
    "$ne" drive --window "$draft_title" observe --no-text
    shot "$provider-draft"

    prompt "Show the $provider footer using the offline provider default."
    if [ "$provider" = claude ]; then
        # Match the retained Before comparison states: the narrow live
        # session is between turns; the wide session is still responding.
        if [ "$scene" = narrow ]; then
            "$ne" drive wait inished --timeout 45
        else
            "$ne" drive wait espond --timeout 30
        fi
    else
        "$ne" drive wait 'Offline' --timeout 30
    fi
    shot "$provider-running"
    "$ne" drive key ctrl+shift+m
    sleep 1
    shot "$provider-model-menu"

    # Model changes in Codex stage the next turn. Let this turn finish, choose
    # the effort, then submit a follow-up so the peer confirms the selection.
    sleep 20

    if [ "${SIDE:-after}" = before ]; then
        "$ne" drive key Escape
        reasoning
        "$ne" drive click 'high'
    elif [ "$provider" = codex ]; then
        # Search → the sole model → Low → Medium → High. The model popup's
        # search still owns focus after its screenshot and the provider pause.
        for _ in 1 2 3 4; do "$ne" drive key Tab; done
        "$ne" drive key Return
    else
        "$ne" drive click 'High'
    fi
    sleep 2
    prompt "Confirm the $provider footer with high effort."
    sleep 2
    shot "$provider-high-effort"
    # Settle while this channel remains visible, so the next provider's
    # sidebar has the same read/finished state on both revisions.
    if [ "$provider" = claude ]; then sleep 20; fi
done
