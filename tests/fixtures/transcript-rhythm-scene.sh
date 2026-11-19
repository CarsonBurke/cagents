#!/bin/bash
# One long agentic Claude turn, for native evidence of the transcript's type
# size and vertical rhythm (#3119). Offline: the synthetic provider replays
# transcript-rhythm-scene/frames.jsonl, and no account or model is contacted.
#
# Run on the build host with CAGENTS_EVIDENCE_TASK naming a native-evidence
# task. The theme goes into the task's seed before a session starts; the
# scenes run inside a ready session:
#
#   tests/fixtures/transcript-rhythm-scene.sh theme dark   # or light
#   scripts/native-evidence start before --width 1200 --height 1000
#   tests/fixtures/transcript-rhythm-scene.sh wide dark
#   scripts/native-evidence stop
#   scripts/native-evidence start before --width 930 --height 1000
#   tests/fixtures/transcript-rhythm-scene.sh narrow dark
#   scripts/native-evidence stop
#
# The turn reads, searches and tests in one group of steps, starts four
# general-purpose agents, thinks without anything to read, starts a fifth,
# thinks again and runs two more commands, the sequence from the issue. It
# then holds for 30 seconds, working, before the agents report and the
# answer arrives.
#
# Scenes, each shooting with the side's prefix as native-evidence does, and
# with LABEL, when given, after "rhythm-":
#
#   wide    rhythm-working (the turn mid-way, its rows live) and
#           rhythm-finished (the same turn answered), then
#           rhythm-quiet-hover, the pointer over the report that has nothing
#           to open, and rhythm-report-open, one agent's report opened under
#           the pointer
#   narrow  rhythm-narrow-working and rhythm-narrow-finished: the same, at
#           two thirds of the strip beside a draft, in a window started 930
#           pixels wide; then rhythm-narrow-pane-finished, the conversation
#           at a third of it, 220 pixels
#   theme   KEY: writes the theme into the task's seed, which every later
#           session starts from
set -euo pipefail

scene=${1:?usage: transcript-rhythm-scene.sh wide|narrow [LABEL] | theme dark|light}
# Names the shots, such as rhythm-dark-working, so each theme keeps its own.
label=${2:+$2-}
here=$(cd -- "$(dirname -- "$0")" && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name a native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude
title='Sidebar directions'

if [ "$scene" = theme ]; then
    key=${2:?dark or light}
    case $key in dark|light) ;; *) exit 2 ;; esac
    # A session restores its fixture from the seed, so the theme goes there.
    settings=$CAGENTS_EVIDENCE_TASK/fixture-seed/config/cagents-development/settings.toml
    mkdir -p "${settings%/*}"
    if [ -f "$settings" ] && grep -q '^app_theme' "$settings"; then
        sed -i "s/^app_theme = .*/app_theme = \"$key\"/" "$settings"
    elif [ -f "$settings" ]; then
        sed -i "/^schema_version/a app_theme = \"$key\"" "$settings"
    else
        printf 'schema_version = 3\napp_theme = "%s"\n' "$key" >"$settings"
    fi
    exit 0
fi

# Never create a provider directory except inside an existing evidence fixture.
[ -d "$fixture/bin" ] && [ -d "$fixture/project/.git" ] && [ -d "$config" ] || {
    printf '%s\n' 'transcript-rhythm-scene: start native-evidence first' >&2
    exit 2
}

shot() {
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}

recorded() {
    rg -q "\"uuid\":\"$1\"" "$config/projects" 2>/dev/null
}

wait_recorded() {
    for _ in $(seq "$2"); do
        if recorded "$1"; then return 0; fi
        sleep 1
    done
    printf 'transcript-rhythm-scene: %s never arrived\n' "$1" >&2
    return 1
}

# A session that has only just started can miss the first shortcut. The
# palette is recognised by its placeholder or an entry, as text recognition
# reads one or the other depending on the theme.
open_channel() {
    local opened=false
    for _ in 1 2 3; do
        evidence drive key ctrl+shift+p >/dev/null
        for words in 'Search commands' 'Open settings'; do
            if evidence drive wait "$words" --timeout 3 >/dev/null 2>&1; then opened=true; fi
        done
        [ "$opened" = true ] && break
        # Close a palette that opened too late to be seen before trying again.
        evidence drive key Escape >/dev/null
        sleep 1
    done
    [ "$opened" = true ] || { printf '%s\n' 'transcript-rhythm-scene: command palette did not open' >&2; exit 1; }
    evidence drive type "# $title" >/dev/null
    evidence drive key Return >/dev/null
    sleep 2
}

