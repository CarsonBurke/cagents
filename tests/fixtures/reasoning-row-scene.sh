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

scene=${1:?usage: reasoning-row-scene.sh wide|narrow|web|coordination|diagnostics}
case "$scene" in wide|narrow|web|coordination|diagnostics) ;; *) exit 2 ;; esac
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
web_done=$fixture/state/reasoning-row-scene-web
coordination_done=$fixture/state/reasoning-row-scene-coordination
initial_spawn=$fixture/state/reasoning-row-scene-spawn.json

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
matches=[r for r in runs if "thinking" in r["text"].lower() or "ssuming" in r["text"].lower()]
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
             if 270 <= r["x"] < 810 and 55 < r["y"] < 780 and (pattern.search(r["text"]) or pattern.search(re.sub(r"^[^A-Za-z0-9]+", "", r["text"])))),
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
    evidence drive scroll --at 280,400 v -- -30 >/dev/null
    sleep 1
}

find_row() {
    local point
    if point=$(locate_row "$1" "${2:-0}"); then printf '%s\n' "$point"; return; fi
    scroll_top
    for _ in 1 2 3 4 5 6; do
        if point=$(locate_row "$1" "${2:-0}"); then printf '%s\n' "$point"; return; fi
        evidence drive scroll --at 280,400 v -- 5 >/dev/null
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
align_row() {
    local point y steps
    point=$(find_row "$1" "${2:-0}")
    y=${point#*,}
    if [ "$y" -gt 200 ]; then
        steps=$(( (y - 140) / 50 ))
        evidence drive scroll --at 280,400 v -- "$steps" >/dev/null
        sleep 1
    fi
}

tools_details() {
    local prefix=$1
    scroll_top
    if ! text_visible '\bRead\b'; then click_row '\bEdited\b'; fi
    align_row '^Used\b'
    if ! text_visible '\bInspection\b'; then click_row '^Used\b'; fi
    find_row '\bInspection\b' >/dev/null
    shot "${prefix}rows-tools-expanded"
    # Restore both disclosure levels; opening the next scene must not depend
    # on whether a long output happened to remain outside the viewport.
    click_row '^Used\b'
    scroll_top
    click_row '\bEdited\b'
    scroll_top
}

# Keep the failed command's original diagnostic alongside a running group and
# its nested success/running commands. No command is executed by this fixture.
outcome_details() {
    local prefix=$1 mixed_parent=false failure_pattern='exit ?1' failure_index=0
    scroll_top
    # The baseline filters the empty thought and merges this failure into
    # the mixed group. Newer UI preserves the thought and its group boundary.
    if ! text_visible '(Ran printf|Tried to run)'; then
        click_row '\bEdited\b'
        mixed_parent=true
    else
        click_row '(Ran printf|Tried to run)'
        # New headers keep one full preview; a long command can fade the
        # appended exit status, so use the common outer/nested prefix.
        failure_pattern='(Ran printf|Tried to run)'
        failure_index=1
    fi
    align_row "$failure_pattern" "$failure_index"
    if ! text_visible '\bRefused\b'; then click_row "$failure_pattern" "$failure_index"; fi
    find_row '\bRefused\b' >/dev/null
    # OCR may split the leading verb from the stable count.
    if ! text_visible 'fixture.log.ready'; then click_row '2 commands'; fi
    local point y steps
    point=$(find_row "$failure_pattern" "$failure_index")
    y=${point#*,}
    if [ "$y" -gt 200 ]; then
        steps=$(( (y - 160) / 60 ))
        evidence drive scroll --at 280,400 v -- "$steps" >/dev/null
        sleep 1
    fi
    shot "${prefix}rows-outcomes-expanded"
    click_row '2 commands'
    click_row "$failure_pattern" "$failure_index"
    scroll_top
    if [ "$mixed_parent" = true ]; then click_row '\bEdited\b'; else click_row '(Ran printf|Tried to run)'; fi
    scroll_top
}

# Every actor uses the initial fixture channel explicitly. Without a caller's
# shared view, agent spawn otherwise makes another New channel hierarchy.
scene_channel() {
    /usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["agent"]["placement"]["Channel"])' "$initial_spawn"
}

close_actor() {
    local source=$1 has_draft=$2 agent
    agent=$(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["agent"]["id"])' "$source")
    # Closing is authorized fixture cleanup; the actor and every provider home
    # belong to native-evidence. It does not reach real provider sessions.
    evidence cli agent close "$agent" >/dev/null
    sleep 1
    if [ "$has_draft" = true ]; then
        # The remaining unsent draft expands to the550px docked workspace.
        # Its native tab Close target is independent of composer key handling.
        evidence drive click 678,22 >/dev/null
        sleep 1
    fi
}

spawn_scene() {
    local provider=$1 model=$2 prompt=$3 receipt=$4
    evidence cli --json agent spawn --provider "$provider" --model "$model" --surface chat \
        --repository "$fixture/project" --channel "$(scene_channel)" --prompt "$prompt" >"$receipt"
}

scroll_bottom() {
    evidence drive scroll --at 280,400 v -- 40 >/dev/null
    sleep 1
}

copy_contains() {
    # This reads only the clipboard belonging to native-evidence's nested
    # compositor. Never use a clipboard command in the invoking host shell.
    evidence nested wl-paste --no-newline | /usr/bin/python3 -c '
import sys
value=sys.stdin.read()
if sys.argv[1] not in value: raise SystemExit("private clipboard did not retain "+sys.argv[1])
print("Verified private clipboard contains "+sys.argv[1])
' "$1"
}

if [ "$scene" = coordination ]; then
    [ -f "$web_done" ] || { printf '%s\n' 'run web before coordination' >&2; exit 2; }
    close_actor "$fixture/state/reasoning-row-scene-web-spawn.json" true
    cp "$here/reasoning-row-scene/coordination.jsonl" "$config/fixture-frames.jsonl.next"
    mv "$config/fixture-frames.jsonl.next" "$config/fixture-frames.jsonl"
    spawn_scene claude haiku 'Coordination fixture: review the recorded transcript controls.' \
        "$fixture/state/reasoning-row-scene-coordination-spawn.json"
    evidence drive wait 'Coordination controls ready' --timeout 30 >/dev/null
    scroll_top
    shot rows-coordination-folded
    click_row '^Show full prompt$'
    find_row PROMPT_END >/dev/null
    shot rows-coordination-prompt-expanded
    click_row '^Show less$'
    scroll_top
    click_row '^Show tool call\b'
    find_row '^Input$' >/dev/null
    shot rows-coordination-tool-expanded
    # The raw input retains its own hover Copy action. The reviewer also
    # exercises it and Transcript in the live native handoff.
    click_row '^Hide tool call\b'
    click_row '^Show full message$'
    find_row MESSAGE_END >/dev/null
    shot rows-coordination-message-expanded
    click_row '^Show less$'
    scroll_bottom
    if text_visible '^Details$'; then
        # The baseline keeps a plain heading and separate Details button.
        click_row '^Details$'
    else
        # The report title and outcome are independent row cells. Match
        # the stable title; verify the original full body below.
        click_row 'Agent.*Review fixture'
    fi
    find_row REPORT_END >/dev/null
    shot rows-report-expanded
    if text_visible '^Copy report$'; then
        click_row '^Copy report$'
        copy_contains REPORT_END
    fi
    touch "$coordination_done"
    printf '%s\n' 'Captured coordination folds, raw call and delivered report.'
    exit 0
fi

if [ "$scene" = diagnostics ]; then
    [ -f "$coordination_done" ] || { printf '%s\n' 'run coordination before diagnostics' >&2; exit 2; }
    close_actor "$fixture/state/reasoning-row-scene-coordination-spawn.json" false
    touch "$fixture/home/.codex/fixture-row-diagnostics"
    spawn_scene codex fixture-mini 'Diagnostic fixture: inspect the original synthetic failure.' \
        "$fixture/state/reasoning-row-scene-diagnostics-spawn.json"
    evidence drive wait 'Copy details' --timeout 30 >/dev/null
    scroll_top
    shot rows-diagnostic-collapsed
    click_row '^Details$'
    evidence drive wait 'fixture-original-diagnostic' --timeout 15 >/dev/null
    shot rows-diagnostic-expanded
    click_row '^Copy details$'
    copy_contains fixture-original-diagnostic
    click_row '^Hide details$'
    printf '%s\n' 'Captured original failure Details and verified private Copy details.'
    exit 0
fi

if [ "$scene" = web ]; then
    [ -x "$fixture/bin/codex" ] && [ -d "$fixture/home/.codex" ] || {
        printf '%s\n' 'reasoning-row-scene: start with --codex tool-rows for web' >&2
        exit 2
    }
    [ -f "$narrow_done" ] || { printf '%s\n' 'run wide and narrow before web' >&2; exit 2; }
    close_actor "$initial_spawn" true
    touch "$fixture/home/.codex/fixture-web-rows"
    spawn_scene codex fixture-mini 'Web references: inspect synthetic search results.' \
        "$fixture/state/reasoning-row-scene-web-spawn.json"
    evidence drive wait 'Web references ready' --timeout 30 >/dev/null
    narrow_layout
    scroll_top
    shot rows-web-collapsed
    click_row '\bsearched the web'
    # A one-step group repeats the nested row title. Target the second
    # matching line, even when a narrow fade conceals the query itself.
    click_row '\bsearched the web' 1
    find_row '\bRecorded\b' >/dev/null
    shot rows-web-expanded
    touch "$web_done"
    printf '%s\n' 'Captured dedicated Codex web-search rows; coordination may run next.'
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
    --repository "$fixture/project" --prompt 'Fixture: retry the permission probe and verify cleanup.' >"$initial_spawn"

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
