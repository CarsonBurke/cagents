#!/bin/bash
# Source through native-evidence's evidence() callback. Both sides need the
# pane Attention rows and `agent spawn --worktree` introduced by #2806.
set -euo pipefail
fx=$(evidence env sh -c 'printf %s "$CAGENTS_EVIDENCE_FIXTURE"' | tail -1)
config=$fx/home/.claude
mkdir -p "$config"
printf '1\n' >"$config/fixture-first-turn-seconds"
printf 'Fixture reply.\n' >"$config/fixture-reply.md"
rm -f "$config/fixture-run-once"
evidence cli project open "$fx/project"
sleep 3
evidence drive click main
sleep 5
spawn() {
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --project "$fx/project" --worktree "$fx/project" "$@"
}
for name in lead retry jitter; do
    spawn --name "$name" "Review the $name task" >/dev/null
    sleep 3
done
# Keep the first two panes visible; the fourth agent must remain unopened.
evidence drive move 500,22
evidence drive scroll horizontal -100
evidence drive click 310,22
printf '300\n' >"$config/fixture-first-turn-seconds"
spawn --name reviewer 'Review the one-row cards' >/dev/null
sleep 10
evidence drive click 174,66
sleep 2
evidence env python3 - "$SIDE" <<'CHECK'
import json, os, sqlite3, sys
from pathlib import Path
fixture = Path(os.environ['CAGENTS_EVIDENCE_FIXTURE']).resolve()
root = Path(os.environ['XDG_DATA_HOME']).resolve() / 'cagents-development'
assert root.is_relative_to(fixture), 'requires a private fixture'
model = sqlite3.connect(f'file:{root / "workspace-model.sqlite3"}?mode=ro', uri=True)
window = sqlite3.connect(f'file:{root / "sessions.sqlite3"}?mode=ro', uri=True)
agent, record = model.execute("SELECT id,record FROM agents WHERE json_extract(record,'$.name')='reviewer'").fetchone()
focus = [value for value, in window.execute('SELECT focused_card_id FROM workspaces')]
assert agent not in focus, 'the reviewer must remain unopened'
title, = window.execute('SELECT title FROM cards WHERE card_id=?', (agent,)).fetchone()
expected = 'reviewer · Review the one-row cards' if sys.argv[1] == 'after' else 'Claude Code agent'
assert title == expected, (title, expected)
print('sidebar-unloaded-state ' + json.dumps({'agent': agent, 'placement': json.loads(record)['placement'], 'focus': focus, 'title': title}), flush=True)
CHECK
evidence drive move 1,1
evidence shot label-sidebar-unloaded
point=$(evidence drive observe | python3 -c 'import re,sys
found=[]
for line in sys.stdin:
 m=re.search(r"\[t\d+\]\s+(\d+),\s+(\d+)\s+(\d+)x(\d+)",line)
 if m and int(m[1])<120 and 80<int(m[2])<750 and ("reviewer" in line.lower() or "claude code agent" in line.lower()):
  found.append(tuple(map(int,m.groups())))
assert found, "missing unopened reviewer Attention row"
x,y,w,h=max(found,key=lambda row:row[1]); print(f"{min(200,x+w//2)},{y+h//2}")')
evidence drive move "$point"
sleep 2
evidence shot label-sidebar-hover
