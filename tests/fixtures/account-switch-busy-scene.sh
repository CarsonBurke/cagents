#!/bin/bash
# Native evidence for an automatic account switch that meets a busy daemon,
# using only the offline Codex `tool-rows` peer and two fixture account homes.
# Start with --codex tool-rows. Prepare a new task once: start a side, run
# this as `prepare`, stop, then save-seed, so the linked second account and
# automatic routing are present when the app starts. Then, in each session:
#   CAGENTS_EVIDENCE_TASK=$task tests/fixtures/account-switch-busy-scene.sh wide
# (or `narrow`, in a session started with --width 900). A pair steps file can
# run it with "$tool/tests/fixtures/account-switch-busy-scene.sh".
#
# The scene starts a conversation on the primary account, reports that account
# limited, keeps the daemon busy (see "An automatic account switch" in
# docs/fixtures.md), sends, and captures the card once the switch is refused.
# It then sends again, frees the daemon while that retry waits, and
# captures where the message went. With VARIANT=exit, the
# conversation's own Codex process exits during the wait instead, and the
# scene ends at the refusal. VARIANT=exclusive holds the daemon exclusively.
# VARIANT=diverged leaves the daemon free, but gives the other account a
# different copy of the conversation's history, so the daemon refuses the
# switch for that reason before changing anything; the scene then removes the
# copy and sends again.
set -euo pipefail
. "$(dirname -- "$(readlink -f -- "${BASH_SOURCE[0]}")")/lib/fixture-home.sh"
require_fixture_home HOME "${CAGENTS_EVIDENCE_TASK:+$CAGENTS_EVIDENCE_TASK/fixture/home}"
require_fixture_home CLAUDE_CONFIG_DIR "$CAGENTS_EVIDENCE_TASK/fixture/home/.claude"
require_fixture_home CODEX_HOME "$CAGENTS_EVIDENCE_TASK/fixture/home/.codex"
scene=${1:?usage: account-switch-busy-scene.sh prepare|wide|narrow}
case $scene in prepare|wide|narrow) ;; *) exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
ne=${CAGENTS_EVIDENCE_COMMAND:-$here/../../scripts/native-evidence}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
alternate=alt-codex

limits() { # HOME-relative Codex home, allowed true|false
    "$ne" env sh -c 'printf "{\"ordinaryUsageAllowed\": %s}\n" "$2" >"$HOME/$1/fixture-rate-limits.json"' sh "$1" "$2"
}

if [ "$scene" = prepare ]; then
    for home in .codex "$alternate"; do
        "$ne" env sh -c 'set -e; mkdir -p "$HOME/$1"; chmod 700 "$HOME/$1"
            touch "$HOME/$1/fixture-rollouts"; echo 1 >"$HOME/$1/fixture-turn-pause"' sh "$home"
        limits "$home" true
    done
    "$ne" env sh -c 'echo first@invalid >"$HOME/.codex/fixture-account-email"
        echo second@invalid >"$HOME/$1/fixture-account-email"' sh "$alternate"
    "$ne" cli account link codex "~/$alternate"
    "$ne" cli account default codex automatic
    # Record each login and its allowance before the seed is saved.
    for account in $("$ne" cli account list | awk '$1 ~ /^codex\// {print $1}'); do
        "$ne" cli account info --account "$account" --refresh
    done
    exit 0
fi

