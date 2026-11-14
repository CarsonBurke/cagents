#!/usr/bin/env python3
"""Offline contracts for the native saved-pane refusal fixture."""

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
import uuid

SCRIPT = Path(__file__).with_name("seed-refusal-registry.py")


class SeedRefusalRegistry(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="seed-refusal-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for leaf in ["home", "data/cagents-development", "bin", "project/.git"]:
            (self.root / leaf).mkdir(parents=True, exist_ok=True)
        self.db = self.root / "data/cagents-development/workspace-model.sqlite3"
        self.channel = str(uuid.uuid4())
        self.other = str(uuid.uuid4())
        with sqlite3.connect(self.db) as db:
            db.executescript("""
                PRAGMA foreign_keys=ON;
                CREATE TABLE channels(id TEXT PRIMARY KEY,title TEXT);
                CREATE TABLE view_scopes(id INTEGER PRIMARY KEY,channel TEXT);
                CREATE TABLE panes(id TEXT PRIMARY KEY,scope INTEGER REFERENCES view_scopes(id) ON DELETE CASCADE,kind TEXT);
            """)
            for i, channel in enumerate([self.channel, self.other], 1):
                db.execute("INSERT INTO channels VALUES(?,?)", (channel, "Fixture channel"))
                db.execute("INSERT INTO view_scopes VALUES(?,?)", (i, channel))
                db.execute("INSERT INTO panes VALUES(?,?,?)", (str(uuid.uuid4()), i, "chat"))
        self.env = os.environ | {
            "CAGENTS_EVIDENCE_FIXTURE": str(self.root),
            "HOME": str(self.root / "home"),
            "XDG_DATA_HOME": str(self.root / "data"),
        }

    def run_script(self, *args, env=None):
        return subprocess.run(["/usr/bin/python3", "-I", str(SCRIPT), *args], env=env or self.env, text=True, capture_output=True, check=False)

    def test_only_selected_channel_refuses_until_allow(self):
        armed = self.run_script("arm", self.channel)
        self.assertEqual(armed.returncode, 0, armed.stderr)
        self.assertTrue(json.loads(armed.stdout)["armed"])
        with sqlite3.connect(self.db) as db:
            self.assertEqual(db.execute("SELECT channel FROM view_scopes").fetchall(), [(self.other,)])
            self.assertEqual(db.execute("SELECT count(*) FROM panes").fetchone()[0], 1)
            with self.assertRaisesRegex(sqlite3.IntegrityError, "The workspace could not store these panes"):
                db.execute("INSERT INTO view_scopes(channel) VALUES(?)", (self.channel,))
            db.execute("INSERT INTO view_scopes(channel) VALUES(?)", (str(uuid.uuid4()),))
        allowed = self.run_script("allow")
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        with sqlite3.connect(self.db) as db:
            db.execute("INSERT INTO view_scopes(channel) VALUES(?)", (self.channel,))

    def test_refuses_nonfixture_home_without_mutation(self):
        result = self.run_script("arm", self.channel, env=self.env | {"HOME": "/home/not-the-fixture"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HOME must be the fixture", result.stderr)
        with sqlite3.connect(self.db) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM panes").fetchone()[0], 2)

    def test_missing_channel_does_not_clear_any_view(self):
        result = self.run_script("arm", str(uuid.uuid4()))
        self.assertNotEqual(result.returncode, 0)
        with sqlite3.connect(self.db) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM panes").fetchone()[0], 2)


if __name__ == "__main__":
    unittest.main()
