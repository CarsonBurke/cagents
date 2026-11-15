# Native-evidence pair steps for automatic-terminal completion persistence.
# Run with the development save-refusal hook documented in docs/fixtures.md.
set -eu

evidence cli daemon protocol-version
evidence cli project open '{fixture}/project'
evidence drive click main
if [ "${SCENE_HIDE_INSPECTOR:-0}" = 1 ]; then
    evidence drive key ctrl+shift+i
fi
evidence env python3 -c '
import os,pathlib
root=pathlib.Path(os.environ["CAGENTS_EVIDENCE_FIXTURE"])
(root/"home/trace-shell.py").write_text("""import os,pathlib,json
pid=os.getppid()
stat=pathlib.Path(f"/proc/{pid}/stat").read_text().split()
parent=int(stat[3])
if pathlib.Path(f"/proc/{parent}/comm").read_text().strip() != "cagents-holder":
 raise SystemExit(1)
with (pathlib.Path.home()/"terminal-workers.jsonl").open("a") as f:
 f.write(json.dumps({"pid":pid,"start":stat[21]})+"\\n")
""")
cfg=pathlib.Path(os.environ["XDG_CONFIG_HOME"])/"fish/config.fish"
cfg.parent.mkdir(parents=True,exist_ok=True)
cfg.write_text("if status is-interactive\n if python3 \"$HOME/trace-shell.py\"\n  echo Completion fixture running\n end\nend\nfunction fish_prompt\n printf \"fixture> \"\nend\n")
(root/"home/.config/fixture-pending-terminal-save-error").touch()
'
evidence drive key ctrl+shift+t
evidence drive wait "Completion fixture running" --timeout 30
# The fault must not prevent typing into the retained worker.
evidence drive click 410,240
evidence drive type 'echo usable-before-save'
evidence drive key Return
sleep 1
if [ "$SIDE" = after ]; then
    # Match the unique OCR word: the full button label is sometimes split
    # into separate Retry/save boxes. State assertions below verify the action.
    # A second refusal must keep the same surface mounted and one notice.
    evidence drive click Retry
    sleep 3
fi
evidence shot "${SCENE_SHOT_PREFIX:-}completion-save-error"
evidence env python3 -c '
import os,pathlib,sqlite3,json
root=pathlib.Path(os.environ["CAGENTS_EVIDENCE_FIXTURE"])
db=next(pathlib.Path(os.environ["XDG_DATA_HOME"]).rglob("workspace-model.sqlite3"))
workers=[json.loads(x) for x in (root/"home/terminal-workers.jsonl").read_text().splitlines()]
assert len(workers)==1, workers
panes=[list(row) for row in sqlite3.connect(db).execute("select id,resource,pending_terminal_start from panes where kind=\"terminal\"").fetchall()]
assert len(panes)==1 and panes[0][2]==1, panes
proof={"panes":panes,"workers":workers}
(root/"home/terminal-save-before.json").write_text(json.dumps(proof))
print(json.dumps(proof))
'
evidence env rm '{fixture}/home/.config/fixture-pending-terminal-save-error'
if [ "$SIDE" = after ]; then
    evidence drive click Retry
    evidence drive wait --gone "This terminal" --timeout 30
fi
# No click back into the terminal: successful Retry save restores keyboard focus.
evidence drive type 'echo usable-after-save'
evidence drive key Return
sleep 1
evidence shot "${SCENE_SHOT_PREFIX:-}completion-recovered"
evidence env python3 -c '
import os,pathlib,sqlite3,json,sys
root=pathlib.Path(os.environ["CAGENTS_EVIDENCE_FIXTURE"])
db=next(pathlib.Path(os.environ["XDG_DATA_HOME"]).rglob("workspace-model.sqlite3"))
workers=[json.loads(x) for x in (root/"home/terminal-workers.jsonl").read_text().splitlines()]
assert len(workers)==1, workers
worker=workers[0]
assert pathlib.Path("/proc/"+str(worker["pid"])+"/stat").read_text().split()[21]==worker["start"]
before=json.loads((root/"home/terminal-save-before.json").read_text())
panes=[list(row) for row in sqlite3.connect(db).execute("select id,resource,pending_terminal_start from panes where kind=\"terminal\"").fetchall()]
assert len(panes)==1 and panes[0][:2]==before["panes"][0][:2], panes
assert panes[0][2]==(0 if sys.argv[1]=="after" else 1), panes
assert workers==before["workers"], workers
print(json.dumps({"panes":panes,"workers":workers,"original_worker_alive":True}))
' "$SIDE"
