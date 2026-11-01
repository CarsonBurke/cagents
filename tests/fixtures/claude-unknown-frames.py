#!/usr/bin/python3
"""Offline stream-json fixture for unknown-frame and stray-stdout recovery.

Exercises normal initialize/prompt/result traffic without network, credentials,
provider history, tools or subprocesses. Use an isolated development account.
The prompt `noise-limit` emits eight malformed lines instead of a result.
"""
import json
import sys
import uuid


def argument(name):
    for index, value in enumerate(sys.argv[1:], 1):
        if value.startswith(name + "="):
            return value.split("=", 1)[1]
        if value == name:
            return sys.argv[index + 1]
    return None


def send(value):
    print(json.dumps(value), flush=True)


if sys.argv[1:] == ["--version"]:
    print("2.1.286 (offline unknown-frame fixture)")
    raise SystemExit

session = argument("--session-id") or argument("--resume") or str(uuid.uuid4())
for line in sys.stdin:
    frame = json.loads(line)
    if frame.get("type") == "control_request":
        response = {"request_id": frame["request_id"]}
        if frame.get("request", {}).get("subtype") == "initialize":
            response.update(subtype="success", response={})
        else:
            response.update(subtype="error", error="Offline fixture supports initialization only")
        send({"type": "control_response", "response": response})
    elif frame.get("type") == "user":
        if "noise-limit" in json.dumps(frame.get("message", {})):
            for index in range(8):
                print(f"offline wrapper diagnostic {index}", flush=True)
            continue
        send({"type": "system", "subtype": "init", "session_id": session})
        send(dict(frame, session_id=session))
        send({"type": "future_frame", "opaque": "fixture-private-payload"})
        send({"type": "system", "subtype": "future_system", "opaque": "fixture-private-payload"})
        print("offline wrapper diagnostic", flush=True)
        message = str(uuid.uuid4())
        def event(value):
            send({"type": "stream_event", "event": value})
        event({"type": "message_start", "message": {"id": message}})
        event({"type": "future_stream_event"})
        event({"type": "content_block_start", "index": 1,
               "content_block": {"type": "future_content"}})
        event({"type": "content_block_delta", "index": 1,
               "delta": {"type": "text_delta", "text": "omitted future-block text"}})
        event({"type": "content_block_stop", "index": 1})
        event({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}})
        event({"type": "content_block_delta", "index": 0, "delta": {
            "type": "text_delta",
            "text": "Fixture answer arrived after unknown frames and wrapper output."}})
        event({"type": "content_block_stop", "index": 0})
        event({"type": "message_stop"})
        send({"type": "result", "subtype": "success", "is_error": False,
              "session_id": session, "usage": {"input_tokens": 1, "output_tokens": 1}})
