#!/usr/bin/python3
"""Seed one invented Codex conversation in a fresh fixture's provider home.

Usage: codex-resume-history.py ROOT [--turns 3]
Writes only ROOT/home/.codex, refusing an existing provider home. The result
on stdout names the thread, rollout and working directory for native evidence.
No provider executable, credentials or network service is used.
"""
import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import pwd
import uuid


def seed(root, turns=3):
    if not 1 <= turns <= 100:
        raise ValueError("fixture turns must be between 1 and 100")
    root = Path(root).resolve()
    home = root / "home" / ".codex"
    account_home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    if home.resolve() == (account_home / ".codex").resolve():
        raise ValueError("refusing the account's real Codex home")
    # A new provider home cannot contain a real thread or credentials, and a
    # rerun cannot rewrite a rollout a running provider might have loaded.
    home.mkdir(parents=True)
    project = root / "project"
    project.mkdir(exist_ok=True)
    thread = str(uuid.uuid4())
    started = datetime.now(timezone.utc).replace(microsecond=0) - timedelta(minutes=2 * turns)
    directory = home / "sessions" / started.strftime("%Y/%m/%d")
    directory.mkdir(parents=True)
    path = directory / f"rollout-{started:%Y-%m-%dT%H-%M-%S}-{thread}.jsonl"
    rows = []

    def add(time, kind, payload):
        rows.append({"timestamp": time.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
                     "ordinal": len(rows),
                     "type": kind, "payload": payload})

    add(started, "session_meta", {
        "id": thread, "timestamp": started.isoformat().replace("+00:00", "Z"),
        "cwd": str(project), "originator": "codex_cli_rs", "cli_version": "0.166.0",
        "source": "cli", "model_provider": "openai",
    })
    for number in range(1, turns + 1):
        time = started + timedelta(minutes=2 * (number - 1))
        turn = str(uuid.uuid4())
        prompt = f"Check the resume fixture, turn {number}."
        reply = f"Resume fixture turn {number} is preserved."
        seconds = int(time.timestamp())
        # response_item records alone are model context. Explicit task
        # boundaries preserve turn identity in resumed conversation history.
        add(time, "event_msg", {
            "type": "task_started", "turn_id": turn, "root_turn_id": turn,
            "turn_attribution": {"turn_id": turn, "turn_trigger": None, "parent_turn_id": None,
                                 "initiating_agent_path": None, "root_turn_id": turn},
            "started_at": seconds, "model_context_window": 258400,
            "collaboration_mode_kind": "default",
        })
        add(time, "turn_context", {"turn_id": turn, "cwd": str(project),
                                   "model": "gpt-5.2-codex", "effort": "medium"})
        for role, text, item_type, content_type in [
            ("user", prompt, "UserMessage", "input_text"),
            ("assistant", reply, "AgentMessage", "output_text"),
        ]:
            message = str(uuid.uuid4())
            payload = {"type": "message", "id": message, "role": role,
                       "content": [{"type": content_type, "text": text}]}
            if role == "assistant":
                payload["phase"] = "final_answer"
            add(time, "response_item", payload)
            if role == "user":
                add(time, "event_msg", {"type": "user_message", "message": text,
                                        "images": [], "local_images": [], "text_elements": []})
            else:
                add(time, "event_msg", {"type": "agent_message", "message": text,
                                        "phase": "final_answer"})
            item = {"type": item_type, "id": message,
                    "content": [{"type": "text" if role == "user" else "Text", "text": text}]}
            if role == "user":
                item["content"][0]["text_elements"] = []
            else:
                item["phase"] = "final_answer"
            add(time, "event_msg", {"type": "item_completed", "thread_id": thread,
                                    "turn_id": turn, "item": item,
                                    "started_at_ms": seconds * 1000,
                                    "completed_at_ms": seconds * 1000})
        add(time, "event_msg", {"type": "task_complete", "turn_id": turn,
                                "root_turn_id": turn, "last_agent_message": reply,
                                "started_at": seconds, "completed_at": seconds,
                                "duration_ms": 0})
    path.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows))
    (home / "session_index.jsonl").write_text(json.dumps({
        "id": thread, "thread_name": "Codex resume history",
        "updated_at": rows[-1]["timestamp"],
    }) + "\n")
    return {"thread_id": thread, "rollout": str(path), "codex_home": str(home),
            "working_directory": str(project), "turns": turns}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--turns", type=int, default=3)
    args = parser.parse_args()
    try:
        print(json.dumps(seed(args.root, args.turns)))
    except (OSError, ValueError) as error:
        parser.exit(1, f"Cannot seed Codex resume history: {error}\n")
