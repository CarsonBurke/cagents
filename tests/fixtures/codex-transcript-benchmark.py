"""Write a private saved-rollout fixture for the transcript page benchmark."""

import json
import sys
from pathlib import Path

def row(kind, typename, **fields):
    return {"type": kind, "timestamp": "2026-11-20T01:23:45Z",
            "payload": {"type": typename, **fields}}

def turn(i):
    output = ("unit result: parsed saved history; code 0; elapsed 0.002s\n" * 150)
    cid = f"call-{i}"
    yield row("event_msg", "task_started")
    yield row("response_item", "message", role="user",
              content=[{"type": "input_text", "text": f"Check history case {i}."}])
    yield row("response_item", "function_call", call_id=cid, name="exec_command",
              arguments='{"cmd":"cargo test --lib history"}')
    yield row("response_item", "function_call_output", call_id=cid, output=output)
    if i % 7:
        yield row("event_msg", "item_completed",
                  item={"type": "CommandExecution", "id": cid, "command": "cargo test --lib history",
                        "status": "completed", "exit_code": 0, "aggregated_output": output})
    yield row("response_item", "message", role="assistant", id=f"reply-{i}",
              content=[{"type": "output_text", "text": "The history case passed."}])
    yield row("event_msg", "task_complete")

root = Path(sys.argv[1])
with (root / "rollout.jsonl").open("x") as file:
    for i in range(900):
        for record in turn(i):
            file.write(json.dumps(record) + "\n")
with (root / "append.jsonl").open("x") as file:
    file.writelines(json.dumps(record) + "\n" for record in turn(900))
