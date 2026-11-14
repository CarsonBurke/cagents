#!/usr/bin/env python3
"""Fixture data-safety contracts, not published-registry admission coverage."""

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
import uuid

SCRIPT = Path(__file__).with_name("seed-refusal-registry.py")
TABLES = (
    "channels", "view_scopes", "panes", "channel_work_placement_epochs",
    "channel_work_placement_clock", "view_revision_clock", "retired_panes",
    "view_operations", "shared_view_terminal_sources", "shared_terminal_retention_pins",
    "terminal_retention_pins", "fixture_actor_history",
)


class SeedRefusalRegistry(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="seed-refusal-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for leaf in ["home", "data/cagents-development", "bin", "project/.git"]:
            (self.root / leaf).mkdir(parents=True, exist_ok=True)
        self.db = self.root / "data/cagents-development/workspace-model.sqlite3"
        self.channel, self.other = [str(uuid.uuid4()) for _ in range(2)]
        self.selected = [str(uuid.uuid4()) for _ in range(2)]
        self.unrelated = str(uuid.uuid4())
        with sqlite3.connect(self.db) as db:
            # These active epoch/clock and FK projection declarations match
            # host/model/channel_work/placement.rs and views/store.rs. The
            # parent view columns are limited to those the fixture consumes.
            db.executescript("""
                PRAGMA foreign_keys=ON;
                CREATE TABLE channels(id TEXT PRIMARY KEY,title TEXT);
                CREATE TABLE view_scopes(id INTEGER PRIMARY KEY,channel TEXT);
                CREATE TABLE panes(id TEXT PRIMARY KEY,scope INTEGER REFERENCES view_scopes(id) ON DELETE CASCADE,kind TEXT);
                CREATE TABLE channel_work_placement_epochs(
                    pane TEXT PRIMARY KEY,epoch INTEGER NOT NULL CHECK(epoch>0)
                ) STRICT;
                CREATE TABLE channel_work_placement_clock(
                    id INTEGER PRIMARY KEY CHECK(id=1),last_epoch INTEGER NOT NULL CHECK(last_epoch>=0)
                ) STRICT;
                INSERT INTO channel_work_placement_clock VALUES(1,3);
                CREATE TABLE view_revision_clock(
                    id INTEGER PRIMARY KEY CHECK(id=1),last INTEGER NOT NULL CHECK(last>=0)
                ) STRICT;
                INSERT INTO view_revision_clock VALUES(1,17);
                CREATE TABLE retired_panes(
                    scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
                    pane TEXT NOT NULL,ordinal INTEGER NOT NULL,retired_at_unix_ms INTEGER NOT NULL,
                    PRIMARY KEY(scope,pane)
                ) STRICT, WITHOUT ROWID;
                CREATE TABLE view_operations(
                    operation TEXT PRIMARY KEY,scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
                    applied_at_unix_ms INTEGER NOT NULL
                ) STRICT, WITHOUT ROWID;
                CREATE TABLE shared_view_terminal_sources(
                    pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
                    source TEXT NOT NULL,ended TEXT NOT NULL
                ) STRICT;
                CREATE TABLE shared_terminal_retention_pins(
                    pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,terminal TEXT NOT NULL
                ) STRICT;
                CREATE TABLE terminal_retention_pins(terminal TEXT PRIMARY KEY,generation TEXT NOT NULL) STRICT;
                CREATE TABLE fixture_actor_history(actor TEXT PRIMARY KEY,history TEXT NOT NULL) STRICT;
            """)
            for scope, channel in [(1, self.channel), (2, self.other)]:
                db.execute("INSERT INTO channels VALUES(?,?)", (channel, "Fixture channel"))
                db.execute("INSERT INTO view_scopes VALUES(?,?)", (scope, channel))
                db.execute("INSERT INTO retired_panes VALUES(?,?,0,100)", (scope, str(uuid.uuid4())))
                db.execute("INSERT INTO view_operations VALUES(?,?,100)", (str(uuid.uuid4()), scope))
            for epoch, (pane, scope, kind) in enumerate([
                (self.selected[0], 1, "chat"), (self.selected[1], 1, "terminal"),
                (self.unrelated, 2, "terminal"),
            ], 1):
                db.execute("INSERT INTO panes VALUES(?,?,?)", (pane, scope, kind))
                db.execute("INSERT INTO channel_work_placement_epochs VALUES(?,?)", (pane, epoch))
                if kind == "terminal":
                    db.execute("INSERT INTO shared_view_terminal_sources VALUES(?,?,?)", (pane, "source", "ended"))
                    db.execute("INSERT INTO shared_terminal_retention_pins VALUES(?,?)", (pane, "terminal-" + pane))
                    db.execute("INSERT INTO terminal_retention_pins VALUES(?,?)", ("terminal-" + pane, "window-pin"))
                db.execute("INSERT INTO fixture_actor_history VALUES(?,?)", (pane, "keep actor and history"))
        self.env = os.environ | {
            "CAGENTS_EVIDENCE_FIXTURE": str(self.root),
            "HOME": str(self.root / "home"),
            "XDG_DATA_HOME": str(self.root / "data"),
        }

    def run_script(self, *args, env=None):
        return subprocess.run(
            ["/usr/bin/python3", "-I", str(SCRIPT), *args], env=env or self.env,
            text=True, capture_output=True, check=False,
        )

    def snapshot(self):
        with sqlite3.connect(self.db) as db:
            return {table: db.execute(f"SELECT * FROM {table} ORDER BY 1,2").fetchall() for table in TABLES}

    def test_arm_removes_selected_active_projections_and_preserves_history_and_clocks(self):
        before = self.snapshot()
        armed = self.run_script("arm", self.channel)
        self.assertEqual(armed.returncode, 0, armed.stderr)
        self.assertTrue(json.loads(armed.stdout)["armed"])
        after = self.snapshot()
        self.assertEqual(after["view_scopes"], [(2, self.other)])
        self.assertEqual(after["panes"], [(self.unrelated, 2, "terminal")])
        self.assertEqual(after["channel_work_placement_epochs"], [(self.unrelated, 3)])
        for table in ["retired_panes", "view_operations"]:
            scope_column = 0 if table == "retired_panes" else 1
            self.assertEqual(after[table], [row for row in before[table] if row[scope_column] == 2])
        for table in ["shared_view_terminal_sources", "shared_terminal_retention_pins"]:
            self.assertEqual(after[table], [row for row in before[table] if row[0] == self.unrelated])
        for table in ["channel_work_placement_clock", "view_revision_clock", "terminal_retention_pins", "fixture_actor_history"]:
            self.assertEqual(after[table], before[table], table)
        allowed = self.run_script("allow")
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        # Same-pane registration allocates fresh generations, without the stale
        # non-FK row that caused the actual native retry's UNIQUE refusal.
        with sqlite3.connect(self.db) as db:
            self.assertEqual(db.execute("PRAGMA foreign_key_list(channel_work_placement_epochs)").fetchall(), [])
            db.execute("INSERT INTO view_scopes VALUES(1,?)", (self.channel,))
            for epoch, pane in enumerate(self.selected, 4):
                db.execute("INSERT INTO panes VALUES(?,1,'chat')", (pane,))
                db.execute("INSERT INTO channel_work_placement_epochs VALUES(?,?)", (pane, epoch))
                db.execute("UPDATE channel_work_placement_clock SET last_epoch=?", (epoch,))
            self.assertEqual(db.execute("SELECT epoch FROM channel_work_placement_epochs WHERE pane=?", (self.unrelated,)).fetchone(), (3,))
            self.assertEqual(db.execute("SELECT last_epoch FROM channel_work_placement_clock").fetchone(), (5,))

    def test_conditional_trigger_body_only_in_plain_sqlite(self):
        # This checks the trigger body only. The real daemon rejects its
        # unpublished schema globally before this conditional body can run.
        result = self.run_script("arm", self.channel)
        self.assertEqual(result.returncode, 0, result.stderr)
        with sqlite3.connect(self.db) as db:
            with self.assertRaisesRegex(sqlite3.IntegrityError, "The workspace could not store these panes"):
                db.execute("INSERT INTO view_scopes(channel) VALUES(?)", (self.channel,))
            db.execute("INSERT INTO view_scopes(channel) VALUES(?)", (str(uuid.uuid4()),))

    def test_refuses_nonfixture_home_without_mutation(self):
        before = self.snapshot()
        result = self.run_script("arm", self.channel, env=self.env | {"HOME": "/home/not-the-fixture"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HOME must be the fixture", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_missing_channel_does_not_clear_any_view_or_projection(self):
        before = self.snapshot()
        result = self.run_script("arm", str(uuid.uuid4()))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
