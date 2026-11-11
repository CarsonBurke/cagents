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
CAPTURE = json.loads((PROBE_PATH.parent / "focused-transfer-producer.json").read_text())


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
        self.repository = self.root / "project/.git"
        self.repository.mkdir(parents=True)
        self.destination = self.root / "project"
        self.registry = self.data / "workspace-model.sqlite3"
        self.stores = [self.data / "sessions.sqlite3", self.data / "windows/window-2/sessions.sqlite3"]
        self.prepared = self.task / "prepared.json"
        self.before = self.task / "before.json"
        self.marker = self.rebase(copy.deepcopy(CAPTURE["prepared"]))
        self.moved, self.sibling = self.marker["moved"], self.marker["sibling"]
        self.occupant = self.marker["destination"]["panes"][0]["id"]
        self.channel = self.marker["source"]["channel"]
        self.registry_id = self.marker["registry"]["identity"]
        self.panes = {pane["id"]: pane for scope in ("source", "destination") for pane in self.marker[scope]["panes"]}
        # Use actual captured table definitions and rows, including normalized
        # browser_restore and the common-.git project identity. Only private
        # paths and filesystem identities are rebased. The stopped capture was
        # not a valid pre-go focus state: controlled selections below establish
        # test preconditions rather than relabel the failed native evidence.
        for relative, database in CAPTURE["databases"].items():
            with writable(self.root / relative) as db:
                db.execute("PRAGMA user_version=" + str(database["user_version"]))
                for table, captured in database["tables"].items():
                    db.execute(captured["sql"])
                    for original in captured["rows"]:
                        row = self.rebase(original)
                        columns = ",".join(row)
                        db.execute("INSERT INTO " + table + " (" + columns + ") VALUES (" + ",".join("?" for _ in row) + ")", tuple(row.values()))
        self.workspace_ids, self.project_ids, self.worktree_ids = [], [], []
        for index, path in enumerate(self.stores):
            with writable(path) as db:
                self.workspace_ids.append(db.execute("SELECT workspace_id FROM workspaces").fetchone()[0])
                self.project_ids.append(db.execute("SELECT project_id FROM projects").fetchone()[0])
                self.worktree_ids.append(db.execute("SELECT worktree_id FROM worktrees").fetchone()[0])
                db.execute("UPDATE workspaces SET focused_card_id=?", (self.moved if index == 0 else self.occupant,))
            self.metadata(index, "threads", lambda channels: [{**channel, "focused": self.moved} for channel in channels])
            self.metadata(index, "active_thread", lambda _: self.channel if index == 0 else None)
        with writable(self.registry) as db:
            self.source_scope = db.execute("SELECT id FROM view_scopes WHERE channel=?", (self.channel,)).fetchone()[0]
            self.destination_scope = db.execute("SELECT id FROM view_scopes WHERE repository=? AND worktree=?", (str(self.repository), str(self.destination))).fetchone()[0]
        self.marker["registry"].update(self.identity(self.registry))
        for entry, path in zip(self.marker["stores"], self.stores):
            entry.update(self.identity(path))
        self.write_marker()

    def rebase(self, value):
        if isinstance(value, dict):
            return {key: self.rebase(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.rebase(item) for item in value]
        if isinstance(value, str):
            return value.replace(CAPTURE["prepared"]["fixture_root"], str(self.root))
        return value

    def tearDown(self):
        self.temporary.cleanup()

    @staticmethod
    def identity(path):
        info = path.stat()
        return dict(device=info.st_dev, inode=info.st_ino)

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
            db.execute("UPDATE panes SET scope=?,position=(SELECT max(position)+1048576 FROM panes WHERE scope=?) WHERE id=?",
                       (self.destination_scope, self.destination_scope, self.moved))
            db.execute("UPDATE view_scopes SET revision=revision+2")
            db.execute("INSERT INTO retired_panes(scope,pane,ordinal,retired_at_unix_ms) VALUES(?,?,0,0)", (self.source_scope, self.moved))
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

    def test_literal_producer_panes_match_captured_canonical_rows(self):
        prepared = CAPTURE["prepared"]
        registry = CAPTURE["databases"]["data/cagents-development/workspace-model.sqlite3"]
        captured = {row["id"]: row for row in registry["tables"]["panes"]["rows"]}
        for scope in (prepared["source"], prepared["destination"]):
            for pane in scope["panes"]:
                with self.subTest(pane=pane["id"]):
                    self.assertEqual(pane["kind"]["kind"], "browser")
                    self.assertEqual(PROBE["pane_key"](pane), PROBE["pane_key"](PROBE["observed_pane"](captured[pane["id"]])))

    def test_captured_window_browser_payload_is_normalized_separately(self):
        prepared = CAPTURE["prepared"]
        for relative, database in CAPTURE["databases"].items():
            if relative.endswith("workspace-model.sqlite3"):
                continue
            cards = {row["card_id"]: row for row in database["tables"]["cards"]["rows"]}
            browsers = {row["card_id"]: row for row in database["tables"]["browser_restore"]["rows"]}
            for pane in prepared["source"]["panes"]:
                card, browser = cards[pane["id"]], browsers[pane["id"]]
                self.assertEqual(card["kind_tag"], "browser")
                self.assertIsNone(card["content_json"])
                self.assertEqual(browser["browser_id"], pane["kind"]["id"])
                self.assertEqual(pane["kind"]["history"], [])
                self.assertEqual(json.loads(browser["history_json"]), ["about:blank"])

    def test_binary_enum_shape_is_refused_by_the_json_consumer(self):
        pane = copy.deepcopy(self.marker["source"]["panes"][0])
        payload = dict(pane["kind"])
        payload.pop("kind")
        pane["kind"] = {"Browser": payload}
        self.marker["source"]["panes"][0] = pane
        self.write_marker()
        with self.assertRaisesRegex(PROBE["Refused"], "canonical JSON pane kind"):
            self.observe()

    def test_actual_browser_dto_keeps_strict_identity_types_and_resource(self):
        pane = self.marker["source"]["panes"][0]
        for name, bad in (("id", None), ("history_index", True), ("scroll_x", False), ("fidelity", "unknown")):
            with self.subTest(name=name):
                changed = copy.deepcopy(pane)
                changed["kind"][name] = bad
                with self.assertRaises(PROBE["Refused"]):
                    PROBE["pane_key"](changed)
        changed = copy.deepcopy(pane)
        changed["pending_terminal_start"] = 0
        with self.assertRaisesRegex(PROBE["Refused"], "field types"):
            PROBE["pane_key"](changed)
        with writable(self.registry) as db:
            db.execute("UPDATE panes SET resource=? WHERE id=?", (uid(), self.moved))
        with self.assertRaisesRegex(PROBE["Refused"], "resource identity"):
            self.observe()

    def test_stopped_native_capture_remains_an_invalid_before_selection(self):
        # The failed native artifact was stopped without completing the focus
        # setup. Repairing its DTO parser must not turn that state into proof.
        relative = "data/cagents-development/sessions.sqlite3"
        captured = CAPTURE["databases"][relative]["tables"]
        with writable(self.stores[0]) as db:
            db.execute("UPDATE workspaces SET focused_card_id=?", (captured["workspaces"]["rows"][0]["focused_card_id"],))
            for original in captured["metadata"]["rows"]:
                row = self.rebase(original)
                db.execute("UPDATE metadata SET value=? WHERE key=?", (row["value"], row["key"]))
        with self.assertRaisesRegex(PROBE["Refused"], "Source membership/focus"):
            self.observe()

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
            db.execute("UPDATE view_scopes SET revision=? WHERE id=?", (self.marker["source"]["revision"], self.source_scope))
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
