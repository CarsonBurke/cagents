#!/bin/bash
# The Work board at a long-lived installation's scale, for native evidence of
# its cards and columns: eight projects, about seventy cards, most of them old
# checkouts, a detached checkout, pull requests in every state, and a card with
# four agents at once. See tests/fixtures/work-board.py for the data.
#
# Run in a session, from a steps file or a shell on the build host, with
# CAGENTS_EVIDENCE_TASK naming a scripts/native-evidence task (see "Native
# evidence" in docs/development.md):
#
#   scripts/native-evidence start after --width 1500
#   tests/fixtures/work-board-scene.sh live    # each session
#   tests/fixtures/work-board-scene.sh open    # opens the Work board
#
# `live` makes the projects and seeds the forge (once per fixture), opens the
# projects, then starts synthetic Claude agents
# (tests/fixtures/chat-composer-claude.sh): four in atlas's
# feat/search-ranking worktree, one working, one asking for permission, one
# asking a question and one finished, and one working in journal's
# feat/offline-mode. A stopped session keeps no running agent, so it runs in
# every session. Git dates are relative to the time it first runs, so ages
# read the same in every session of a task that seeds again.
#
# `open` opens the board from the command palette and waits for it.
set -euo pipefail

here=$(cd -- "$(dirname -- "$0")/../.." && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
frames=$fixture/home/.claude/fixture-frames.jsonl

frames_for() {
    case $1 in
        working) rm -f -- "$frames" ;;
        finished)
            printf '%s\n' \
                '{"type":"result","uuid":"scene-result-@TURN@","subtype":"success","is_error":false,"result":"Done."}' \
                '{"_end":true}' >"$frames"
            ;;
        permission)
            printf '%s\n' \
                '{"type":"control_request","request_id":"permission-@TURN@","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-@TURN@","input":{"command":"cargo test --workspace","description":"Run the tests"}}}' \
                '{"_sleep":3600}' >"$frames"
            ;;
        question)
            printf '%s\n' \
                '{"type":"control_request","request_id":"question-@TURN@","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"tool-@TURN@","input":{"questions":[{"question":"Should recency outweigh click-through?","header":"Ranking","multiSelect":false,"options":[{"label":"Recency","description":"Newest first"},{"label":"Click-through","description":"Most opened first"}]}]}}}' \
                '{"_sleep":3600}' >"$frames"
            ;;
    esac
}

# agent STATE WORKTREE: a synthetic Claude among the worktree's panes.
agent() {
    local state=$1 worktree=$2
    frames_for "$state"
    evidence cli agent spawn --provider claude --model haiku --surface chat \
        --repository "${worktree%/.worktrees/*}" --worktree "$worktree" \
        --prompt "Fixture: $state" >/dev/null
    # Each agent reads the frames as its first turn starts; give it that turn
    # before the next state replaces them.
    sleep 4
}

cmd_live() {
    local project
    while read -r project; do
        evidence cli project open "$project" >/dev/null
    done < <(evidence env python3 -I "$here/tests/fixtures/work-board.py")
    local ranking=$fixture/projects/atlas/.worktrees/feat-search-ranking
    agent working "$ranking"
    agent permission "$ranking"
    agent question "$ranking"
    agent finished "$ranking"
    agent working "$fixture/projects/journal/.worktrees/feat-offline-mode"
    rm -f -- "$frames"
}

cmd_open() {
    local opened=false
    for _ in 1 2 3; do
        evidence drive key ctrl+shift+p >/dev/null
        if evidence drive wait 'Search commands' --timeout 5 >/dev/null 2>&1; then
            opened=true
            break
        fi
        evidence drive key Escape >/dev/null
        sleep 1
    done
    [ "$opened" = true ] || { echo 'the command palette did not open' >&2; return 1; }
    evidence drive type 'Open Work board' >/dev/null
    evidence drive key Return >/dev/null
    # The board's first scan reads every checkout and asks the forge.
    evidence drive wait 'Stale' --timeout 60 >/dev/null
    sleep 8
}

case ${1:-} in
    live) cmd_live ;;
    open) cmd_open ;;
    *) sed -n '2,24p' "$0" >&2; exit 2 ;;
esac
