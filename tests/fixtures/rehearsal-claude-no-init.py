#!/usr/bin/python3
"""Offline Claude that accepts a turn but never reports a conversation ID.

Account probes finish normally; only real conversations omit initialization.
There is intentionally no transcript to resume. Stop the fixture by closing
stdin or terminating it; an interrupt acknowledges control but does not settle
its unconfirmed turn.
"""
import json
import os
from pathlib import Path
import sys


def emit(value):
    print(json.dumps(value), flush=True)


if "--version" in sys.argv:
    print("2.1.286 (Claude Code rehearsal no-init fixture)")
    raise SystemExit
if sys.argv[1:2] == ["auth"]:
    emit({"loggedIn": True, "email": "fixture@example.invalid", "subscriptionType": "pro"})
    raise SystemExit
probe = "--no-session-persistence" in sys.argv
if not probe:
    (Path(os.environ["CLAUDE_CONFIG_DIR"]) / "rehearsal-claude-no-init.started").write_text(str(os.getpid()) + "\n")
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
        emit({"type": "command_lifecycle", "command_uuid": frame.get("uuid"), "state": "started"})
        if probe:
            emit({"type": "result", "subtype": "success", "is_error": False})
        else:
            emit({"type": "assistant", "uuid": "rehearsal-no-init",
                  "message": {"id": "rehearsal-no-init", "role": "assistant",
                              "content": [{"type": "text", "text": "Working without reporting a session ID."}]}})
