#!/usr/bin/env python3
"""Seed a numbered, saved Claude chat for native history paging checks.

Run through scripts/native-evidence env so HOME belongs to the fixture:
    python3 tests/fixtures/claude-long-history.py PROJECT [--turns 260] [--varied]
The generated conversation exceeds the retained 128-item paging window.
--varied gives exchanges the mixed heights of real work: tool calls, code
blocks, lists and attached screenshots.
"""
import argparse
import base64
import json
import struct
import zlib
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
parser.add_argument("--first-prompt",
                    help="what the reader first asked, in place of the first checkpoint")
parser.add_argument("--untitled", action="store_true",
                    help="leave out the conversation's name, so only its first prompt names it")
parser.add_argument("--varied", action="store_true",
                    help="mix tool calls, code blocks, lists and images into the exchanges")
args = parser.parse_args()
if args.turns < 129:
    parser.error("use at least 129 turns to exercise history eviction")
if args.scale < 1:
    parser.error("scale must be positive")
if args.tool_bytes < 0:
    parser.error("tool bytes must not be negative")



def png(width, height, turn):
    """A small gradient screenshot, distinct for each turn."""
    rows = b"".join(
        b"\x00" + bytes(
            channel
            for x in range(width)
            for channel in ((x * 255 // width + turn * 37) % 256,
                            (y * 255 // height) % 256, (turn * 91) % 256)
        )
        for y in range(height)
    )

    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b""))


def code(turn):
    lines = 6 + (turn * 7) % 30
    body = "\n".join(
        f"    let value_{line} = checkpoint({turn}, {line}).map(|row| row.height * {line + 1});"
        for line in range(lines)
    )
    return f"```rust\nfn checkpoint_{turn:03d}() -> Option<f32> {{\n{body}\n    None\n}}\n```"


def varied(turn, call):
    """Tool rows and assistant prose of varying height for one exchange."""
    blocks = []
    if turn % 2 == 0:
        read = f"{call}_read"
        blocks.append([
            ("assistant", [{"type": "tool_use", "id": read, "name": "Read",
                            "input": {"file_path": f"src/checkpoint_{turn % 23}.rs"}}]),
            ("user", [{"type": "tool_result", "tool_use_id": read, "content":
                       "\n".join(f"{line:4}\tfn row_{line}() {{}}"
                                 for line in range(1, 40 + turn % 60))}]),
        ])
    if turn % 3 == 0:
        edit = f"{call}_edit"
        blocks.append([
            ("assistant", [{"type": "tool_use", "id": edit, "name": "Edit",
                            "input": {"file_path": f"src/checkpoint_{turn % 23}.rs",
                                      "old_string": f"let height = {turn};",
                                      "new_string": f"let height = {turn} * 2;"}}]),
            ("user", [{"type": "tool_result", "tool_use_id": edit,
                       "content": "The file has been updated."}]),
        ])
    prose = []
    if turn % 3 == 1:
        prose.append(code(turn))
    if turn % 5 == 0:
        prose.append("\n".join(f"- Row {item}: measured {turn * item % 97} px"
                               for item in range(1, 4 + turn % 6)))
    if turn % 11 == 0:
        prose.append("| Page | Rows | Height |\n|---|---|---|\n" + "\n".join(
            f"| {page} | {turn % 40 + page} | {page * 311 % 900} |" for page in range(1, 6)))
    return blocks, prose


project = os.path.realpath(args.project)
session = str(uuid.uuid5(uuid.NAMESPACE_URL, f"claude-long-history:{project}"))
directory = Path.home() / ".claude" / "projects" / re.sub(r"[^A-Za-z0-9]", "-", project)
directory.mkdir(parents=True, exist_ok=True)
parent = None
clock = time.time() - (args.turns * (3 if args.varied else 1)
                       + (args.turns // 32 if args.tool_bytes else 0)) * 90
with (directory / f"{session}.jsonl").open("w") as transcript:
    for turn in range(1, args.turns + 1):
        # Newer pages are taller: paging backward must discard their extent,
        # rather than leaving a blank tail with the old content height.
        details = "\n\n".join(
            f"Check {check + 1}: the fixture keeps each exchange numbered so the reader "
            "can check that paging preserves their position and restores every newer turn."
            for check in range(args.scale * (1 + (turn - 1) // 64))
        )
        prompt = f"Review checkpoint {turn:03d}"
        if turn == 1 and args.first_prompt:
            prompt = args.first_prompt
        user = [{"type": "text", "text": prompt}]
        if args.varied and turn % 17 == 0:
            width, height = (480, 270) if turn % 34 else (240, 420)
            user.append({"type": "image", "source": {
                "type": "base64", "media_type": "image/png",
                "data": base64.b64encode(png(width, height, turn)).decode()}})
        messages = [("user", user)]
        prose = []
        if args.varied:
            tools, prose = varied(turn, f"toolu_varied_{turn}")
            for pair in tools:
                messages.extend(pair)
        if args.tool_bytes and turn % 32 == 0:
            call = f"toolu_checkpoint_{turn}"
            messages.extend([
                ("assistant", [{"type": "tool_use", "id": call, "name": "Bash",
                                "input": {"command": "cat checkpoint.log",
                                          "description": f"Verify checkpoint {turn:03d}"}}]),
                ("user", [{"type": "tool_result", "tool_use_id": call,
                           "content": "".join(
                               f"checkpoint {turn:03d} log line {line:06d}: verification passed\n"
                               for line in range(args.tool_bytes // 52 + 1))[:args.tool_bytes]}]),
            ])
        messages.append(("assistant", [{"type": "text", "text":
                         f"Checkpoint {turn:03d} is complete.\n\n{details}\n\n"
                         + "".join(f"{part}\n\n" for part in prose) +
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
    if not args.untitled:
        transcript.write(json.dumps({"type": "custom-title", "sessionId": session,
                                     "customTitle": "History paging checkpoints"}) + "\n")
print(session)
