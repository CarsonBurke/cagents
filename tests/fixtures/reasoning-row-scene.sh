#!/bin/bash
# Offline Claude reasoning rows in the actual native app. Run inside a ready
# native-evidence session; no live provider/account/model is contacted.
#
#   scripts/native-evidence start before --width 1200 --height 900
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh wide
#   scripts/native-evidence stop
#
# Repeat with the other side, or use narrow with a 900x900 viewport. For the
# remote pair route, a steps file can run:
#   CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" wide
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
prefix=
[ "$scene" = narrow ] && prefix=narrow-
evidence drive move 1,1 >/dev/null
evidence shot "${prefix}reasoning-active-empty"
# The old row is captioned Thinking; the new one contains its first line.
# Pick the first visible thought in reading order on either revision.
row=$(evidence drive --json observe | /usr/bin/python3 -c '
import json,sys
runs=json.load(sys.stdin)["runs"]
matches=[r for r in runs if "thinking" in r["text"].lower() or "assuming" in r["text"].lower()]
if not matches: raise SystemExit("reasoning-row-scene: no visible reasoning row")
r=min(matches,key=lambda r:(r["y"],r["x"]))
print("{},{}".format(round(r["x"]+r["width"]/2),round(r["y"]+r["height"]/2)))
')
evidence drive click "$row" >/dev/null
evidence drive wait 'new setting' --timeout 15 >/dev/null
evidence drive move 1,1 >/dev/null
evidence shot "${prefix}reasoning-expanded"
# Collapse the row while the provider remains active, then observe its later
# text update. The baseline keeps that text hidden; the fixed row shows its
# latest line while retaining the first line for the completed thoughts.
evidence drive click "$row" >/dev/null
evidence drive wait 'new setting' --gone --timeout 15 >/dev/null
sleep 44
evidence drive move 1,1 >/dev/null
evidence shot "${prefix}reasoning-working"
printf '%s\n' 'Captured active-empty, expanded and working/latest rows; stop the session now.'
