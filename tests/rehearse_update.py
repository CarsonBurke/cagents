#!/usr/bin/env python3
"""Pure contracts for the update rehearsal; no compositor or installed daemon."""
from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

REPOSITORY = Path(__file__).resolve().parent.parent
SCRIPT = REPOSITORY / "scripts/rehearse-update"
spec = importlib.util.spec_from_file_location("rehearsal_records", REPOSITORY / "scripts/rehearse_update_records.py")
records = importlib.util.module_from_spec(spec)
spec.loader.exec_module(records)
AUTHORITY = ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN")


def snapshot() -> dict:
    return {"schema": 1, "errors": [], "windows": {
        "main": {"id": 1, "app_id": "cagents", "title": "Main", "size": [1000, 800], "painted": True}},
        "terminals": {"terminal": {"id": "terminal", "state": "running"}},
        "sessions": {"lineage": {"id": "chat-old", "lineage": "lineage", "state": "Working"}},
        "agents": {"agent": {"agent": {"id": "agent", "name": "named"}, "state": "Running"}},
        "channels": {"channel": {"id": "channel", "title": "Release Details: rollout"}},
        "projects": {},
        "stores": {"windows/main/sessions.sqlite3": {"rows": {
            "cards": [{"card_id": "pane", "workspace_id": "workspace", "ordinal": 0,
                       "kind_tag": "terminal", "title": "Shell", "existence": "open"}],
            "terminal_recipes": [{"card_id": "pane", "terminal_id": "terminal"}]}}},
        "processes": [{"pid": 42, "ppid": 10, "start_ticks": 123,
                       "terminal_id": "terminal", "argv": ["sleep", "86400"]}]}


