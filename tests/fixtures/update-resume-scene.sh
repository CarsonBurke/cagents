#!/bin/bash
# Offline native scene for issue #3094: the offer, after an update, to resume
# the conversations whose work in flight the update stopped.
#
# Run as a native-evidence steps file, with `evidence` standing for
# scripts/native-evidence:
#   prepare  in a running session: opens the fixture project and starts four
#            offline Claude agents in one channel. Then `stop` and `save-seed`.
#   shots    in a session started from that seed: records an ended update
#            whose census says three of the conversations lost their work,
#            restarts only the app window so it reads that update as a window
#            opened after an update does, and captures the offer. Where the
#            offer exists it then resumes one conversation, shows the message
#            that conversation received and dismisses the rest, capturing each
#            state.
#
# A session's start reopens the seeded conversations under new ids, which the
# update names, so the update is recorded within the session that shows it.
set -euo pipefail
scene=${1:?usage: update-resume-scene.sh prepare|shots}
fx=$(evidence env sh -c 'printf %s "$CAGENTS_EVIDENCE_FIXTURE"' | tail -1)
config=$fx/home/.claude
channel_title='Release checks'

shot() {
    evidence drive hover 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}

# The centre of the last OCR line reading exactly $1.
exact() {
    evidence drive observe 2>/dev/null | python3 -c 'import re,sys
want=sys.argv[1]; found=None
for line in sys.stdin:
 m=re.search(r"\[t\d+\]\s+(\d+),\s+(\d+)\s+(\d+)x(\d+)\s+(.*)$",line)
 if m and m[5].strip()==want:
  found=tuple(map(int,m.groups()[:4]))
assert found, f"no {want!r} on screen"
x,y,w,h=found; print(f"{x+w//2},{y+h//2}")' "$1"
}

record_update() {
    evidence env python3 - "$fx/home/.config/fixture-update-review.json" <<'FIXTURE'
import json, os, sqlite3, sys
from pathlib import Path
fixture = Path(os.environ['CAGENTS_EVIDENCE_FIXTURE']).resolve()
root = Path(os.environ['XDG_DATA_HOME']).resolve() / 'cagents-development'
assert root.is_relative_to(fixture), 'the scene writes only into its private fixture'
model = sqlite3.connect(f'file:{root / "workspace-model.sqlite3"}?mode=ro', uri=True)
ids = dict(model.execute(
    "SELECT json_extract(record, '$.name'), session_id FROM agents WHERE closed=0"))
for name in ('login', 'ci', 'settings', 'notes'):
    assert ids.get(name), f'agent {name} has no conversation yet'
def task(id, kind, name):
    return {'id': id, 'kind': kind, 'name': name, 'waiting': False}
in_flight = [
    # Restarted without its subagent and background command.
    {'id': ids['login'], 'label': 'Fix the login redirect loop', 'fate': 'restarted',
     'tasks': [task('toolu_review', 'subagent', 'Review the redirect fix'),
               task('bash_7', 'command', 'cargo test -p auth')]},
    # Its turn was interrupted.
    {'id': ids['ci'], 'label': 'Watch CI on the release branch', 'fate': 'interrupted',
     'turn': True},
    # Ended by the update with a queued message undelivered.
    {'id': ids['settings'], 'label': 'Rename the old settings keys', 'fate': 'ended',
     'turn': True, 'queued': ['queued-1']},
    # Kept running through the update: not offered.
    {'id': ids['notes'], 'label': 'Draft the release notes', 'fate': 'continued',
     'tasks': [task('toolu_notes', 'subagent', 'Collect merged PRs')]},
]
job = {
    'id': 'fixture-update-review',
    'version': '0.0.0-fixture',
    'phase': 'Complete',
    'message': 'Updated.',
    'log_path': str(fixture / 'update.log'),
    'in_flight': in_flight,
}
path = Path(sys.argv[1])
path.write_text(json.dumps(job, indent=2))
path.chmod(0o600)
FIXTURE
}

case $scene in
prepare)
    mkdir -p "$fx/home/.config" "$config"
    printf '1\n' >"$config/fixture-first-turn-seconds"
    printf 'Checked where things stood and carried on.\n' >"$config/fixture-reply.md"
    evidence cli project open "$fx/project"
    channel=$(evidence cli --json channel spawn --repository "$fx/project" \
        --title "$channel_title" | jq -er '.data.channel.id')
    spawn() {
        evidence cli --json agent spawn --provider claude --surface chat \
            --repository "$fx/project" --channel "$channel" "$@" >/dev/null
        sleep 3
    }
    spawn --name login --prompt 'Fix the login redirect loop.'
    spawn --name ci --prompt 'Watch CI on the release branch.'
    spawn --name settings --prompt 'Rename the old settings keys.'
    spawn --name notes --prompt 'Draft the release notes.'
    sleep 5
    ;;
shots)
    record_update
    # Only this private native app receives a regular window-close request.
    evidence nested niri msg action close-window >/dev/null
    sleep 4
    nohup "$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" relaunch \
        </dev/null >"$CAGENTS_EVIDENCE_TASK/logs/update-resume-relaunch.log" 2>&1 &
    evidence drive wait "$channel_title" --timeout 60 >/dev/null
    sleep 3
    evidence drive click "$channel_title"
    sleep 3
    shot update-resume-offer
    if evidence drive observe 2>/dev/null | grep -q 'Resume all'; then
        # The last Resume belongs to the settings conversation.
        evidence drive click "$(exact Resume)"
        sleep 6
        shot update-resume-one-resumed
        # Its tab, third in the 1500-pixel session; OCR misreads the title.
        evidence drive click 560,22 >/dev/null
        sleep 3
        shot update-resume-continued
        # The offer's close button sits just right of Resume all.
        all=$(exact 'Resume all')
        evidence drive click "$((${all%,*} + 68)),${all#*,}"
        sleep 2
        shot update-resume-dismissed
    fi
    ;;
*)
    exit 2
    ;;
esac
