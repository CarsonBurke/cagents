#!/usr/bin/env python3
"""Read-only baseline/retired schema proofs for the native pane-close fixture."""
from contextlib import closing
import json
from pathlib import Path
import runpy
import sqlite3
import tempfile
import unittest


HELPER = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/measure-chat-memory-closepanes"))


class CloseProof(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.ui = Path(self.directory.name) / "sessions.sqlite3"
        self.model = Path(self.directory.name) / "model.sqlite3"
        with closing(sqlite3.connect(self.ui)) as db, db:
            db.executescript('''CREATE TABLE cards(card_id,workspace_id,existence_json);
                CREATE TABLE workspaces(workspace_id,focused_card_id);
                INSERT INTO workspaces VALUES ('workspace','minted');
                INSERT INTO cards VALUES ('minted','workspace','"open"');''')
        with closing(sqlite3.connect(self.model)) as db, db:
            db.executescript('''CREATE TABLE shared_view_panes(id,scope);
                CREATE TABLE shared_views(scope,panes);
                INSERT INTO shared_view_panes VALUES ('minted','fixture');
                INSERT INTO shared_views VALUES ('fixture','[{"id":"minted"}]');''')

    def snapshot(self):
        return HELPER["snapshot"](self.ui, self.model)

    def unshare(self):
        with closing(sqlite3.connect(self.model)) as db, db:
            db.execute("DELETE FROM shared_view_panes")
            db.execute("UPDATE shared_views SET panes='[]'")

    def test_baseline_tombstone_requires_both_membership_and_owner_removal(self):
        with closing(sqlite3.connect(self.ui)) as db, db:
            db.execute('UPDATE cards SET existence_json=?', ('{"closed":{"tombstone":{}}}',))
        self.assertFalse(HELPER["closed"]("minted", self.snapshot()))
        with closing(sqlite3.connect(self.model)) as db, db:
            db.execute("DELETE FROM shared_view_panes")
        self.assertFalse(HELPER["closed"]("minted", self.snapshot()))
        self.unshare()
        self.assertTrue(HELPER["closed"]("minted", self.snapshot()))

    def test_retired_missing_card_requires_guard(self):
        self.unshare()
        with closing(sqlite3.connect(self.ui)) as db, db:
            db.execute("DELETE FROM cards")
        self.assertFalse(HELPER["closed"]("minted", self.snapshot()))
        with closing(sqlite3.connect(self.ui)) as db, db:
            db.executescript("CREATE TABLE closed_cards(card_id); INSERT INTO closed_cards VALUES('minted');")
        self.assertTrue(HELPER["closed"]("minted", self.snapshot()))

    def test_open_or_unknown_card_never_counts_as_closed(self):
        self.unshare()
        self.assertFalse(HELPER["closed"]("minted", self.snapshot()))
        self.assertFalse(HELPER["closed"]("unknown", self.snapshot()))
        self.assertEqual(self.snapshot()[1], {"workspace": "minted"})

    def test_resume_requires_the_same_exact_adoption_proof(self):
        path = Path(self.directory.name) / "attempt1.jsonl"
        proof = {"adopted_cards": ["minted"], "pid": 42, "start_ticks": "99", "manifest_sha256": "exact"}
        path.write_text(json.dumps({"action": ["observe"]}) + "\n" + json.dumps(proof) + "\n")
        self.assertEqual(HELPER["verified_adoption"](path, proof), proof)
        for key, replacement in (("pid", 43), ("start_ticks", "100"), ("manifest_sha256", "other"), ("adopted_cards", ["other"])):
            with self.subTest(changed=key), self.assertRaisesRegex(SystemExit, "prior adoption"):
                HELPER["verified_adoption"](path, {**proof, key: replacement})

    def test_control_in_another_channel_is_rejected_before_closing_actors(self):
        owners = {"control": "fixture", "minted": "fixture"}
        members = {"control": {"fixture"}, "minted": {"fixture"}}
        self.assertEqual(HELPER["shared_scope"]("control", {"minted"}, owners, members), "fixture")
        for bad_owners, bad_members in (({**owners, "control": "other"}, members),
                                       (owners, {**members, "minted": {"fixture", "other"}})):
            with self.assertRaisesRegex(SystemExit, "one exact shared-view"):
                HELPER["shared_scope"]("control", {"minted"}, bad_owners, bad_members)


if __name__ == "__main__":
    unittest.main()
