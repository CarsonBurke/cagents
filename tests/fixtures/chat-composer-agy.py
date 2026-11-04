#!/usr/bin/env python3
"""Offline Antigravity CLI fixture speaking the stream-json of agy 1.4.12.

Makes no network requests. A prompt's first word chooses the turn: `deny` ends
with a refused tool, `slow` streams until SIGINT, `fail` reports an error. Any
other prompt streams a command and a reply. Writes the transcript and index a
resumed conversation and the resume list read. Set `AGY_FIXTURE_SIGNED_OUT=1` to
refuse `agy models` as a signed-out CLI does.

This checks this application's handling, not Google's service.
"""
import json
import os
import signal
import sqlite3
import sys
import time
import uuid
from pathlib import Path

ROOT = Path(os.environ["HOME"]) / ".gemini/antigravity-cli"
state = {"id": None, "step": 0, "turns": 0}


def emit(frame):
    print(json.dumps(frame), flush=True)


def record(kind, **fields):
    logs = ROOT / "brain" / state["id"] / ".system_generated/logs"
    logs.mkdir(parents=True, exist_ok=True)
    with (logs / "transcript.jsonl").open("a") as transcript:
        row = {"step_index": state["step"], "type": kind, "status": "DONE", **fields}
        transcript.write(json.dumps(row) + "\n")


def step(step_type, done, **fields):
    update = {"conversation_id": state["id"], "step_index": state["step"], "step_type": step_type}
    update["state"] = "DONE" if done else "ACTIVE"
    emit({"event": "step_update", "step_update": {**update, **fields}})


def result(status="SUCCESS", response="", **fields):
    usage = {"input_tokens": 10000, "output_tokens": 20, "total_tokens": 10020}
    body = {"conversation_id": state["id"], "status": status, "response": response}
    emit({"event": "result", "result": {**body, "num_turns": state["turns"], "usage": usage, **fields}})


def on_interrupt(_signal, _frame):
    result("ERROR", error="stream input cancelled: context canceled", finished_reason="interrupted")
    os._exit(1)


def reply(text):
    state["step"] += 1
    for word in text.split(" "):
        step("agent_response", False, text_delta=word + " ")
    step("agent_response", True, text_delta="\n")
    record("PLANNER_RESPONSE", content=text)
    return text


def turn(text):
    state["turns"] += 1
    state["step"] += 1
    step("user_input", True)
    record("USER_INPUT", content=f"<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>")
    word = (text.split() or [""])[0].lower()
    if word == "fail":
        result("ERROR", error="Fixture model is overloaded", finished_reason="error")
    elif word == "deny":
        result("SUCCESS", reply("I could not run that."), denied_actions=["run_command"])
    elif word == "slow":
        for count in range(600):
            state["step"] += 1
            step("agent_response", False, text_delta=f"Working step {count}. ")
            time.sleep(0.5)
    else:
        state["step"] += 1
        call = {"name": "run_command", "parameters": {"CommandLine": "ls"}}
        step("tool", False, tool_name="run_command", tool_info=call)
        step("tool", True, tool_name="run_command", tool_info={**call, "output": "README.md\n"})
        result("SUCCESS", reply("The fixture listed the directory."))


def main():
    arguments = sys.argv[1:]
    if "--version" in arguments:
        print("1.4.12")
    elif arguments[:1] == ["models"]:
        print("Fetching available models...")
        if os.environ.get("AGY_FIXTURE_SIGNED_OUT"):
            print("Error: Please sign in to view available models.", file=sys.stderr)
            sys.exit(1)
        print("gemini-fixture-low\tFixture (Low)\ngemini-fixture-high\tFixture (High)")
    elif "--input-format" not in arguments:
        print("Antigravity CLI fixture. Press Enter to quit.")
        sys.stdin.readline()
    else:
        resume = arguments[arguments.index("--conversation") + 1] if "--conversation" in arguments else None
        state["id"] = resume or str(uuid.uuid4())
        ROOT.mkdir(parents=True, exist_ok=True)
        index = sqlite3.connect(ROOT / "conversation_summaries.db")
        index.execute("CREATE TABLE IF NOT EXISTS conversation_summaries (conversation_id TEXT PRIMARY KEY, workspace_uris TEXT)")
        index.execute("INSERT OR REPLACE INTO conversation_summaries VALUES (?, ?)", (state["id"], json.dumps(["file://" + os.getcwd()])))
        index.commit()
        index.close()
        transcript = ROOT / "brain" / state["id"] / ".system_generated/logs/transcript.jsonl"
        if transcript.exists():
            state["step"] = len(transcript.read_text().splitlines()) + 1
        signal.signal(signal.SIGINT, on_interrupt)
        emit({"event": "init", "conversation_id": state["id"], "init": {"cwd": os.getcwd(), "tools": [], "permission_mode": "request-review"}})
        for line in sys.stdin:
            turn(json.loads(line)["message"]["content"])


if __name__ == "__main__":
    main()
