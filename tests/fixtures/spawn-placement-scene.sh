# Offline native scene for issue #2735, used identically on both revisions.
set -euo pipefail
fx=$(evidence env sh -c 'printf %s "$CAGENTS_EVIDENCE_FIXTURE"' | tail -1)
config=$fx/home/.claude
mkdir -p "$fx/casmi" "$config"
printf '1\n' >"$config/fixture-first-turn-seconds"
printf 'Fixture reply.\n' >"$config/fixture-reply.md"
rm -f "$config/fixture-run-once"
parent_tab() {
    evidence drive move 500,22 >/dev/null
    evidence drive scroll horizontal -100 >/dev/null
    sleep 1
    evidence drive click 310,22 >/dev/null
    sleep 2
}
shot() {
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}
# OCR can locate a row's text even when its action is an unlabelled glyph.
# The action stays at the row's trailing edge in both supported viewports.
open_point() {
    evidence drive observe | python3 -c 'import re,sys
width=int(sys.argv[1]); right=260+(width-270)//2; found=[]
for line in sys.stdin:
 m=re.search(r"\[t\d+\]\s+(\d+),\s+(\d+)\s+(\d+)x(\d+)",line)
 if m and 296<=int(m[1])<=325 and 120<int(m[2])<700 and any(name in line.lower() for name in ("reviewer", "casm")) and "cagents" not in line.lower():
  found.append(tuple(map(int,m.groups())))
assert found, "missing reviewer spawn row"
x,y,w,h=max(found,key=lambda row:row[1]); print(f"{right-26},{y+h//2}")' "${SCENE_WIDTH:-1500}"
}
open_spawn() {
    evidence drive click "$(open_point)"
}
assert_agent_focus() {
    evidence env python3 - "$1" <<'CHECK'
import os, sqlite3, sys
from pathlib import Path
fixture = Path(os.environ['CAGENTS_EVIDENCE_FIXTURE']).resolve()
root = Path(os.environ['XDG_DATA_HOME']).resolve() / 'cagents-development'
assert root.is_relative_to(fixture), 'focus checks require the private fixture'
model = sqlite3.connect(f'file:{root / "workspace-model.sqlite3"}?mode=ro', uri=True)
window = sqlite3.connect(f'file:{root / "sessions.sqlite3"}?mode=ro', uri=True)
agent = model.execute("SELECT id FROM agents WHERE json_extract(record, '$.name')=? ORDER BY rowid DESC LIMIT 1", (sys.argv[1],)).fetchone()
focus = [row[0] for row in window.execute('SELECT focused_card_id FROM workspaces')]
assert agent and agent[0] in focus, (sys.argv[1], 'did not receive keyboard activation', focus)
CHECK
}
evidence cli project open "$fx/project"
evidence cli project open "$fx/casmi"
channel=$(evidence cli --json channel spawn --repository "$fx/project" --title 'Backoff implementation' | jq -er '.data.channel.id')
spawn() {
    evidence cli --json agent spawn --provider claude --model haiku --surface chat --repository "$fx/project" --channel "$channel" "$@"
}
parent=$(spawn --name lead --prompt 'Review PR #2689 before it merges.' | jq -er '.data.agent.id')
sleep 3
spawn --name retry --prompt 'Review the retry change for restart safety.' >/dev/null
sleep 3
spawn --name jitter --prompt 'Check the jitter range against the spec.' >/dev/null
sleep 5
evidence drive observe
evidence drive key ctrl+shift+i
sleep 2
evidence drive click 'Backoff implementation'
sleep 2
parent_tab
sleep 4
evidence drive observe
shot spawn-start
if [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
    printf '300\n' >"$config/fixture-first-turn-seconds"
fi
printf '%s\n' "cagents agent spawn --name reviewer --model haiku --surface chat 'Review the one-row cards'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the one-row cards.'
sleep 10
evidence cli --json channel list
evidence cli --json agent list
evidence drive observe
shot same-project
if [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
    evidence drive move 500,22
    evidence drive scroll horizontal 100
    sleep 2
    shot same-project-pager-end
    parent_tab
    at=$(open_point)
    evidence drive move "$at"
    sleep 2
    evidence shot same-project-hover
    evidence drive move 1,1
    for _ in $(seq 1 "${SPAWN_ACTION_REVERSE_TABS:-3}"); do
        evidence drive key shift+Tab
    done
    sleep 1
    evidence shot same-project-keyboard
fi
if [ "$SIDE" = after ] || [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
    if [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
        evidence drive key Return
    else
        open_spawn
    fi
    sleep 3
    evidence drive observe
    if [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
        assert_agent_focus reviewer
    fi
    shot same-project-opened
fi
if [ "${LABEL_FOLLOWUP:-0}" = 1 ]; then
    # Open must put the keyboard in the destination's composer.
    evidence drive type 'Reviewer destination draft'
    evidence drive observe
    shot same-project-opened-keyboard
    evidence drive key ctrl+a
    evidence drive key BackSpace
fi
parent_tab
sleep 2
printf '%s\n' "cagents agent spawn --name casmi-reviewer --model haiku --surface chat --project '$fx/casmi' 'Review the casmi build'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the casmi build.'
sleep 10
evidence cli --json channel list
evidence cli --json agent list
evidence drive observe
shot cross-project
