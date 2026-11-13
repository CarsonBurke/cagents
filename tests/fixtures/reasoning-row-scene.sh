#!/bin/bash
# Offline Claude reasoning rows in the actual native app. Run inside a ready
# native-evidence session; no live provider/account/model is contacted.
#
#   scripts/native-evidence start before --width 1200 --height 900
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh wide
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh narrow
#   scripts/native-evidence stop
#
# Repeat both scenes with the other side at the same 1200x900 viewport. Narrow
# adds a blank thread beside the existing conversation and cycles both panes
# to one third, which shares the workspace into two roughly 270px panes. The
# inspector stays docked; no window resize or second provider is needed. For
# the remote pair route, run wide then narrow in the same steps file with
# CAGENTS_EVIDENCE_TASK=$task and "$tool/tests/fixtures/reasoning-row-scene.sh".
#
# The first turn streams two finished thoughts interleaved with command groups,
# a finished empty thought, then a live thought whose latest line changes.
# All thought and command frames use shapes consumed by Claude's wire adapter.
# A 40-second empty-live pause covers the no-text state. The final 300-second
# pause leaves the turn busy for interaction and capture;
# stop the evidence session afterward. Commands only describe synthetic output.
set -euo pipefail

scene=${1:?usage: reasoning-row-scene.sh wide|narrow}
case "$scene" in wide|narrow) ;; *) exit 2 ;; esac
here=$(cd -- "$(dirname -- "$0")" && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name a ready native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
config=$fixture/home/.claude
# Never create a provider directory except inside an existing evidence fixture.
[ -d "$fixture/bin" ] && [ -d "$fixture/project/.git" ] && [ -d "$config" ] || {
    printf '%s\n' 'reasoning-row-scene: start native-evidence first' >&2
    exit 2
}
wide_ready=$fixture/state/reasoning-row-scene-wide
narrow_done=$fixture/state/reasoning-row-scene-narrow

shot() {
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}

first_reasoning_row() {
    # The baseline caption says Thinking; the new row contains its first line.
    evidence drive --json observe | /usr/bin/python3 -c '
import json,sys
runs=json.load(sys.stdin)["runs"]
matches=[r for r in runs if "thinking" in r["text"].lower() or "assuming" in r["text"].lower()]
if not matches: raise SystemExit("reasoning-row-scene: no visible reasoning row")
r=min(matches,key=lambda r:(r["y"],r["x"]))
print("{},{}".format(round(r["x"]+r["width"]/2),round(r["y"]+r["height"]/2)))
'
}

expanded_shot() {
    local row
    row=$(first_reasoning_row)
    evidence drive click "$row" >/dev/null
    evidence drive wait 'new setting' --timeout 15 >/dev/null
    shot "$1"
    evidence drive click "$row" >/dev/null
    evidence drive wait 'new setting' --gone --timeout 15 >/dev/null
}

if [ "$scene" = narrow ]; then
    [ -f "$wide_ready" ] && [ ! -e "$narrow_done" ] || {
        printf '%s\n' 'reasoning-row-scene: run wide once in this session before narrow' >&2
        exit 2
    }
    # Ctrl+T creates an unsent draft. The hosted chat stores the default half
    # width; a new draft in this compact workspace stores a full-width choice.
    evidence drive key ctrl+t >/dev/null
    sleep 2
    # While its neighbour stays full, cycling the first pane advances its
    # stored preference: half -> two thirds -> full -> one third. The second
    # pane then needs one cycle from full to one third; the pair fits and the
    # strip expands their spare space equally.
    evidence drive key alt+1 >/dev/null
    for _ in 1 2 3; do
        evidence drive key alt+r >/dev/null
        sleep 1
    done
    evidence drive key alt+2 >/dev/null
    evidence drive key alt+r >/dev/null
    sleep 1
    evidence drive key alt+1 >/dev/null
    sleep 1
    shot reasoning-narrow-working
    expanded_shot reasoning-narrow-expanded
    touch "$narrow_done"
    printf '%s\n' 'Captured narrow working and expanded rows; stop the evidence session now.'
    exit 0
fi

cp "$here/reasoning-row-scene/frames.jsonl" "$config/fixture-frames.jsonl"
evidence cli project open "$fixture/project" >/dev/null
evidence cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository "$fixture/project" --prompt 'Fixture: retry the permission probe and verify cleanup.' >/dev/null

# Open the new channel after the provider starts. Its sidebar label may
# already contain the prompt, depending on when title preparation finishes.
opened=false
for _ in 1 2 3 4 5 6 7 8; do
    sleep 3
    evidence drive move 1,1 >/dev/null
    if evidence drive click 'New channel' >/dev/null 2>&1 ||
        evidence drive click 'Fixture:' >/dev/null 2>&1; then
        sleep 2
        opened=true
        break
    fi
done
[ "$opened" = true ] || { printf '%s\n' 'reasoning-row-scene: new channel was not visible' >&2; exit 1; }
# The baseline hides live text when collapsed, so wait on the provider's
# persisted command result rather than text that only the fixed row reveals.
ready=false
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if rg -q '"uuid":"reasoning-command-five-1-result"' "$config/projects"; then
        ready=true
        break
    fi
    sleep 2
done
[ "$ready" = true ] || { printf '%s\n' 'reasoning-row-scene: command stream did not finish' >&2; exit 1; }
# The live thought starts empty and stays that way for 40 seconds.
sleep 2
shot reasoning-active-empty
expanded_shot reasoning-expanded
# The baseline hides live text when collapsed. The fixed row shows the later
# latest-line delta while keeping the first line for completed thoughts.
sleep 44
shot reasoning-working
touch "$wide_ready"
printf '%s\n' 'Captured wide rows; run narrow in this same session, then stop it.'
