#!/usr/bin/env python3
"""Write a Claude Code conversation whose transcript was damaged, for resuming.

    claude-damaged-history.py PROJECT [--home HOME] [--hours-ago N]

PROJECT is the directory the conversation ran in. The transcript goes where
Claude Code keeps it, under HOME/.claude/projects (HOME defaults to $HOME, so
run it through `scripts/native-evidence env` to write into a fixture). It
prints the transcript's path. Every name and message is invented.

The transcript holds what a crash and a large tool output leave, laid out as
Claude Code writes its records: parent and branch first, then the message,
and its type and UUID after it.

- A prompt cut short mid-record by a crash. On resume Claude Code went on from
  the last record it could read, and appended its next prompt to the cut line.
- A subagent's records written into the main transcript, as older Claude Code
  did, just before a tool result over the 30 MiB a record is read at.
- A last record still being written, which a reader must leave for later.
"""
import argparse
import json
import os
import re
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
parser.add_argument("project")
parser.add_argument("--home", default=os.environ.get("HOME"))
parser.add_argument("--hours-ago", type=float, default=2.5)
args = parser.parse_args()

project = os.path.realpath(args.project)
session = str(uuid.uuid5(uuid.NAMESPACE_URL, f"claude-damaged-history:{project}"))
ended = time.time() - args.hours_ago * 3600
clock = ended - 1800
records = []
parent = None


def stamp():
    global clock
    clock += 45
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(clock))


def record(kind, message, *, sidechain=False, parent_uuid=..., follows=True, **extra):
    """Append a record in Claude Code's field order and return its UUID."""
    global parent
    uid = str(uuid.uuid4())
    head = {
        "parentUuid": parent if parent_uuid is ... else parent_uuid,
        "isSidechain": sidechain,
        "userType": "external",
        "cwd": project,
        "sessionId": session,
        "version": "2.1.90",
        "gitBranch": "main",
    }
    if kind == "assistant":
        body = {"message": message, "type": kind}
    else:
        body = {"type": kind, "message": message}
    records.append(json.dumps({**head, **extra, **body, "uuid": uid, "timestamp": stamp()}, separators=(",", ":")))
    if follows:
        parent = uid
    return uid


def ask(text, parent_uuid=...):
    return record("user", {"role": "user", "content": [{"type": "text", "text": text}]}, parent_uuid=parent_uuid)


def answer(text, *, sidechain=False, parent_uuid=..., follows=True):
    message = {
        "model": "claude-sonnet-4-5",
        "id": "msg_" + uuid.uuid4().hex[:24],
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 3, "output_tokens": 120},
    }
    return record("assistant", message, sidechain=sidechain, parent_uuid=parent_uuid, follows=follows)


ask("Add keyboard navigation to the project switcher")
answer("Arrow keys now move through the switcher and Enter opens the project. I added a test for wrapping at the ends.")
ask("The settings page flickers on load, find out why")
answer("The theme is applied after the first paint. Reading it before mounting removes the flicker.")
# The prompt a crash cut short. Resumed, Claude Code goes on from the reply
# before it, and its next record shares the cut line.
cut = len(records)
before_cut = parent
ask("Also check the profile page, it might have the same problem")
ask("Run the settings tests", parent_uuid=before_cut)
call = "toolu_" + uuid.uuid4().hex[:20]
answer("Running them now.")
records[-1] = records[-1].replace(
    '"content":[{"type":"text","text":"Running them now."}]',
    f'"content":[{{"type":"tool_use","id":"{call}","name":"Bash","input":{{"command":"npm test -- settings"}}}}]',
)
# A subagent's records, written into the main transcript.
side = record("user", {"role": "user", "content": "Look for other pages that read the theme late"}, sidechain=True, parent_uuid=None, follows=False)
answer("Only the settings and profile pages do.", sidechain=True, parent_uuid=side, follows=False)
# A test log too large to read whole.
log = "".join(f"  settings › renders without flicker ({n}) ok\n" for n in range(800_000))
record("user", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call, "content": log}]})
answer("All 412 settings tests pass.")
ask("Where did we leave the flicker fix?")
answer("Both pages now read the theme before mounting, and the settings tests pass.")
records.append(json.dumps({"type": "custom-title", "customTitle": "Theme flicker fix (saved mid-crash)", "sessionId": session}))

lines = list(records)
lines[cut + 1] = lines[cut][: len(lines[cut]) // 3] + lines[cut + 1]
del lines[cut]
pending = json.dumps({"parentUuid": parent, "isSidechain": False, "type": "user", "message": {"role": "user", "content": "Still being written"}})

directory = os.path.join(args.home, ".claude", "projects", re.sub(r"[^A-Za-z0-9]", "-", project))
os.makedirs(directory, exist_ok=True)
path = os.path.join(directory, f"{session}.jsonl")
with open(path, "w") as transcript:
    transcript.write("\n".join(lines) + "\n" + pending[: len(pending) // 2])
os.utime(path, (ended, ended))
print(path)
