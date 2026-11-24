#!/bin/bash
# Native evidence of how the app names sessions, accounts and channels in
# words rather than by their IDs. Run inside a ready native-evidence session;
# no live provider, account or model is contacted.
#
#   scripts/native-evidence start before --width 1200
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/raw-id-scene.sh cli-send
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/raw-id-scene.sh approval
#   scripts/native-evidence stop
#
# From a cagents session, give `scripts/native-evidence --remote fw16 pair` a
# steps file that runs `CAGENTS_EVIDENCE_TASK=$task
# "$tool/tests/fixtures/raw-id-scene.sh" cli-send` and then `approval`, with
# `SIDE` set as the pair sets it.
#
# Scenes, in this order when both run in one session:
#
#   cli-send  shoots cli-send-rows: a Claude conversation in the channel
#             "Coordination rows" whose turn replays raw-id-scene/cli-send.jsonl,
#             two `cagents` CLI calls and the receipts the CLI printed for them:
#               cagents send 95fcd1b9 ...  Sent 45d4828a to session 95fcd1b9
#                                          "Fix the sidebar"; ...
#               cagents reply 6e21b0c4 ... Replied 9b73d2e1 to session 3f0e2a17;
#                                          ...
#             so one row names a titled session and the other an untitled one.
#   approval  shoots approval-dialog: a Claude conversation in the channel
#             "Releases" runs `cagents automation propose` as a real Bash call
#             (fixture-run-once, with the conversation's agent authority), for
#             a definition placed in that channel; the scene then opens the
#             request from the banner the project's window shows.
#
# The approval scene needs SIDE (before or after), to run that side's CLI.
set -euo pipefail

scene=${1:?usage: raw-id-scene.sh cli-send|approval}
case "$scene" in cli-send | approval) ;; *) exit 2 ;; esac
here=$(cd -- "$(dirname -- "$0")" && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name a ready native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude
# Never create a provider directory except inside an existing evidence fixture.
[ -d "$fixture/bin" ] && [ -d "$fixture/project/.git" ] && [ -d "$config" ] || {
    printf '%s\n' 'raw-id-scene: start native-evidence first' >&2
    exit 2
}

log() { printf '== %s %s %s\n' "$(date +%T)" "$scene" "$*" >&2; }

shot() {
    evidence drive hover 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}

# Whether the screen shows text matching the extended regular expression $1.
visible() {
    evidence drive observe 2>/dev/null | grep -qE -- "$1"
}

# Wait up to $2 seconds for text matching $1. On a timeout the scene keeps
# what the window showed, then fails.
await() {
    local pattern=$1 seconds=${2:-60} waited=0
    while ! visible "$pattern"; do
        if [ "$waited" -ge "$seconds" ]; then
            log "not shown within ${seconds}s: $pattern"
            evidence drive observe >&2 || :
            evidence shot "debug-$scene-missing" >&2 || :
            exit 1
        fi
        sleep 2
        waited=$((waited + 2))
    done
}

# A channel titled $1 in the fixture project, by its ID.
channel() {
    evidence cli channel spawn --repository "$fixture/project" --title "$1" | cut -f1
}

# Start a Claude conversation in channel $1 with prompt $2, then show that
# channel by its title $3.
converse() {
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --channel "$1" --prompt "$2" >/dev/null
    # The channel's sidebar row opens it; the command palette is the
    # fallback, since a session that has only just started can miss a
    # shortcut.
    local tries=0
    until evidence drive click "$3" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -lt 15 ] || break
        sleep 2
    done
    if [ "$tries" -ge 15 ]; then
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
        [ "$opened" = true ] || { log "could not open $3"; exit 1; }
        evidence drive type "# $3" >/dev/null
        evidence drive key Return >/dev/null
    fi
    sleep 2
}

# A fresh fixture has no project open yet; opening one again is harmless.
evidence cli project open "$fixture/project" >/dev/null
sleep 2

if [ "$scene" = cli-send ]; then
    cp "$here/raw-id-scene/cli-send.jsonl" "$config/fixture-frames.jsonl.next"
    mv "$config/fixture-frames.jsonl.next" "$config/fixture-frames.jsonl"
    printf '%s\n' 'Both sessions have what they need.' >"$config/fixture-reply.md"
    id=$(channel 'Coordination rows')
    converse "$id" 'Tell the sidebar session to rebase, and answer the release question.' \
        'Coordination rows'
    # The answer comes after both rows; the screen reading may split it.
    await 'what they need|sessions have' 120
    # Either row, before or after the change, by the part of it the change
    # leaves alone.
    # The screen reading splits a row's runs where it likes ("Replyto").
    await 'sidebar' 10
    await 'Repl' 10
    shot cli-send-rows
    # Later conversations in this fixture replay nothing.
    rm -f "$config/fixture-frames.jsonl"
    exit 0
fi

: "${SIDE:?set SIDE to before or after}"
rm -f "$config/fixture-frames.jsonl"
printf '%s\n' 'I asked you to approve the weekly release check.' >"$config/fixture-reply.md"
id=$(channel 'Releases')
definition=$fixture/proposal.json
/usr/bin/python3 -I - "$fixture/project" "$id" "$definition" <<'PY'
import json, sys
project, channel, path = sys.argv[1:]
definition = {
    "name": "Weekly release check",
    "prompt": "Read the release checklist, update the changelog draft, and report anything that still blocks the weekly release.",
    "agent": {
        "provider": "Claude", "account": None,
        "target": {"repository": project, "directory": project, "channel": channel},
        "surface": "Chat", "model": None, "permission_mode": None,
    },
    "schedule": {"pattern": {"weekdays": {"hour": 9, "minute": 30}}, "timezone": "UTC"},
    "session_mode": "fresh",
    "precheck": None,
}
with open(path, "w") as f:
    json.dump(definition, f)
PY
# This side's own CLI: the provider's PATH need not hold the development
# build. Only the next turn to start runs it, which is the new conversation's.
cli=$CAGENTS_EVIDENCE_TASK/target-$SIDE/dev-runtime/cagents
printf '%q automation propose --request-id 6f2c1a52-8a3e-4c1e-9d47-2b5f0e9a1c11 --definition %q\n' \
    "$cli" "$definition" >"$config/fixture-run-once"
converse "$id" 'Schedule the weekly release check.' 'Releases'
await 'Scheduled work requested' 90
# The button's label, through the accessibility tree; the screen reading may
# read it as two words.
evidence drive click 'Review request' >/dev/null 2>&1 || evidence drive click 'Review' >/dev/null
# The dialog's buttons read whole where its heading may not.
await 'Not now|Approve and|Approve scheduled' 15
shot approval-dialog
evidence drive key Escape >/dev/null
