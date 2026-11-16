#!/usr/bin/env python3
"""Seed a numbered, saved Claude chat for native history paging checks.

Run through scripts/native-evidence env so HOME belongs to the fixture:
    python3 tests/fixtures/claude-long-history.py PROJECT [--turns 260]
The generated conversation exceeds the retained 128-item paging window.
"""
import argparse
import json
import os
from pathlib import Path
import re
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("project")
parser.add_argument("--turns", type=int, default=260)
parser.add_argument("--scale", type=int, default=1,
                    help="paragraph multiplier; 64 also exceeds Claude's provider history page")
parser.add_argument("--tool-bytes", type=int, default=0,
                    help="insert a collapsed Bash result of this size every 32 exchanges")
args = parser.parse_args()
if args.turns < 129:
    parser.error("use at least 129 turns to exercise history eviction")
if args.scale < 1:
    parser.error("scale must be positive")
if args.tool_bytes < 0:
    parser.error("tool bytes must not be negative")

project = os.path.realpath(args.project)
session = str(uuid.uuid5(uuid.NAMESPACE_URL, f"claude-long-history:{project}"))
directory = Path.home() / ".claude" / "projects" / re.sub(r"[^A-Za-z0-9]", "-", project)
directory.mkdir(parents=True, exist_ok=True)
parent = None
clock = time.time() - (args.turns + (args.turns // 32 if args.tool_bytes else 0)) * 90
with (directory / f"{session}.jsonl").open("w") as transcript:
    for turn in range(1, args.turns + 1):
        # Newer pages are taller: paging backward must discard their extent,
        # rather than leaving a blank tail with the old content height.
        details = "\n\n".join(
            f"Check {check + 1}: the fixture keeps each exchange numbered so the reader "
            "can check that paging preserves their position and restores every newer turn."
            for check in range(args.scale * (1 + (turn - 1) // 64))
        )
        messages = [("user", [{"type": "text", "text": f"Review checkpoint {turn:03d}"}])]
        if args.tool_bytes and turn % 32 == 0:
            call = f"toolu_checkpoint_{turn}"
            messages.extend([
                ("assistant", [{"type": "tool_use", "id": call, "name": "Bash",
                                "input": {"command": "cat checkpoint.log",
                                          "description": f"Verify checkpoint {turn:03d}"}}]),
                ("user", [{"type": "tool_result", "tool_use_id": call,
                           "content": ("Checkpoint verification passed.\n" *
                                       (args.tool_bytes // 32 + 1))[:args.tool_bytes]}]),
            ])
        messages.append(("assistant", [{"type": "text", "text":
                         f"Checkpoint {turn:03d} is complete.\n\n{details}\n\n"
                         f"Verification marker: checkpoint-{turn:03d}."}]))
        for index, (role, content) in enumerate(messages):
            uid = str(uuid.uuid5(uuid.NAMESPACE_URL, f"{session}:{turn}:{index}:{role}"))
            message = {"role": role, "content": content}
            if role == "assistant":
                message.update(model="claude-haiku-4-5", id=f"msg_{uid}", type="message",
                               usage={"input_tokens": 3, "output_tokens": 60})
            transcript.write(json.dumps({
                "parentUuid": parent, "isSidechain": False, "userType": "external",
                "cwd": project, "sessionId": session, "version": "2.1.90",
                "gitBranch": "main", "type": role, "message": message, "uuid": uid,
                "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(clock)),
            }) + "\n")
            clock += 45
            parent = uid
    transcript.write(json.dumps({"type": "custom-title", "sessionId": session,
                                 "customTitle": "History paging checkpoints"}) + "\n")
print(session)
