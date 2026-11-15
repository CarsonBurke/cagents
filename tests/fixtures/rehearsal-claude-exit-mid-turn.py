#!/usr/bin/python3
"""Offline Claude that initializes, starts work, and exits without a result.

Account probes complete normally. Real prompts end with exit status 1 while
work is unconfirmed, exercising upgrade preparation rather than a clean stop.
"""
import json
import os
from pathlib import Path
import sys
import uuid


def emit(value):
    print(json.dumps(value), flush=True)


if "--version" in sys.argv:
    print("2.1.286 (Claude Code rehearsal interrupted fixture)")
    raise SystemExit
if sys.argv[1:2] == ["auth"]:
    emit({"loggedIn": True, "email": "fixture@example.invalid", "subscriptionType": "pro"})
    raise SystemExit
probe = "--no-session-persistence" in sys.argv
if not probe:
    (Path(os.environ["CLAUDE_CONFIG_DIR"]) / "rehearsal-claude-exit-mid-turn.started").write_text(str(os.getpid()) + "\n")
session = str(uuid.uuid4())
for flag in ("--session-id", "--resume"):
    if flag in sys.argv:
        session = sys.argv[sys.argv.index(flag) + 1]
for line in sys.stdin:
    frame = json.loads(line)
    if frame.get("type") == "control_request":
        metadata = {}
        if frame.get("request", {}).get("subtype") == "initialize":
            metadata = {"claude_code_version": "2.1.286", "current_permission_mode": "default",
                        "models": [{"value": "haiku", "displayName": "Offline fixture"}]}
        emit({"type": "control_response", "response": {"subtype": "success",
              "request_id": frame["request_id"], "response": metadata}})
    elif frame.get("type") == "user":
        emit({"type": "system", "subtype": "init", "session_id": session,
              "claude_code_version": "2.1.286", "model": "haiku"})
        emit({"type": "command_lifecycle", "command_uuid": frame.get("uuid"), "state": "started"})
        if probe:
            emit({"type": "result", "subtype": "success", "session_id": session, "is_error": False})
        else:
            emit({"type": "assistant", "uuid": "rehearsal-interrupted",
                  "message": {"id": "rehearsal-interrupted", "role": "assistant",
                              "content": [{"type": "text", "text": "Starting work before an unexpected provider exit."}]}})
            print("rehearsal fixture: provider exited mid-turn without confirming completion", file=sys.stderr)
            raise SystemExit(1)
