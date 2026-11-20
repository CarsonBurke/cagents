#!/bin/bash
# Native fixture scene for images pasted into the chat composer. Run inside a
# ready native-evidence session, or set CAGENTS_EVIDENCE_REMOTE to drive one
# over plain SSH:
#
#   CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/3259-tray \
#     tests/fixtures/composer-attachments-scene.sh open
#   tests/fixtures/composer-attachments-scene.sh paste 3 'Compare '
#   tests/fixtures/composer-attachments-scene.sh paste 2
#
# `open` starts an offline Claude chat and waits for its first turn to finish.
# `paste N TEXT` clicks the composer, replaces any draft with TEXT, then
# pastes N invented images, one at a time, through the nested compositor's
# private clipboard, as a person pasting screenshots would; each paste
# inserts its [Image N] reference at the caret. `paste N` without TEXT
# continues at the caret, leaving focus where the last paste left it, since
# a click into wrapped text would move the caret. Each image shows its own
# number, counted across the session, so the tiles can be told apart. `composer` prints the composer input's centre as
# X,Y. The images are synthetic gradients made with ImageMagick on the
# session's host; no model, network or host clipboard is used.
set -euo pipefail
mode=${1:?usage: composer-attachments-scene.sh open|paste N [TEXT]|composer}
here=$(cd "$(dirname "$0")" && pwd)
ne=${CAGENTS_EVIDENCE_COMMAND:-$here/../../scripts/native-evidence}
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
project=$CAGENTS_EVIDENCE_TASK/fixture/project
[ -z "${CAGENTS_EVIDENCE_REMOTE:-}" ] || project='{fixture}/project'

locate() {
    "$ne" drive observe 2>/dev/null |
        sed -nE "/$1/s/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+).*/\1 \2 \3 \4/p" |
        head -1 | awk '{printf "%d,%d", $1 + $3 / 2, $2 + $4 / 2}'
}
# The composer's + is the lowest text run that is only a +; the input starts
# just right of it.
composer() {
    "$ne" drive observe 2>/dev/null |
        sed -nE 's/^ *\[t[0-9]+\] +([0-9]+), +([0-9]+) +([0-9]+)x([0-9]+) +\+$/\1 \2 \3 \4/p' |
        sort -n -k2 | tail -1 | awk '{printf "%d,%d", $1 + 120, $2 + $4 / 2}'
}

case $mode in
open)
    "$ne" env sh -c 'mkdir -p "$HOME/.claude"; echo 1 >"$HOME/.claude/fixture-first-turn-seconds"'
    "$ne" cli project open "$project" >/dev/null
    "$ne" cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$project" --prompt 'Fixture: review the new settings screens.' >/dev/null
    for attempt in 1 2 3 4 5 6 7 8; do
        sleep 3
        row=$(locate 'New channel|review the new settings')
        [ -n "$row" ] || continue
        "$ne" drive click "$row" >/dev/null
        if "$ne" drive wait Finished --timeout 15 >/dev/null 2>&1; then
            exit 0
        fi
    done
    echo 'Fixture conversation did not open.' >&2
    exit 1
    ;;
paste)
    count=${2:?paste needs a count}
    text=${3:-}
    if [ -n "$text" ]; then
        at=$(composer)
        [ -n "$at" ] || { echo 'Could not locate the native chat composer.' >&2; exit 1; }
        "$ne" drive click "$at" >/dev/null
        "$ne" drive key ctrl+a BackSpace >/dev/null
        "$ne" drive type "$text" >/dev/null
    fi
    for _ in $(seq 1 "$count"); do
        "$ne" nested sh -c '
            counter=$XDG_RUNTIME_DIR/composer-scene-count
            number=$(( $(cat "$counter" 2>/dev/null || echo 0) + 1 ))
            echo "$number" >"$counter"
            hue=$(( (number * 47) % 360 ))
            image=$XDG_RUNTIME_DIR/composer-scene-$number.png
            magick -size 960x600 "gradient:hsl($hue,70%,35%)-hsl($(( (hue + 60) % 360 )),70%,60%)" \
                -fill white -pointsize 220 -gravity center -annotate +0+0 "$number" "$image"
            # wl-copy stays behind to serve the clipboard; it must not keep
            # the output of this command open, or the paste never returns.
            wl-copy --type image/png <"$image" >/dev/null 2>&1
        '
        "$ne" drive key ctrl+v >/dev/null
        sleep 1
        "$ne" drive type ' ' >/dev/null
    done
    ;;
composer)
    composer
    echo
    ;;
*) exit 2 ;;
esac
