#!/usr/bin/env python3
"""Refuse a saved channel's seed in a native-evidence fixture's real registry.

Run `arm CHANNEL` after closing only the native UI, leaving its daemon alive;
run `allow` before pressing Retry. Adding a trigger makes the published schema
unrecognized, so the real registry refuses admission. This is a global schema
fault, not a channel-specific daemon write fault; its conditional SQLite body
is only a fallback. No transport or UI update is replaced.
"""

import sys
from pathlib import Path

sys.dont_write_bytecode = True
for _fixture_parent in Path(__file__).resolve().parents:
    if (_fixture_parent / "scripts/fixture_home.py").is_file():
        sys.path.insert(0, str(_fixture_parent / "scripts"))
        break
from fixture_home import require_fixture_home

import argparse
import json
import os
import sqlite3
import stat
import uuid

TRIGGER = "fixture_seed_refusal"
SQLITE_TRIGGER_REASON = "The workspace could not store these panes"


def registry_path():
    require_fixture_home("HOME")
    raw = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not raw:
        raise ValueError("run through native-evidence env inside its private fixture")
    root = Path(raw).resolve(strict=True)
    if not (root / "bin").is_dir() or not (root / "project" / ".git").exists():
        raise ValueError("the fixture must contain its provider bin and project repository")
    if root.stat().st_uid != os.getuid() or stat.S_IMODE(root.stat().st_mode) & 0o077:
        raise ValueError("the fixture root must be private and owned by this user")
    if Path(os.environ.get("HOME", "")).resolve() != root / "home":
        raise ValueError("HOME must be the fixture's private home")
    data = require_fixture_home("XDG_DATA_HOME").resolve(strict=True)
    if data != root / "data":
        raise ValueError("XDG_DATA_HOME must be the fixture's data directory")
    paths = list(data.glob("cagents-development*/workspace-model.sqlite3"))
    if len(paths) != 1:
        raise ValueError("expected exactly one development registry in this fixture")
    path = paths[0].resolve(strict=True)
    if not path.is_relative_to(data):
        raise ValueError("the registry must remain inside fixture data")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["arm", "allow", "status"])
    parser.add_argument("channel", nargs="?")
    args = parser.parse_args()
    path = registry_path()
    db = sqlite3.connect(f"file:{path}?mode=rw", uri=True, timeout=5)
    db.execute("PRAGMA foreign_keys=ON")
    with db:
        if args.action == "arm":
            if not args.channel:
                parser.error("arm needs the fixture channel UUID")
            channel = str(uuid.UUID(args.channel))
            if db.execute("SELECT title FROM channels WHERE id=?", (channel,)).fetchone() is None:
                raise ValueError("the requested channel is not in this fixture registry")
            panes = db.execute("SELECT p.id, p.kind FROM panes p JOIN view_scopes s ON p.scope=s.id WHERE s.channel=?", (channel,)).fetchall()
            if not panes or not any(kind == "chat" for _, kind in panes):
                raise ValueError("the fixture channel must already contain a saved chat pane")
            db.execute(f"DROP TRIGGER IF EXISTS {TRIGGER}")
            # Deleting the selected view cascades to its panes and their
            # projections, preserving the clocks and all actor, history and
            # window-retention records. A schema 26 or older registry also
            # keeps channel-work placement epochs without a foreign key; reset
            # the selected panes' rows so they can register again.
            if db.execute("SELECT 1 FROM sqlite_schema WHERE name='channel_work_placement_epochs'").fetchone():
                db.execute(
                    "DELETE FROM channel_work_placement_epochs WHERE pane IN ("
                    "SELECT p.id FROM panes p JOIN view_scopes s ON p.scope=s.id "
                    "WHERE s.channel=?)",
                    (channel,),
                )
            db.execute("DELETE FROM view_scopes WHERE channel=?", (channel,))
            # The UUID is parsed above; the reason is a fixed fixture sentence.
            db.execute(f"CREATE TRIGGER {TRIGGER} BEFORE INSERT ON view_scopes WHEN NEW.channel='{channel}' BEGIN SELECT RAISE(ABORT, '{SQLITE_TRIGGER_REASON}'); END")
            print(json.dumps({"armed": True, "channel": channel, "panes": len(panes), "fault": "unrecognized registry schema trigger"}))
        elif args.action == "allow":
            db.execute(f"DROP TRIGGER IF EXISTS {TRIGGER}")
            print(json.dumps({"armed": False}))
        else:
            armed = db.execute("SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?", (TRIGGER,)).fetchone() is not None
            print(json.dumps({"armed": armed}))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, sqlite3.Error) as error:
        raise SystemExit(f"seed-refusal-registry: {error}") from error
