#!/bin/bash
# A Claude turn whose groups of steps failed in part, entirely, and across
# mixed kinds of step, for native evidence of how a collapsed group says how
# many of its steps failed. Offline: the synthetic provider replays
# step-failures-scene/frames.jsonl, and no account or model is contacted.
#
# Run on the build host with CAGENTS_EVIDENCE_TASK naming a native-evidence
# task, inside a ready session:
#
#   scripts/native-evidence start before --width 1200
#   tests/fixtures/step-failures-scene.sh show light
#   tests/fixtures/step-failures-scene.sh retheme dark
#   scripts/native-evidence stop
#
# The turn runs three commands of which one fails, says so, runs two that
# both fail, says so, then edits a file, fails to read another and runs a
# command, before it answers. Each stretch of work between the agent's words
# is a group of its own.
#
# Scenes, each shooting with the side's prefix as native-evidence does:
#
#   show LABEL    step-failures-LABEL: the answered turn with its three
#                 groups collapsed
#   retheme KEY   closes the app, sets the theme to KEY (dark or light),
#                 relaunches it in the same session, reopens the same
#                 conversation and shoots step-failures-KEY
#   theme KEY     writes the theme into the task's seed instead, which every
#                 later session starts from
set -euo pipefail

scene=${1:?usage: step-failures-scene.sh show LABEL | retheme KEY | theme KEY}
here=$(cd -- "$(dirname -- "$0")" && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name a native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude

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
case $scene in show | retheme) ;; *)
    printf 'step-failures-scene: unknown scene %s\n' "$scene" >&2
    exit 2
    ;;
esac
label=${2:?name the shot}

# Never create a provider directory except inside an existing evidence fixture.
[ -d "$fixture/bin" ] && [ -d "$fixture/project/.git" ] || {
    printf '%s\n' 'step-failures-scene: start native-evidence first' >&2
    exit 2
}
# The reply as the screen reading finds it, which splits a line into runs at
# whichever spaces it likes.
reply='type error is|checks cleanly|crate checks'

if [ "$scene" = show ]; then
    mkdir -p "$config"
    cp "$here/step-failures-scene/frames.jsonl" "$config/fixture-frames.jsonl"
    printf '%s\n' 'The type error is fixed and the crate checks cleanly.' >"$config/fixture-reply.md"
    # A fresh fixture has no project open yet; opening one again is harmless.
    evidence cli project open "$fixture/project" >/dev/null 2>&1 || true
    sleep 2
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --prompt 'Fix the failing library tests.' >/dev/null
else
    case $label in dark|light) ;; *) exit 2 ;; esac
    # Only this private native app receives a regular window-close request;
    # the app reads its theme only at startup.
    evidence nested niri msg action close-window >/dev/null
    sleep 4
    settings=$fixture/config/cagents-development/settings.toml
    mkdir -p "${settings%/*}"
    if [ -f "$settings" ] && grep -q '^app_theme' "$settings"; then
        sed -i "s/^app_theme = .*/app_theme = \"$label\"/" "$settings"
    elif [ -f "$settings" ]; then
        sed -i "1i app_theme = \"$label\"" "$settings"
    else
        printf 'schema_version = 3\napp_theme = "%s"\n' "$label" >"$settings"
    fi
    # The relaunch stays until the session stops; it holds no build lock.
    mkdir -p "$CAGENTS_EVIDENCE_TASK/logs"
    nohup "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" \
        --task "$CAGENTS_EVIDENCE_TASK" relaunch \
        </dev/null >"$CAGENTS_EVIDENCE_TASK/logs/step-failures-relaunch.log" 2>&1 &
    sleep 8
fi
# The conversation opens from its channel's row once its turn has answered;
# a busy host can take a minute or two.
for _ in $(seq 20); do
    sleep 4
    evidence drive hover 1,1 >/dev/null 2>&1
    sleep 1
    row=$(evidence drive observe 2>/dev/null | grep -E 'New channel|Fix the|failing library' | head -1 |
        sed -E 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/' |
        awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}')
    [ -n "$row" ] || continue
    evidence drive click "$row" >/dev/null 2>&1
    sleep 3
    if evidence drive observe 2>/dev/null | grep -qE "$reply"; then
        evidence drive hover 1,1 >/dev/null
        sleep 1
        evidence shot "step-failures-$label"
        exit 0
    fi
done
evidence shot "step-failures-$label-did-not-open" >/dev/null 2>&1 || true
printf '%s\n' 'step-failures-scene: the conversation did not open' >&2
exit 1
