#!/usr/bin/python3
"""Exercise stdout framing with an offline Codex or OMP pipe peer."""

import json
import os
from pathlib import Path
import sys

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parent
sys.path.insert(0, str(root / "scripts"))
from fixture_home import require_fixture_home

require_fixture_home("HOME")
provider = Path(__file__).name
if provider == "codex":
    require_fixture_home("CODEX_HOME")

SESSION = "c81f5f74-4385-4dbb-b4e6-36b0d177ac9b"
ANSWER = "The provider answered despite wrapper output."


def send(frame, prefix=""):
    print(prefix + json.dumps(frame), flush=True)


def complete(frame):
    send(frame, os.environ["FIXTURE_GLUE_PREFIX"])


def codex(request):
    if "id" not in request:
        return
    method = request["method"]
    result = {"data": []}
    if method == "initialize":
        result = {"codexHome": os.environ["CODEX_HOME"], "userAgent": "offline fixture"}
    elif method == "thread/start":
        result = {"thread": {"id": SESSION, "status": {"type": "idle"},
                             "canAcceptDirectInput": True},
                  "initialTurnsPage": {"data": [], "nextCursor": None}}
    elif method == "turn/start":
        result = {"turn": {"id": "fixture-turn", "status": "inProgress", "items": []}}
    send({"id": request["id"], "result": result})
    if method == "turn/start":
        params = {"threadId": SESSION, "turnId": "fixture-turn"}
        send({"method": "turn/started", "params": {**params, **result}})
        send({"method": "GET", "path": "/health",
              "note": {"method": "turn/completed", "params": {}}}, "wrapper status: ")
        send({"method": "item/agentMessage/delta",
              "params": {**params, "itemId": "answer", "delta": ANSWER}})
        complete({"method": "turn/completed",
                  "params": {**params, "turn": {**result["turn"], "status": "completed"}}})


def omp(request):
    command = request["type"]
    data = {}
    if command == "get_state":
        data = {"sessionId": SESSION}
    elif command == "get_messages_page":
        data = {"messages": [], "nextCursor": None}
    send({"type": "response", "command": command, "id": request["id"],
          "success": True, "data": data})
    if command == "prompt":
        send({"type": "agent_start"})
        send({"type": "warning", "note": {"type": "agent_end"}}, "wrapper status: ")
        answer = {"role": "assistant", "timestamp": 1, "stopReason": "stop",
                  "content": [{"type": "text", "text": ANSWER}]}
        send({"type": "message_end", "message": answer})
        complete({"type": "agent_end", "isTerminal": True, "messages": [answer]})


print("wrapper startup diagnostic", flush=True)
if provider == "omp":
    send({"type": "ready"})
for line in sys.stdin:
    {"codex": codex, "omp": omp}[provider](json.loads(line))
