#!/usr/bin/env python3
"""File-backed verifier contracts; these do not establish native UI behavior."""

from contextlib import contextmanager
import copy
import hashlib
import json
import os
from pathlib import Path
import runpy
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import uuid


PROBE_PATH = Path(__file__).resolve().parent / "fixtures/focused-transfer-probe.py"
PROBE = runpy.run_path(str(PROBE_PATH))


def uid():
    return str(uuid.uuid4())


@contextmanager
def writable(path):
    db = sqlite3.connect(path)
    try:
        with db:
            yield db
    finally:
        db.close()


class FocusedTransferProbe(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-transfer-proof-")
        self.task = Path(self.temporary.name).resolve()
        (self.task / ".native-evidence").touch()
        self.root = self.task / "fixture"
        self.data = self.root / "data/cagents-development"
        (self.data / "windows/window-2").mkdir(parents=True)
        self.repository = self.root / "project"
        self.destination = self.repository / "linked"
        self.destination.mkdir(parents=True)
        self.registry = self.data / "workspace-model.sqlite3"
        self.stores = [self.data / "sessions.sqlite3", self.data / "windows/window-2/sessions.sqlite3"]
        self.prepared = self.task / "prepared.json"
        self.before = self.task / "before.json"
        self.moved, self.sibling, self.occupant, self.channel, self.registry_id = [uid() for _ in range(5)]
        self.workspace_ids = [uid(), uid()]
        self.project_ids = [uid(), uid()]
        self.worktree_ids = [uid(), uid()]
        self.panes = {value: dict(id=value, title=name, user_title=name,
                                 pending_terminal_start=False, width_fraction=0.5,
                                 kind={"Browser": dict(id=uid(), current_url="about:blank", history=[],
                                                       history_index=0, scroll_x=0.0, scroll_y=0.0, fidelity="url_only")},
                                 scope=None)
                      for value, name in [(self.moved, "Moved"), (self.sibling, "Sibling"), (self.occupant, "Existing") ]}
        self.panes[self.occupant]["scope"] = str(self.destination)
        with writable(self.registry) as db:
            db.executescript("""
                CREATE TABLE metadata(key INTEGER PRIMARY KEY, identity TEXT);
                CREATE TABLE channels(id TEXT PRIMARY KEY,repository TEXT,title TEXT,title_source INTEGER);
                CREATE TABLE view_scopes(id INTEGER PRIMARY KEY,channel TEXT,repository TEXT,worktree TEXT,revision INTEGER);
                CREATE TABLE panes(id TEXT PRIMARY KEY,scope INTEGER,position INTEGER,title TEXT,user_title TEXT,
                    width_fraction REAL,pending_terminal_start INTEGER,filing TEXT,kind TEXT,resource TEXT,
                    descriptor_format INTEGER,descriptor TEXT);
                CREATE TABLE retired_panes(scope INTEGER,pane TEXT,ordinal INTEGER);
            """)
            db.execute("INSERT INTO metadata VALUES(0,?)", (self.registry_id,))
            db.execute("INSERT INTO channels VALUES(?,?,?,2)", (self.channel, str(self.repository), "Focused transfer proof"))
            db.execute("INSERT INTO view_scopes VALUES(10,?,NULL,NULL,7)", (self.channel,))
            db.execute("INSERT INTO view_scopes VALUES(20,NULL,?,?,8)", (str(self.repository), str(self.destination)))
            self.put_pane(db, self.sibling, 10, 1)
            self.put_pane(db, self.moved, 10, 2)
            self.put_pane(db, self.occupant, 20, 1)
        for index, path in enumerate(self.stores):
            workspace, project = self.workspace_ids[index], self.project_ids[index]
            with writable(path) as db:
                db.executescript("""
                    PRAGMA user_version=13;
                    CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT);
                    CREATE TABLE workspaces(workspace_id TEXT PRIMARY KEY,existence_json TEXT,focused_card_id TEXT);
                    CREATE TABLE projects(project_id TEXT PRIMARY KEY,repository_json TEXT,existence_json TEXT);
                    CREATE TABLE worktrees(worktree_id TEXT PRIMARY KEY,project_id TEXT,path_json TEXT,existence_json TEXT);
                    CREATE TABLE cards(card_id TEXT PRIMARY KEY,workspace_id TEXT,existence_json TEXT,kind_tag TEXT);
                    CREATE TABLE browser_restore(card_id TEXT PRIMARY KEY,browser_id TEXT);
                """)
                db.execute("INSERT INTO workspaces VALUES(?,?,?)", (workspace, '"open"', self.moved if index == 0 else self.occupant))
                db.execute("INSERT INTO projects VALUES(?,?,?)", (project, json.dumps(str(self.repository)), '"open"'))
                db.execute("INSERT INTO worktrees VALUES(?,?,?,?)", (self.worktree_ids[index], project, json.dumps(str(self.destination)), '"open"'))
                for pane in self.panes.values():
                    db.execute("INSERT INTO cards VALUES(?,?,?,?)", (pane["id"], workspace, '"open"', "browser"))
                    db.execute("INSERT INTO browser_restore VALUES(?,?)", (pane["id"], pane["kind"]["Browser"]["id"]))
                values = dict(threads=[dict(id=self.channel, project_id=project, title="Focused transfer proof",
                                           title_source="Manual", cards=[self.sibling, self.moved], focused=self.moved)],
                              active_thread=self.channel if index == 0 else None,
                              active_worktree=None if index == 0 else str(self.destination),
                              worktree_focus={str(self.destination): self.occupant},
                              card_scopes={self.occupant: str(self.destination)},
                              workspace_model_identity=self.registry_id)
                for name, value in values.items():
                    db.execute("INSERT INTO metadata VALUES(?,?)", (f"workspace:{workspace}:navigation:{name}", json.dumps(value)))
        self.marker = dict(schema=1, event="focused_transfer_prepared", nonce=uid(), fixture_root=str(self.root),
                           namespace="cagents-development", registry=dict(path=str(self.registry), identity=self.registry_id, **self.identity(self.registry)),
                           stores=[dict(role=role, path=str(path), **self.identity(path)) for role, path in zip(PROBE["ROLES"], self.stores)],
                           source=dict(channel=self.channel, title="Focused transfer proof", repository=str(self.repository),
                                       scope={"Channel": self.channel}, revision=7,
                                       panes=[self.panes[self.sibling], self.panes[self.moved]], retired=[]),
                           destination=dict(scope={"Worktree": dict(repository=str(self.repository), path=str(self.destination))},
                                            revision=8, panes=[self.panes[self.occupant]], retired=[]),
                           sibling=self.sibling, moved=self.moved)
        self.write_marker()

    def tearDown(self):
        self.temporary.cleanup()

    @staticmethod
    def identity(path):
        info = path.stat()
        return dict(device=info.st_dev, inode=info.st_ino)

    def put_pane(self, db, pane_id, scope, position):
        pane = self.panes[pane_id]
        payload = pane["kind"]["Browser"]
        db.execute("INSERT INTO panes VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                   (pane_id, scope, position, pane["title"], pane["user_title"], pane["width_fraction"],
                    0, pane["scope"], "browser", payload["id"], 1, json.dumps(dict(kind="browser", **payload))))

    def write_marker(self):
        self.prepared.write_text(json.dumps(self.marker))

    def files(self):
        return {str(path.relative_to(self.root)): hashlib.sha256(path.read_bytes()).hexdigest()
                for path in self.root.rglob("*") if path.is_file()}

    def observe(self, phase="before", before=None):
        original = self.files()
        try:
            return PROBE["observe"](self.root, self.prepared, phase, before)
        finally:
            self.assertEqual(self.files(), original, "The probe changed fixture files")

    def metadata(self, index, name, transform):
        key = f"workspace:{self.workspace_ids[index]}:navigation:{name}"
        with writable(self.stores[index]) as db:
            current = json.loads(db.execute("SELECT value FROM metadata WHERE key=?", (key,)).fetchone()[0])
            db.execute("UPDATE metadata SET value=? WHERE key=?", (json.dumps(transform(current)), key))

    def transfer(self):
        self.before.write_text(json.dumps(self.observe()))
        self.go()
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET scope=20,position=2 WHERE id=?", (self.moved,))
            db.execute("UPDATE view_scopes SET revision=revision+2")
            db.execute("INSERT INTO retired_panes VALUES(10,?,0)", (self.moved,))
        for index, path in enumerate(self.stores):
            def change(channels):
                channels[0]["cards"] = [self.sibling]
                channels[0]["focused"] = self.sibling
                return channels
            self.metadata(index, "threads", change)
            self.metadata(index, "card_scopes", lambda value: {**value, self.moved: str(self.destination)})
            if index == 0:
                with writable(path) as db:
                    db.execute("UPDATE workspaces SET focused_card_id=?", (self.sibling,))

    def go(self):
        self.prepared.with_suffix(".go").write_text(json.dumps(dict(
            schema=1, event="focused_transfer_go", nonce=self.marker["nonce"], registry=self.registry_id,
            source_channel=self.channel, sibling=self.sibling, moved=self.moved,
            destination_scope=self.marker["destination"]["scope"])))

    def test_before_binds_two_distinct_stores_and_local_identities_without_writes(self):
        proof = self.observe()
        self.assertEqual([store["workspace_id"] for store in proof["stores"]], self.workspace_ids)
        self.assertEqual([store["project_id"] for store in proof["stores"]], self.project_ids)
        self.assertEqual(proof["stores"][0]["global_focused"], self.moved)
        self.assertEqual(proof["stores"][1]["source_focused"], self.moved)
        self.assertTrue(proof["stores"][1]["destination_selected"])

    def test_after_and_reopened_require_exact_sibling_focus_and_destination_ownership(self):
        self.transfer()
        for phase in ("after", "reopened"):
            proof = self.observe(phase, self.before)
            for store in proof["stores"]:
                self.assertEqual(store["source_cards"], [self.sibling])
                self.assertEqual(store["source_focused"], self.sibling)
                self.assertEqual(store["moved_scope"], str(self.destination))
            self.assertEqual([pane["id"] for pane in proof["registry"]["destination"]["panes"]], [self.occupant, self.moved])

    def test_post_projection_filing_and_inactive_destination_focus_are_valid(self):
        self.transfer()
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET filing=? WHERE id=?", (str(self.destination), self.moved))
        with writable(self.stores[1]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (self.moved,))
        self.assertEqual(self.observe("after", self.before)["stores"][1]["global_focused"], self.moved)

    def test_wrong_before_source_focus_active_global_focus_and_inactive_selection_refuse(self):
        mutations = [
            ("threads", lambda value: [{**value[0], "focused": self.sibling}]),
            ("threads", lambda value: [{**value[0], "cards": [self.moved]}]),
            ("workspace_model_identity", lambda _: uid()),
            ("active_thread", lambda _: self.channel),
        ]
        for field, transform in mutations:
            with self.subTest(field=field):
                with writable(self.stores[1]) as db:
                    key = f"workspace:{self.workspace_ids[1]}:navigation:{field}"
                    saved = db.execute("SELECT value FROM metadata WHERE key=?", (key,)).fetchone()[0]
                self.metadata(1, field, transform)
                with self.assertRaises(PROBE["Refused"]):
                    self.observe()
                with writable(self.stores[1]) as db:
                    db.execute("UPDATE metadata SET value=? WHERE key=?", (saved, key))
        with writable(self.stores[0]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (self.sibling,))
        with self.assertRaisesRegex(PROBE["Refused"], "global focus"):
            self.observe()

    def test_post_null_or_moved_source_focus_is_never_a_fallback_proof(self):
        self.transfer()
        for focus in (None, self.moved, self.occupant):
            with self.subTest(focus=focus):
                self.metadata(1, "threads", lambda value: [{**value[0], "focused": focus}])
                with self.assertRaisesRegex(PROBE["Refused"], "membership/focus"):
                    self.observe("after", self.before)

    def test_stale_placement_any_channel_or_duplicate_pre_membership_refuses(self):
        self.metadata(1, "threads", lambda value: [*value, dict(value[0], id=uid(), cards=[self.moved])])
        with self.assertRaisesRegex(PROBE["Refused"], "placement"):
            self.observe()

    def test_post_moved_in_other_workspace_channel_is_refused(self):
        self.transfer()
        with writable(self.stores[1]) as db:
            db.execute("INSERT INTO metadata VALUES(?,?)", (f"workspace:{uid()}:navigation:threads",
                       json.dumps([dict(id=uid(), cards=[self.moved])])) )
        with self.assertRaisesRegex(PROBE["Refused"], "placement"):
            self.observe("after", self.before)

    def test_wrong_filing_extra_destination_occupant_or_wrong_browser_payload_refuses(self):
        self.transfer()
        original = None
        for field, change in [("card_scopes", lambda value: {**value, self.moved: str(self.repository)}),
                              ("card_scopes", lambda value: {**value, self.sibling: str(self.destination)})]:
            with self.subTest(change=change):
                key = f"workspace:{self.workspace_ids[0]}:navigation:{field}"
                with writable(self.stores[0]) as db:
                    original = db.execute("SELECT value FROM metadata WHERE key=?", (key,)).fetchone()[0]
                self.metadata(0, field, change)
                with self.assertRaises(PROBE["Refused"]):
                    self.observe("after", self.before)
                with writable(self.stores[0]) as db:
                    db.execute("UPDATE metadata SET value=? WHERE key=?", (original, key))
        with writable(self.stores[0]) as db:
            db.execute("UPDATE browser_restore SET browser_id=? WHERE card_id=?", (uid(), self.moved))
        with self.assertRaisesRegex(PROBE["Refused"], "payload identity"):
            self.observe("after", self.before)

    def test_changed_existing_destination_descriptor_refuses(self):
        self.transfer()
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET title='Changed' WHERE id=?", (self.occupant,))
        with self.assertRaisesRegex(PROBE["Refused"], "destination descriptors"):
            self.observe("after", self.before)

    def test_wrong_canonical_moved_path_and_stale_revisions_refuse(self):
        self.transfer()
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET filing=? WHERE id=?", (str(self.repository), self.moved))
        with self.assertRaisesRegex(PROBE["Refused"], "filing"):
            self.observe("after", self.before)
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET filing=NULL WHERE id=?", (self.moved,))
            db.execute("UPDATE view_scopes SET revision=7 WHERE id=10")
        with self.assertRaisesRegex(PROBE["Refused"], "revision"):
            self.observe("after", self.before)

    def test_identity_and_nonce_mismatches_refuse(self):
        self.transfer()
        go = self.prepared.with_suffix(".go")
        value = json.loads(go.read_text())
        value["nonce"] = uid()
        go.write_text(json.dumps(value))
        with self.assertRaisesRegex(PROBE["Refused"], "Go marker"):
            self.observe("after", self.before)
        self.go()
        baseline = json.loads(self.before.read_text())
        baseline["stores"][0]["workspace_id"] = uid()
        self.before.write_text(json.dumps(baseline))
        with self.assertRaisesRegex(PROBE["Refused"], "identity changed since"):
            self.observe("after", self.before)

    def test_same_or_hardlinked_databases_refuse(self):
        self.stores[1].unlink()
        os.link(self.stores[0], self.stores[1])
        self.marker["stores"][1].update(self.identity(self.stores[1]))
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "hardlinked"):
            self.observe()
        self.marker["stores"][1]["path"] = str(self.stores[0])
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "database path"):
            self.observe()

    def test_stale_database_stat_or_registry_id_refuses(self):
        self.marker["stores"][0]["inode"] += 1
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "Database identity"):
            self.observe()
        self.marker["stores"][0].update(self.identity(self.stores[0]))
        self.marker["registry"]["identity"] = uid()
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "Registry identity"):
            self.observe()

    def test_missing_unknown_or_view_schema_refuses_without_migration(self):
        with writable(self.stores[0]) as db:
            db.execute("PRAGMA user_version=999")
        with self.assertRaisesRegex(PROBE["Refused"], "schema"):
            self.observe()
        with writable(self.stores[0]) as db:
            db.execute("PRAGMA user_version=13")
            db.execute("DROP TABLE browser_restore")
            db.execute("CREATE VIEW browser_restore AS SELECT card_id,'fake' AS browser_id FROM cards")
        with self.assertRaisesRegex(PROBE["Refused"], "unsafe table"):
            self.observe()

    def test_wrong_root_noncanonical_symlink_and_outside_marker_refuse(self):
        alias = self.task / "alias.json"
        alias.symlink_to(self.prepared)
        original = self.prepared
        self.prepared = alias
        with self.assertRaisesRegex(PROBE["Refused"], "canonical"):
            self.observe()
        self.prepared = self.root / "inside.json"
        self.prepared.write_text(original.read_text())
        with self.assertRaisesRegex(PROBE["Refused"], "outside the fixture"):
            self.observe()
        self.prepared = original
        (self.task / ".native-evidence").unlink()
        with self.assertRaises(FileNotFoundError):
            self.observe()

    def test_wrong_phase_missing_before_and_early_go_refuse(self):
        self.go()
        with self.assertRaisesRegex(PROBE["Refused"], "precede"):
            self.observe()
        with self.assertRaisesRegex(PROBE["Refused"], "original before"):
            self.observe("after")

    def test_each_store_uses_a_consistent_read_transaction_during_concurrent_commit(self):
        # WAL allows a real writer to commit while the probe retains its old
        # snapshot. Seeing new channel focus with old global focus would fail.
        with writable(self.stores[0]) as db:
            db.execute("PRAGMA journal_mode=WAL")
        real_connect = sqlite3.connect
        fixture = self
        changed = []

        class ConcurrentConnection(sqlite3.Connection):
            def execute(self, sql, parameters=()):
                if sql.startswith("SELECT key,value FROM metadata") and not changed:
                    writer = real_connect(fixture.stores[0])
                    try:
                        with writer:
                            writer.execute("UPDATE workspaces SET focused_card_id=?", (fixture.sibling,))
                            key = f"workspace:{fixture.workspace_ids[0]}:navigation:threads"
                            channels = json.loads(writer.execute("SELECT value FROM metadata WHERE key=?", (key,)).fetchone()[0])
                            channels[0]["focused"] = fixture.sibling
                            writer.execute("UPDATE metadata SET value=? WHERE key=?", (json.dumps(channels), key))
                        changed.append(True)
                    finally:
                        writer.close()
                return super().execute(sql, parameters)

        def connect(*args, **kwargs):
            return real_connect(*args, **kwargs, factory=ConcurrentConnection)

        with mock.patch.object(sqlite3, "connect", connect):
            proof = PROBE["observe"](self.root, self.prepared, "before")
        self.assertEqual(changed, [True])
        self.assertEqual(proof["stores"][0]["global_focused"], self.moved)
        self.assertEqual(proof["stores"][0]["source_focused"], self.moved)

    def test_locked_database_fails_with_bounded_busy_wait_and_no_writes(self):
        locker = sqlite3.connect(self.stores[0])
        try:
            locker.execute("BEGIN EXCLUSIVE")
            with self.assertRaisesRegex(sqlite3.OperationalError, "locked"):
                self.observe()
        finally:
            locker.rollback()
            locker.close()

    def test_writable_fixture_directory_and_stale_global_identity_refuse(self):
        self.root.chmod(0o777)
        with self.assertRaisesRegex(PROBE["Refused"], "private owned"):
            self.observe()
        self.root.chmod(0o755)
        with writable(self.stores[1]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (uid(),))
        with self.assertRaisesRegex(PROBE["Refused"], "Global focus identity is stale"):
            self.observe()

    def test_duplicate_json_identity_fields_refuse(self):
        value = self.prepared.read_text()
        self.prepared.write_text(value[:-1] + ',"nonce":"' + uid() + '"}')
        with self.assertRaisesRegex(PROBE["Refused"], "Duplicate JSON"):
            self.observe()

    def test_inactive_global_focus_must_belong_to_a_valid_selected_scope(self):
        for invalid in (None, self.sibling):
            with self.subTest(focus=invalid):
                with writable(self.stores[1]) as db:
                    db.execute("UPDATE workspaces SET focused_card_id=?", (invalid,))
                with self.assertRaisesRegex(PROBE["Refused"], "selected scope"):
                    self.observe()
        with writable(self.stores[1]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (self.occupant,))
        self.metadata(1, "active_thread", lambda _: uid())
        with self.assertRaisesRegex(PROBE["Refused"], "Active channel identity is stale"):
            self.observe()

    def test_inactive_other_channel_requires_its_actual_member_as_global_focus(self):
        selected = uid()
        self.metadata(1, "threads", lambda value: [*value, dict(value[0], id=selected, cards=[self.occupant], focused=self.occupant)])
        self.metadata(1, "active_thread", lambda _: selected)
        self.assertEqual(self.observe()["stores"][1]["active_channel"], selected)
        with writable(self.stores[1]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (self.sibling,))
        with self.assertRaisesRegex(PROBE["Refused"], "selected scope"):
            self.observe()

    def test_malformed_namespace_and_changed_marker_path_refuse(self):
        self.marker["namespace"] = "cagents-development--bad"
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "namespace"):
            self.observe()
        self.marker["namespace"] = "cagents-development"
        self.write_marker()
        self.transfer()
        copied = self.task / "copied-prepared.json"
        copied.write_text(self.prepared.read_text())
        self.prepared = copied
        self.go()
        with self.assertRaisesRegex(PROBE["Refused"], "Stale or unrelated"):
            self.observe("after", self.before)

    def test_cli_creates_only_new_task_evidence_and_preserves_database_bytes(self):
        output = self.task / "proof.json"
        original = self.files()
        command = [sys.executable, "-I", "-B", str(PROBE_PATH), "--root", str(self.root),
                   "--prepared", str(self.prepared), "--phase", "before", "--output", str(output)]
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(output.read_text())["phase"], "before")
        self.assertEqual(self.files(), original)
        self.assertEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 2)


if __name__ == "__main__":
    unittest.main()
