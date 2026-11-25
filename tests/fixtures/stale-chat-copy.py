#!/usr/bin/python3
"""Leave a native-evidence window with the stale draft copy of #4115.

Run it with the app window closed, the daemon still running, and a channel
whose first card is a chat:
    scripts/native-evidence env python3 tests/fixtures/stale-chat-copy.py TITLE [DRAFT]

Before #4115, when a pane's conversation came back from the daemon with only
a new proof, as after a reboot, the window kept an inert copy of the pane
beside it: the old descriptor and the unsent draft, never to be admitted.
This writes that copy into the window's store for the channel titled TITLE:
a new card after the channel's first chat, holding that chat's descriptor
under an earlier, made-up proof, kept local, waiting for admission, and
holding DRAFT (a single space by default) as its draft. The next window then
opens on the state the copy left.

It edits only the fixture's development store, which it finds under
XDG_DATA_HOME, and refuses anywhere else.
"""
import glob
import json
import os
import sqlite3
import sys
import time
import uuid


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    title = sys.argv[1]
    draft = sys.argv[2] if len(sys.argv) == 3 else " "
    if not os.environ.get("CAGENTS_EVIDENCE_FIXTURE"):
        sys.exit("Run this through scripts/native-evidence env, in a fixture.")
    stores = glob.glob(
        os.path.join(os.environ["XDG_DATA_HOME"], "cagents-development*", "sessions.sqlite3")
    )
    if len(stores) != 1:
        sys.exit(f"Expected one development store, found {stores}")
    db = sqlite3.connect(stores[0])

    def meta(key):
        row = db.execute("SELECT value FROM metadata WHERE key = ?", (key,)).fetchone()
        return json.loads(row[0]) if row else None

    def set_meta(key, value):
        db.execute(
            "INSERT INTO metadata (key, value) VALUES (?, ?) "
            "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, json.dumps(value)),
        )

    found = None
    for (workspace,) in db.execute("SELECT workspace_id FROM workspaces").fetchall():
        channels = meta(f"workspace:{workspace}:navigation:threads") or []
        for channel in channels:
            if channel["title"] == title:
                found = (workspace, channels, channel)
    if found is None:
        sys.exit(f"No channel titled {title!r}")
    workspace, channels, channel = found
    lead = None
    for card in channel["cards"]:
        row = db.execute(
            "SELECT title, width_fraction, content_json FROM cards "
            "WHERE card_id = ? AND kind_tag = 'chat' AND existence_json = '\"open\"'",
            (card,),
        ).fetchone()
        if row:
            lead = (card, row)
            break
    if lead is None:
        sys.exit(f"Channel {title!r} holds no open chat")
    card, (card_title, width, content) = lead
    source = json.loads(content)
    source["proof"] = {
        "host_session_id": str(uuid.uuid4()),
        "boot_id": str(uuid.uuid4()),
        "host_incarnation": str(uuid.uuid4()),
    }
    copy = str(uuid.uuid4())
    (ordinal,) = db.execute(
        "SELECT coalesce(max(ordinal) + 1, 0) FROM cards WHERE workspace_id = ?",
        (workspace,),
    ).fetchone()
    db.execute(
        "INSERT INTO cards (card_id, workspace_id, ordinal, generation, title, width_fraction, "
        "existence_json, kind_tag, content_json, updated_at_unix_ms, content_hash) "
        "VALUES (?, ?, ?, 0, ?, ?, '\"open\"', 'chat', ?, ?, NULL)",
        (copy, workspace, ordinal, card_title, width, json.dumps(source), int(time.time() * 1000)),
    )
    channel["cards"].insert(channel["cards"].index(card) + 1, copy)
    set_meta(f"workspace:{workspace}:navigation:threads", channels)
    prefix = f"workspace:{workspace}:navigation:"
    local = meta(prefix + "local_only_cards") or []
    set_meta(prefix + "local_only_cards", local + [copy])
    drafts = meta(prefix + "chat_drafts") or {}
    drafts[copy] = draft
    set_meta(prefix + "chat_drafts", drafts)
    pending = meta(prefix + "pending_chat_admissions") or {}
    pending[copy] = source
    set_meta(prefix + "pending_chat_admissions", pending)
    scopes = meta(prefix + "card_scopes") or {}
    if card in scopes:
        scopes[copy] = scopes[card]
        set_meta(prefix + "card_scopes", scopes)
    db.commit()
    print(json.dumps({"channel": channel["id"], "pane": card, "copy": copy}))


main()
