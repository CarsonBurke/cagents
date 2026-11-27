#!/bin/bash
# Native evidence of the chat model picker with several providers offered.
# Start with --codex tool-rows --acp, so ChatGPT, Antigravity and Muse are
# offered beside the synthetic Claude. A pair steps file runs:
#   CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/model-picker-scene.sh" picker
# Prepare a new task once: start/stop a side, run this as prepare while it is
# stopped, then save-seed, so both sides' startup probes see the same
# catalogs. It captures a new chat's picker, a search across providers, and
# the picker of a conversation already under way. Every catalog is invented
# or recorded, and no provider is contacted.
set -euo pipefail
scene=${1:?usage: model-picker-scene.sh picker|prepare}
case $scene in picker|prepare) ;; *) exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
ne=${CAGENTS_EVIDENCE_COMMAND:-$here/../../scripts/native-evidence}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"

if [ "$scene" = prepare ]; then
    if ! "$ne" status | rg '^session: none$' >/dev/null; then
        echo 'Stop the native session before preparing its model catalogs.' >&2
        exit 2
    fi
    # The catalogs travel as arguments, so this also prepares a task on
    # another host through --remote.
    "$ne" env sh -c 'mkdir -p "$HOME/.claude" "$HOME/.codex" &&
        printf "%s\n" "$1" >"$HOME/.claude/fixture-models.json"' sh \
        "$(cat "$here/../../fixtures/accounts/claude-initialize-models.json")"
    effort='"supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"medium"},{"reasoningEffort":"high"},{"reasoningEffort":"xhigh"}]'
    "$ne" env sh -c 'printf "%s\n" "$1" >"$HOME/.codex/fixture-models.json"' sh \
        "{\"data\":[{\"id\":\"gpt-5.5\",\"model\":\"gpt-5.5\",\"displayName\":\"GPT-5.5\",\"isDefault\":true,\"defaultReasoningEffort\":\"medium\",$effort},{\"id\":\"gpt-5.5-mini\",\"model\":\"gpt-5.5-mini\",\"displayName\":\"GPT-5.5 mini\",\"defaultReasoningEffort\":\"medium\",$effort},{\"id\":\"gpt-5.4\",\"model\":\"gpt-5.4\",\"displayName\":\"GPT-5.4\",\"defaultReasoningEffort\":\"medium\",$effort}],\"nextCursor\":null}"
    "$ne" env sh -c 'printf "%s\n" "The offline picker fixture is ready." >"$HOME/.claude/fixture-reply.md"'
    # A bootstrap session may have cached the peers' original catalogs.
    "$ne" env python3 -c 'import os; from pathlib import Path; [p.unlink() for p in Path(os.environ["XDG_CACHE_HOME"]).rglob("account-usage.json")]'
    exit 0
fi

shot() {
    "$ne" drive hover 1,1 >/dev/null
    sleep 1
    "$ne" shot "$1"
}
# Where `observe` read the last target matching a pattern, as X,Y.
locate() {
    "$ne" drive observe 2>/dev/null |
        sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        tail -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
dismiss_inspector() {
    if "$ne" drive observe | rg -F 'Source control' >/dev/null; then
        "$ne" drive key ctrl+shift+i
    fi
    "$ne" drive wait 'Source control' --gone --timeout 20
}
# A new channel in the fixture project, from the project row's new-chat
# control, with its draft in the pane.
new_channel() {
    "$ne" drive click 174,108
    "$ne" drive wait 'conversation appears' --timeout 30
    dismiss_inspector
}
picker() {
    local at
    at=$(locate 'GPT|Default|Opus|Sonnet|Haiku|Fable|model')
    [ -n "$at" ] || { echo 'Could not locate the model picker.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
    "$ne" drive wait 'models' --timeout 20
    sleep 1
}

dismiss_inspector
"$ne" cli project open "$CAGENTS_EVIDENCE_TASK/fixture/project" >/dev/null
"$ne" drive wait project --timeout 30

# A conversation under way on Claude, which also lists Claude's models for
# the drafts after it.
new_channel
picker
"$ne" drive type claude
"$ne" drive key Return
"$ne" drive wait 'models' --gone --timeout 20
"$ne" drive click "$(locate Message)"
"$ne" drive type 'Show the picker of a conversation under way.'
"$ne" drive key Return
"$ne" drive wait 'picker fixture' --timeout 45
sleep 2
picker
shot started-picker
"$ne" drive key Escape
"$ne" drive wait 'models' --gone --timeout 20

# A new chat, which can start on any provider.
new_channel
picker
shot draft-picker
"$ne" drive type gpt
sleep 1
shot search
"$ne" drive key Escape