# Where the first or last text run holding WORDS starts, as X,Y, from what the
# session sees now.
find_text() {
    evidence drive --json observe >"$fixture/state/transcript-rhythm-observed.json"
    /usr/bin/python3 -c '
import json, sys

def runs(node):
    if isinstance(node, dict):
        text = node.get("text")
        if isinstance(text, str) and sys.argv[2] in text and "x" in node and "y" in node:
            yield node
        for value in node.values():
            yield from runs(value)
    elif isinstance(node, list):
        for value in node:
            yield from runs(value)

found = sorted(runs(json.load(sys.stdin)), key=lambda run: run["y"])
if found:
    run = found[0] if sys.argv[1] == "first" else found[-1]
    print(run["x"] + 4, run["y"] + 4, sep=",")
' "$1" "$2" <"$fixture/state/transcript-rhythm-observed.json"
}

scroll_top() {
    evidence drive scroll --at 400,400 v -- -40 >/dev/null
    sleep 1
}

start_turn() {
    cp "$here/transcript-rhythm-scene/frames.jsonl" "$config/fixture-frames.jsonl.next"
    mv "$config/fixture-frames.jsonl.next" "$config/fixture-frames.jsonl"
    evidence cli project open "$fixture/project" >/dev/null
    channel=$(evidence cli --json channel spawn --repository "$fixture/project" --title "$title" |
        /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["channel"]["id"])')
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --channel "$channel" \
        --prompt 'Draft five sidebar directions and consolidate them.' \
        >"$fixture/state/transcript-rhythm-spawn.json"
    open_channel
}

# Two drafts beside the conversation, each cycled to a third of the strip.
# A preset is judged by the width it draws, so after one press each the
# conversation sits at two thirds, beside one draft: the split pane. Two
# more presses take it through the whole strip to a third, which in a
# window 930 pixels wide leaves it 220.
split_three() {
    evidence drive key ctrl+t >/dev/null
    sleep 2
    evidence drive key ctrl+t >/dev/null
    sleep 2
    for tab in 330,22 450,22 565,22; do
        evidence drive click "$tab" >/dev/null
        sleep 1
        evidence drive key alt+r >/dev/null
        sleep 1
    done
    evidence drive click 330,22 >/dev/null
    sleep 1
}

narrowest() {
    evidence drive click 330,22 >/dev/null
    sleep 1
    for _ in 1 2; do
        evidence drive key alt+r >/dev/null
        sleep 1
    done
}

# Shoots the turn while it works, once its last commands are recorded, and
# once it has answered.
working_and_finished() {
    wait_recorded 'rhythm-lint-1-result' 60
    sleep 3
    scroll_top
    shot "rhythm-${label}${1}working"
    wait_recorded 'rhythm-answer-1' 60
    sleep 3
    scroll_top
    shot "rhythm-${label}${1}finished"
}

case $scene in
wide)
    start_turn
    working_and_finished ''
    # The last agent reported nothing, so its row cannot open: the pointer
    # over it, which shows its copy and time.
    at=$(find_text last Consolidate)
    [ -n "$at" ] || { printf '%s\n' 'transcript-rhythm-scene: no empty report row' >&2; exit 1; }
    evidence drive move "$at" >/dev/null
    sleep 2
    evidence shot "rhythm-${label}quiet-hover"
    # The first agent's report opened, with the pointer over it. A report's
    # outcome is the only lowercase "finished" in the turn.
    at=$(find_text first finished)
    [ -n "$at" ] || { printf '%s\n' 'transcript-rhythm-scene: no report row' >&2; exit 1; }
    evidence drive click "$at" >/dev/null
    sleep 2
    evidence shot "rhythm-${label}report-open"
    ;;
narrow)
    start_turn
    split_three
    working_and_finished narrow-
    narrowest
    sleep 1
    scroll_top
    shot "rhythm-${label}narrow-pane-finished"
    ;;
*) exit 2 ;;
esac
