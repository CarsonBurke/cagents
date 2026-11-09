#!/usr/bin/python3
"""Seed invented Claude history in a fresh fixture's provider home.

Usage: claude-resume-history.py ROOT [--turns 50]
Writes ROOT/home/.claude, refusing an existing or real provider home. The
result names the thread, transcript and project for native evidence. No
provider executable, credentials or network service is used.
"""
import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import pwd
import uuid


def seed(root, turns=50):
    if not 1 <= turns <= 200:
        raise ValueError("fixture turns must be between 1 and 200")
    root = Path(root).resolve()
    home = root / "home" / ".claude"
    account_home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    if home.resolve() == (account_home / ".claude").resolve():
        raise ValueError("refusing the account's real Claude home")
    # A rerun cannot overwrite a transcript a running provider may have loaded.
    home.mkdir(parents=True)
    project = root / "project"
    project.mkdir(exist_ok=True)
    thread = "33333333-4444-4aaa-8bbb-cccccccccccc"
    encoded_project = "".join(
        character if character.isascii() and character.isalnum() else "-"
        for character in str(project)
    )
    directory = home / "projects" / encoded_project
    directory.mkdir(parents=True)
    path = directory / f"{thread}.jsonl"
    parent = None
    rows = []
    base = datetime.now(timezone.utc) - timedelta(seconds=20 * turns)
    paragraph = (
        "The saved conversation remains readable while its provider is stopped. "
        "Check the original result, keep the unsent draft, and inspect the exact "
        "refusal before choosing a recovery action."
    )
    for number in range(1, turns + 1):
        for role in ["user", "assistant"]:
            message = str(uuid.uuid4())
            text = (
                f"Retained history workload {number:02}: review check {number}."
                if role == "user"
                else f"Retained history result {number:02}.\n\n"
                + "\n\n".join([paragraph] * 6)
            )
            rows.append({
                "type": role,
                "uuid": message,
                "parentUuid": parent,
                "sessionId": thread,
                "cwd": str(project),
                "timestamp": (base + timedelta(seconds=20 * number))
                .isoformat().replace("+00:00", "Z"),
                "message": {
                    "id": message, "role": role, "model": "haiku",
                    "content": [{"type": "text", "text": text}],
                },
            })
            parent = message
    with path.open("x") as output:
        for row in rows:
            output.write(json.dumps(row, separators=(",", ":")) + "\n")
    os.chmod(path, 0o600)
    metadata = {
        "thread_id": thread, "history": str(path), "claude_home": str(home),
        "working_directory": str(project), "turns": turns,
        "messages": len(rows), "bytes": path.stat().st_size,
    }
    (root / "claude-history-seed.json").write_text(json.dumps(metadata) + "\n")
    return metadata


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--turns", type=int, default=50)
    args = parser.parse_args()
    try:
        print(json.dumps(seed(args.root, args.turns)))
    except (OSError, ValueError) as error:
        parser.exit(1, f"Cannot seed Claude resume history: {error}\n")