class Comparison(unittest.TestCase):
    def test_same_record_does_not_lose_items(self):
        self.assertEqual(records.compare(snapshot(), snapshot()), [])

    def test_window_compositor_id_and_focus_are_not_persistent_identity(self):
        before, after = snapshot(), snapshot()
        after["windows"]["main"].update(id=20, focused=True)
        self.assertEqual(records.compare(before, after), [])

    def test_reports_losses_state_changes_and_reused_pid(self):
        before, after = snapshot(), snapshot()
        del after["windows"]["main"]
        del after["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][0]
        del after["channels"]["channel"]
        after["agents"]["agent"]["state"] = "Failed"
        after["terminals"]["terminal"]["state"] = "exited"
        after["processes"][0]["start_ticks"] = 124
        changes = records.compare(before, after)
        self.assertEqual({row["kind"] for row in changes},
                         {"windows", "panes", "channels", "agents", "terminals", "terminal-child"})
        self.assertTrue(all(row["regression"] for row in changes))

    def test_chat_resume_and_healthy_completion_are_reported(self):
        before, after = snapshot(), snapshot()
        after["sessions"]["lineage"].update(id="chat-new", state="Idle")
        changes = records.compare(before, after)
        self.assertEqual({row["change"] for row in changes}, {"id", "state"})
        self.assertFalse(any(row["regression"] for row in changes))
        after["sessions"]["lineage"]["state"] = "Stopped"
        self.assertTrue(any(row["regression"] for row in records.compare(before, after)))

    def test_evidence_failure_is_not_an_empty_successful_inventory(self):
        before, after = snapshot(), snapshot()
        after["errors"].append("terminals.json: command refused")
        self.assertTrue(records.compare(before, after)[0]["regression"])

    def test_record_reads_old_cli_envelopes_and_main_window_database(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-old-") as temporary:
            root = Path(temporary)
            out = root / "records/before"
            out.mkdir(parents=True)
            inventories = {
                "agents": [{"agent": {"id": "a", "name": "codex-alpha", "spec": {"provider": "Codex"}},
                            "state": "Running", "run": "r"}],
                "terminals": [{"id": "t", "state": "running", "title": "shell"}],
                "sessions": [{"id": "s", "lineage": "lineage", "surface": "Chat", "state": "Idle"}],
                "channels": [{"id": "c", "title": "Release Details: rollout", "repository": "/alpha/.git"}],
                "projects": [{"repository": "/alpha/.git", "name": "alpha"},
                             {"repository": "/beta/.git", "name": "beta"}],
            }
            for name, rows in inventories.items():
                (out / f"{name}.json").write_text(records.dump({"ok": True, "data": {name: rows}}))
            database = root / "home/.local/share/cagents/sessions.sqlite3"
            database.parent.mkdir(parents=True)
            with sqlite3.connect(database) as db:
                db.execute("CREATE TABLE cards(card_id TEXT, title TEXT, existence_json TEXT, kind_tag TEXT)")
                db.execute("INSERT INTO cards VALUES ('a', 'Codex agent', '\"open\"', 'chat')")
                db.commit()
            windows = out / "windows.json"
            windows.write_text(records.dump({"main": {"window": {"id": 1, "app_id": "cagents", "title": "cagents"},
                                                    "painted": True}}))
            captured = records.record(root, out, windows)
            self.assertEqual(captured["errors"], [])
            self.assertEqual(set(captured["projects"]), {"/alpha/.git", "/beta/.git"})
            self.assertEqual(captured["agents"]["a"]["agent"]["name"], "codex-alpha")
            self.assertIn("sessions.sqlite3:a", records.panes(captured))

    def test_child_pid_file_tracks_identity_without_terminal_environment(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-process-") as temporary:
            root = Path(temporary)
            peer = subprocess.Popen(["sleep", "86400"], env={**os.environ, "HOME": str(root / "home")})
            try:
                (root / "child.pid").write_text(str(peer.pid))
                errors = []
                captured = records.processes(root, errors)
                self.assertEqual(errors, [])
                child = next(row for row in captured if row["pid"] == peer.pid)
                self.assertEqual(child["role"], "long-lived-child")
                before, after = snapshot(), snapshot()
                before["processes"] = [child]
                after["processes"] = []
                changes = records.compare(before, after)
                self.assertEqual(changes[0]["kind"], "terminal-child")
                self.assertTrue(changes[0]["regression"])
            finally:
                peer.terminate()
                peer.wait()

    def test_cli_accepts_record_directories_and_returns_failure(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-record-") as temporary:
            root = Path(temporary)
            for name in ("before", "after"):
                (root / name).mkdir()
                (root / name / "snapshot.json").write_text(records.dump(snapshot()))
            command = [str(REPOSITORY / "scripts/rehearse_update_records.py"), "compare",
                       str(root / "before"), str(root / "after")]
            self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
            value = snapshot()
            value["terminals"] = {}
            (root / "after/snapshot.json").write_text(records.dump(value))
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("terminals terminal: disappeared", result.stdout)

    def test_database_capture_reads_wal_and_does_not_mutate_database(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-store-") as temporary:
            home = Path(temporary)
            path = home / ".local/share/cagents/windows/main/sessions.sqlite3"
            path.parent.mkdir(parents=True)
            with sqlite3.connect(path) as db:
                db.execute("PRAGMA journal_mode=WAL")
                db.execute("CREATE TABLE cards(card_id TEXT, title TEXT, existence_json TEXT)")
                db.execute("INSERT INTO cards VALUES ('one', 'Details: pane', '\"open\"')")
                db.commit()
                original = path.read_bytes()
                errors = []
                captured = records.stores(home, errors)
                self.assertEqual(errors, [])
                self.assertEqual(captured["windows/main/sessions.sqlite3"]["rows"]["cards"][0]["title"],
                                 "Details: pane")
                self.assertEqual(path.read_bytes(), original)


class Pty(unittest.TestCase):
    def run_peer(self, body, *options):
        command = [sys.executable, str(REPOSITORY / "scripts/rehearse_update_pty.py"),
                   *options, "--", sys.executable, "-c", body]
        return subprocess.run(command, capture_output=True, text=True, timeout=10)

    def test_waits_for_split_prompt_and_answers_each_prompt_on_controlling_tty(self):
        result = self.run_peer(
            "import os,sys,time; "
            "assert sys.stdin.isatty(); "
            "tty=open('/dev/tty','r'); "
            "sys.stdout.write('Proceed with this update? [y/'); sys.stdout.flush(); "
            "time.sleep(.1); sys.stdout.write('N] '); sys.stdout.flush(); "
            "assert tty.readline().strip() == 'yes'; "
            "sys.stdout.write('End them and continue? [Y/n] '); sys.stdout.flush(); "
            "assert tty.readline().strip() == 'yes'; "
            "print('both confirmations received')", "--answer", "yes")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("[rehearsal prompt="), 2)
        self.assertIn("both confirmations received", result.stdout)

    def test_child_exit_status_is_preserved(self):
        result = self.run_peer("raise SystemExit(7)")
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)

    def test_timeout_kills_peer_that_ignores_sigterm(self):
        result = self.run_peer("import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); signal.signal(signal.SIGHUP, signal.SIG_IGN); time.sleep(30)",
                               "--timeout", ".2")
        self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
        self.assertIn("exceeded PTY timeout", result.stdout)


class Arguments(unittest.TestCase):
    def test_inherited_authority_is_refused_before_any_action(self):
        for name in AUTHORITY:
            with self.subTest(name=name):
                result = subprocess.run([str(SCRIPT), "--root", "/var/tmp/should-not-create", "init"],
                                        env={**os.environ, name: "fixture-authority"},
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 77)
                self.assertIn("inherited agent authority was not modified", result.stderr)

    @unittest.skipIf(any(name in os.environ for name in AUTHORITY),
                     "stop is tested in a genuine user shell; never strip authority")
    def test_stop_restores_only_its_private_preservation_fault(self):
        for fault in ("marker", "interrupted-rename", "unrelated-file", "unrelated-symlink"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(prefix="ru-stop-", dir="/var/tmp") as temporary:
                root = Path(temporary)
                (root / ".rehearse-update").touch()
                saved = root / "rt/cagents/holders.rehearsal-saved"
                saved.mkdir(parents=True)
                (saved / "private.json").write_text("private registry bytes")
                holders = saved.with_name("holders")
                if fault == "marker":
                    holders.write_text("rehearse-update: preservation unavailable\n")
                elif fault == "unrelated-file":
                    holders.write_text("unrelated file; preserve me")
                elif fault == "unrelated-symlink":
                    (root / "unrelated-target").write_text("unrelated file; preserve me")
                    holders.symlink_to(root / "unrelated-target")
                result = subprocess.run([str(SCRIPT), "--root", str(root), "stop"],
                                        capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, 1 if fault.startswith("unrelated-") else 0,
                                 result.stdout + result.stderr)
                if fault.startswith("unrelated-"):
                    if fault == "unrelated-symlink":
                        self.assertTrue(holders.is_symlink())
                    self.assertEqual(holders.read_text(), "unrelated file; preserve me")
                    self.assertEqual((saved / "private.json").read_text(), "private registry bytes")
                else:
                    self.assertTrue(holders.is_dir(), result.stdout + result.stderr)
                    self.assertFalse(saved.exists())
                    self.assertEqual((holders / "private.json").read_text(), "private registry bytes")

    @unittest.skipIf(any(name in os.environ for name in AUTHORITY),
                     "argument parsing is tested in a genuine user shell; never strip authority")
    def test_argument_checks(self):
        for arguments in (["--root", "/" + "x" * 41, "init"],
                          ["--root", "/var/tmp/bad space", "init"],
                          ["--root", "/var/tmp/nonexistent-rehearsal", "unknown"]):
            with self.subTest(arguments=arguments):
                result = subprocess.run([str(SCRIPT), *arguments], capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