# Each step leaves the located text beside the shots, so a failed run says
# where it stopped.
notes=${CAGENTS_EVIDENCE_NOTES:-$CAGENTS_EVIDENCE_TASK/scene-${SIDE:-side}-$scene${VARIANT:+-$VARIANT}}
mkdir -p "$notes"
step() {
    echo "== $1" >&2
    "$ne" drive observe >"$notes/$1.txt" 2>&1 || :
}
shot() {
    "$ne" drive hover 1,1 >/dev/null
    sleep 1
    "$ne" shot "$scene${VARIANT:+-$VARIANT}-$1"
    step "$1"
}
locate() { # regex -> centre of the last matching text run
    "$ne" drive observe 2>/dev/null |
        sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        tail -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
composer() {
    local at attempt
    for attempt in 1 2 3; do
        at=$(locate 'Message|Now list|questions')
        # OCR can miss the placeholder; the composer's + stays beside it.
        [ -n "$at" ] || at=$("$ne" drive observe 2>/dev/null |
            sed -nE '/ +\+$/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
            awk '$2 > 700' | tail -1 | awk '{printf "%d,%d", $1 + 75, $2 + $4 / 2}')
        [ -n "$at" ] && break
        sleep 1
    done
    [ -n "$at" ] || { echo 'Could not locate the native chat composer.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
}
hold() { # Keep the daemon busy with a held shutdown preflight.
    # VARIANT=exclusive holds the daemon as a long operation does, so even
    # the switch's handshake waits; otherwise as a slow ordinary request.
    "$ne" env sh -c 'case $1 in exclusive) echo exclusive ;; esac >"$HOME/.cagents-fixture-hold-preflight"' sh "${VARIANT:-}"
    # The preflight's own client gives up after a few seconds; the daemon
    # keeps holding its admission until the marker goes.
    "$ne" cli daemon preflight >/dev/null 2>&1 || :
}
release() {
    "$ne" env sh -c 'rm -f "$HOME/.cagents-fixture-hold-preflight"'
}
trap release EXIT
refresh() { # Ask the app to check the system Codex account again now.
    local at
    at=$(locate '\+1')
    [ -n "$at" ] || { echo 'Could not locate the Codex account status.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
    "$ne" drive wait 'Linked account' --timeout 10 >/dev/null
    # The check button sits just right of the account's plan chip.
    at=$("$ne" drive observe 2>/dev/null |
        sed -nE '/ Free$/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p' |
        head -1 | awk '{printf "%d,%d", $1 + $3 + 21, $2 + $4 / 2}')
    [ -n "$at" ] || { echo 'Could not locate the account check button.' >&2; return 1; }
    "$ne" drive click "$at" >/dev/null
    "$ne" drive wait 'Limited' --timeout 20 >/dev/null
    sleep 1
    step limited
    "$ne" drive key Escape >/dev/null
}
diverge() { # Give the other account a different copy of each rollout.
    "$ne" env sh -c 'set -e; cd "$HOME/.codex"
        find sessions -type f -name "*.jsonl" | while IFS= read -r rollout; do
            mkdir -p "$HOME/$1/${rollout%/*}"
            printf "{\"fixture\":\"diverged\"}\n" >"$HOME/$1/$rollout"
        done' sh "$alternate"
}
converge() { # Remove those copies, so the switch can copy the history.
    "$ne" env sh -c 'set -e; cd "$HOME/.codex"
        find sessions -type f -name "*.jsonl" | while IFS= read -r rollout; do
            rm -f "$HOME/$1/$rollout"
        done' sh "$alternate"
}
peer_exits() { # End the conversation's own Codex process on the first account.
    "$ne" env sh -c 'for pid in $(pgrep -f "[c]odex-tool-rows[.]py .*--listen"); do
        tr "\0" "\n" </proc/$pid/environ | grep -qx "CODEX_HOME=$HOME/.codex" && kill "$pid"
    done; :'
}

"$ne" cli project open "$CAGENTS_EVIDENCE_TASK/fixture/project" >/dev/null
"$ne" drive wait main --timeout 30 >/dev/null
"$ne" drive click main >/dev/null
"$ne" drive key ctrl+shift+m >/dev/null
"$ne" drive wait models --timeout 20 >/dev/null
"$ne" drive type codex >/dev/null
"$ne" drive key Return >/dev/null
"$ne" drive wait models --gone --timeout 20 >/dev/null
if "$ne" drive observe 2>/dev/null | rg -F 'Source control' >/dev/null; then
    "$ne" drive key ctrl+shift+i >/dev/null
fi
step draft
composer
"$ne" drive type 'Summarize the project layout.' >/dev/null
# The model chooser can still hold focus after the pick, so the message
# lands in its search instead; close it and type into the composer.
if "$ne" drive observe 2>/dev/null | rg -F 'No model matches' >/dev/null; then
    "$ne" drive key Escape >/dev/null
    "$ne" drive wait 'No model matches' --gone --timeout 10 >/dev/null
    composer
    "$ne" drive type 'Summarize the project layout.' >/dev/null
fi
"$ne" drive key Return >/dev/null
if ! "$ne" drive wait 'Done' --timeout 45 >/dev/null; then
    # Keep what the window showed, so an unsent first turn can be told
    # from one the peer never finished.
    step first-turn-missing
    echo "The first turn never finished; see $notes/first-turn-missing.txt" >&2
    exit 1
fi
sleep 2
shot first-turn

# The conversation's account is now out of usage; the other is available.
limits .codex false
refresh

composer
"$ne" drive type 'Now list the open questions.' >/dev/null
if [ "${VARIANT:-}" = diverged ]; then
    # Nothing holds the daemon, so its refusal arrives at once.
    diverge
    "$ne" drive key Return >/dev/null
    sleep 5
    shot refused
    # Open the report's details, which carry the daemon's own reason.
    at=$(locate 'Details')
    [ -n "$at" ] || { echo 'Could not locate the report details.' >&2; exit 1; }
    "$ne" drive click "$at" >/dev/null
    shot refused-details
    converge
    composer
    "$ne" drive key Return >/dev/null
    sleep 15
    shot sent-again
    exit 0
fi
hold
"$ne" drive key Return >/dev/null
sleep 5
shot waiting
# The switch waits at the gate for its 35 second deadline.
if [ "${VARIANT:-}" = exit ]; then
    peer_exits
fi
sleep 38
shot refused
[ "${VARIANT:-}" = exit ] && exit 0

# Send again while the daemon is still busy: the retry waits in turn.
composer
"$ne" drive key Return >/dev/null
sleep 5
shot retry-waiting
release
sleep 15
shot sent-again
