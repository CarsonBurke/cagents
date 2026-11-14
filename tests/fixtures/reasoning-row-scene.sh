#!/bin/bash
# Offline Claude reasoning rows in the actual native app. Run inside a ready
# native-evidence session; no live provider/account/model is contacted.
#
#   scripts/native-evidence start before --width 1200 --height 900 --ui-trace --codex tool-rows
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh wide
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh narrow
#   CAGENTS_EVIDENCE_TASK=... tests/fixtures/reasoning-row-scene.sh web
#   scripts/native-evidence stop
#
# Repeat both scenes with the other side at the same 1200x900 viewport. Narrow
# adds a blank thread beside the existing conversation and cycles both panes
# to one third, which shares the workspace into two roughly 270px panes. The
# inspector stays docked; no window resize or second provider is needed. For
# the remote pair route, run wide then narrow in the same steps file with
# CAGENTS_EVIDENCE_TASK=$task and "$tool/tests/fixtures/reasoning-row-scene.sh".
#
# The first turn interleaves reasoning with successful commands, read/search/
# edit/tool steps, a failed command, and an acknowledged background command.
# It retains finished-empty and active-empty reasoning and a live latest line.
# All thought and command frames use shapes consumed by Claude's wire adapter.
# A 40-second empty-live pause covers the no-text state. The final 600-second
# pause leaves the turn busy for interaction and capture;
# stop the evidence session afterward. Commands only describe synthetic output.
set -euo pipefail

scene=${1:?usage: reasoning-row-scene.sh wide|narrow|web}
case "$scene" in wide|narrow|web) ;; *) exit 2 ;; esac
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
    evidence drive wait afterward --timeout 15 >/dev/null
    shot "$1"
    # Expanding in a narrow pane can move the header as its scroll anchor
    # settles. Locate it again rather than clicking the old body position.
    row=$(first_reasoning_row)
    evidence drive click "$row" >/dev/null
    evidence drive wait afterward --gone --timeout 15 >/dev/null
}

# OCR references are derived again after every disclosure or scroll, since
# native layout can move the header. Overlapping OCR runs on one line count once.
locate_row() {
    evidence drive --json observe | /usr/bin/python3 -c '
import json,re,sys
pattern=re.compile(sys.argv[1],re.I);index=int(sys.argv[2])
runs=sorted((r for r in json.load(sys.stdin)["runs"]
             if 270 <= r["x"] < 810 and 55 < r["y"] < 780 and pattern.search(r["text"])),
            key=lambda r:(r["y"],r["x"]))
rows=[]
for r in runs:
    if not rows or r["y"]-rows[-1]["y"]>8: rows.append(r)
if len(rows)<=index: raise SystemExit(1)
r=rows[index]
print("{},{}".format(round(r["x"]+r["width"]/2),round(r["y"]+r["height"]/2)))
' "$1" "${2:-0}"
}

scroll_top() {
    evidence drive scroll --at 420,400 v -- -30 >/dev/null
    sleep 1
}

find_row() {
    local point
    if point=$(locate_row "$1" "${2:-0}"); then printf '%s\n' "$point"; return; fi
    scroll_top
    for _ in 1 2 3 4 5 6; do
        if point=$(locate_row "$1" "${2:-0}"); then printf '%s\n' "$point"; return; fi
        evidence drive scroll --at 420,400 v -- 5 >/dev/null
        sleep 1
    done
    printf 'reasoning-row-scene: row not visible: %s\n' "$1" >&2
    return 1
}

click_row() {
    local point
    point=$(find_row "$1" "${2:-0}")
    evidence drive click "$point" >/dev/null
    sleep 1
}

text_visible() {
    locate_row "$1" >/dev/null
}

# Focus the two native tabs, rather than sending Alt+1/Alt+2 to the draft.
narrow_layout() {
    # Ctrl+T creates an unsent draft. The hosted chat stores the default half
    # width; a new draft in this compact workspace stores a full-width choice.
    evidence drive key ctrl+t >/dev/null
    sleep 2
    # This scene fixes the window at 1200x900. Focus its native tabs directly
    # so nested-compositor shortcuts cannot type into the empty draft.
    # While its neighbour stays full, cycling the first pane advances its
    # stored preference: half -> two thirds -> full -> one third. The second
    # pane then needs one cycle from full to one third; the pair fits and the
    # strip expands their spare space equally.
    evidence drive click 390,22 >/dev/null
    sleep 1
    for _ in 1 2 3; do
        evidence drive key alt+r >/dev/null
        sleep 1
    done
    evidence drive click 650,22 >/dev/null
    sleep 1
    evidence drive key alt+r >/dev/null
    sleep 1
    evidence drive click 390,22 >/dev/null
    sleep 1
    sleep 1
}

