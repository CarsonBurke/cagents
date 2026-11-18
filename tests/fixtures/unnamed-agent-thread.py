#!/usr/bin/env python3
"""Forget the thread IDs of a native-evidence fixture's Codex agents.

Before #3095 a daemon-hosted Codex agent's confirmed thread ID reached neither
its agent record nor, unless a window showed it, its pane; this puts a fixture
back in that state. Run `forget` through `native-evidence env` between
sessions, with the fixture daemon stopped.
"""

import argparse
import json
import os
from pathlib import Path
import sqlite3
import stat


def registry_path():
    raw = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not raw:
        raise ValueError("run through native-evidence env inside its private fixture")
    root = Path(raw).resolve(strict=True)
    if root.stat().st_uid != os.getuid() or stat.S_IMODE(root.stat().st_mode) & 0o077:
        raise ValueError("the fixture root must be private and owned by this user")
    data = Path(os.environ.get("XDG_DATA_HOME", "")).resolve(strict=True)
    if data != root / "data":
        raise ValueError("XDG_DATA_HOME must be the fixture's data directory")
    paths = list(data.glob("cagents-development*/workspace-model.sqlite3"))
    if len(paths) != 1:
        raise ValueError("expected exactly one development registry in this fixture")
    return paths[0].resolve(strict=True)


def forget(db):
    forgotten = []
    for agent, record in db.execute("SELECT id, record FROM agents WHERE closed = 0").fetchall():
        record = json.loads(record)
        chat = (record.get("session") or {}).get("Chat")
        if not chat or chat["launch"]["provider"] != "Codex":
            continue
        chat["launch"]["session_id"] = None
        db.execute("UPDATE agents SET record = ? WHERE id = ?", (json.dumps(record), agent))
        for token, source in db.execute(
            "SELECT token, source FROM agent_authorities WHERE session_id = ?", (chat["id"],)
        ).fetchall():
            source = json.loads(source)
            source["Chat"]["launch"]["session_id"] = None
            db.execute(
                "UPDATE agent_authorities SET source = ? WHERE token = ?", (json.dumps(source), token)
            )
        for pane, descriptor in db.execute(
            "SELECT id, descriptor FROM panes WHERE kind = 'chat' AND resource = ?", (chat["id"],)
        ).fetchall():
            descriptor = json.loads(descriptor)
            descriptor["launch"]["session_id"] = None
            db.execute(
                "UPDATE panes SET descriptor = ?, provider_session = NULL WHERE id = ?",
                (json.dumps(descriptor), pane),
            )
        forgotten.append(chat["id"])
    if not forgotten:
        raise ValueError("the fixture has no open Codex agent")
    return forgotten


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["forget"])
    parser.parse_args()
    db = sqlite3.connect(f"file:{registry_path()}?mode=rw", uri=True, timeout=5)
    with db:
        print(json.dumps({"forgotten": forget(db)}))


if __name__ == "__main__":
    main()
