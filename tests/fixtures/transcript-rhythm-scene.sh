#!/bin/bash
# One long agentic Claude turn, for native evidence of the transcript's type
# size and vertical rhythm (#3119). Offline: the synthetic provider replays
# transcript-rhythm-scene/frames.jsonl, and no account or model is contacted.
#
# Run inside a ready native-evidence session, on the build host:
#
#   tests/fixtures/transcript-rhythm-scene.sh theme dark   # or light, between sessions
#   scripts/native-evidence start before --width 1200 --height 1000
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/transcript-rhythm-scene.sh wide
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/transcript-rhythm-scene.sh narrow
#   scripts/native-evidence stop
#
# The turn reads, searches and tests in one group of steps, starts four
# general-purpose agents, thinks without anything to read, starts a fifth,
# thinks again and runs two more commands, the sequence from the issue. It
# then holds for 30 seconds, working, before the agents report and the
# answer arrives.
#
# Scenes, each shooting with the side's prefix as native-evidence does:
#
#   wide    rhythm-working (the turn mid-way, its rows live) and
#           rhythm-finished (the same turn answered)
#   narrow  rhythm-narrow: the finished turn in a pane about 220 pixels wide,
#           beside two others, after `wide` in the same session
#   theme   KEY: writes the fixture's theme setting, for the next session
set -euo pipefail

scene=${1:?usage: transcript-rhythm-scene.sh wide|narrow|theme dark|light}
here=$(cd -- "$(dirname -- "$0")" && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name a native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

if [ "$scene" = theme ]; then
    key=${2:?dark or light}
    case $key in dark|light) ;; *) exit 2 ;; esac
    settings=$fixture/config/cagents-development/settings.toml
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

scroll_top() {
    evidence drive scroll --at 400,400 v -- -40 >/dev/null
    sleep 1
}

case $scene in
wide)
    cp "$here/transcript-rhythm-scene/frames.jsonl" "$config/fixture-frames.jsonl.next"
    mv "$config/fixture-frames.jsonl.next" "$config/fixture-frames.jsonl"
    evidence cli project open "$fixture/project" >/dev/null
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" \
        --prompt 'Draft five sidebar directions and consolidate them.' \
        >"$fixture/state/transcript-rhythm-spawn.json"
    wait_recorded 'rhythm-lint-1-result' 60
    sleep 3
    scroll_top
    shot rhythm-working
    wait_recorded 'rhythm-answer-1' 60
    sleep 3
    scroll_top
    shot rhythm-finished
    ;;
narrow)
    # Two drafts beside the conversation, each pane cycled to a third of the
    # strip, leave the conversation about 220 pixels.
    evidence drive key ctrl+t >/dev/null
    sleep 2
    evidence drive key ctrl+t >/dev/null
    sleep 2
    scroll_top
    shot rhythm-narrow
    ;;
*) exit 2 ;;
esac