# A single expanded mixed group covers Read/Search, Changes and a generic MCP
# tool. The latter's literal input/output verifies its own nested disclosure.
tools_details() {
    local prefix=$1
    scroll_top
    click_row '^edited'
    if ! text_visible '\bInspection\b'; then click_row 'inspect'; fi
    evidence drive wait Inspection --timeout 15 >/dev/null
    shot "${prefix}rows-tools-expanded"
    click_row '^edited'
    scroll_top
}

# Keep the failed command's original diagnostic alongside a running group and
# its nested success/running commands. No command is executed by this fixture.
outcome_details() {
    local prefix=$1
    scroll_top
    click_row 'optional-probe-denied'
    if ! text_visible '\bRefused\b'; then click_row 'optional-probe-denied' 1; fi
    evidence drive wait Refused --timeout 15 >/dev/null
    click_row '(ran|running) 2 commands'
    local point y steps
    point=$(find_row 'optional-probe-denied')
    y=${point#*,}
    if [ "$y" -gt 200 ]; then
        steps=$(( (y - 160) / 60 ))
        evidence drive scroll --at 420,400 v -- "$steps" >/dev/null
        sleep 1
    fi
    shot "${prefix}rows-outcomes-expanded"
    click_row '(ran|running) 2 commands'
    click_row 'optional-probe-denied'
    scroll_top
}

if [ "$scene" = web ]; then
    [ -x "$fixture/bin/codex" ] && [ -d "$fixture/home/.codex" ] || {
        printf '%s\n' 'reasoning-row-scene: start with --codex tool-rows for web' >&2
        exit 2
    }
    [ -f "$narrow_done" ] || { printf '%s\n' 'run wide and narrow before web' >&2; exit 2; }
    # Close the two cards; their provider conversations remain in the sidebar.
    evidence drive key ctrl+w >/dev/null
    sleep 1
    evidence drive key ctrl+w >/dev/null
    sleep 1
    touch "$fixture/home/.codex/fixture-web-rows"
    evidence cli --json agent spawn --provider codex --model fixture-mini --surface chat \
        --repository "$fixture/project" --prompt 'Web references: inspect synthetic search results.' >/dev/null
    opened=false
    for _ in 1 2 3 4 5 6 7 8; do
        sleep 2
        evidence drive move 1,1 >/dev/null
        if evidence drive click 'Web references' >/dev/null 2>&1 ||
            evidence drive click 'Offline tool rows' >/dev/null 2>&1 ||
            evidence drive click 'New channel' >/dev/null 2>&1; then
            opened=true
            break
        fi
    done
    [ "$opened" = true ] || { printf '%s\n' 'web conversation did not open' >&2; exit 1; }
    evidence drive wait 'Web references ready' --timeout 30 >/dev/null
    narrow_layout
    scroll_top
    shot rows-web-collapsed
    click_row '^searched the web'
    click_row temporary
    evidence drive wait Recorded --timeout 15 >/dev/null
    shot rows-web-expanded
    printf '%s\n' 'Captured dedicated Codex web-search rows.'
    exit 0
fi

if [ "$scene" = narrow ]; then
    [ -f "$wide_ready" ] && [ ! -e "$narrow_done" ] || {
        printf '%s\n' 'reasoning-row-scene: run wide once in this session before narrow' >&2
        exit 2
    }
    narrow_layout
    scroll_top
    shot reasoning-narrow-working
    expanded_shot reasoning-narrow-expanded
    tools_details narrow-
    outcome_details narrow-
    touch "$narrow_done"
    printf '%s\n' 'Captured narrow reasoning, tools and outcomes; web may run next.'
    exit 0
fi

cp "$here/reasoning-row-scene/frames.jsonl" "$config/fixture-frames.jsonl.next"
mv "$config/fixture-frames.jsonl.next" "$config/fixture-frames.jsonl"
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
    if rg -q '"uuid":"rows-running-1-ack"' "$config/projects"; then
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
tools_details ""
outcome_details ""
touch "$wide_ready"
printf '%s\n' 'Captured wide reasoning, tools and outcomes; run narrow in this session.'
