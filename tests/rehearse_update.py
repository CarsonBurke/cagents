#!/usr/bin/env python3
"""Pure contracts for the update rehearsal; no compositor or installed daemon."""
from __future__ import annotations

import importlib.util
import io
import copy
import hashlib
import json
import fcntl
from contextlib import closing
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import tarfile
import time
import signal
import shutil
import unittest
from unittest.mock import patch

REPOSITORY = Path(__file__).resolve().parent.parent
SCRIPT = REPOSITORY / "scripts/rehearse-update"
spec = importlib.util.spec_from_file_location("rehearsal_records", REPOSITORY / "scripts/rehearse_update_records.py")
records = importlib.util.module_from_spec(spec)
spec.loader.exec_module(records)
sys.modules["rehearse_update_records"] = records
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
    def paused_replacement(self):
        before, after = snapshot(), snapshot()
        source = {"host_session_id": "source", "boot_id": "boot", "host_incarnation": "old"}
        serving = {**source, "host_session_id": "target", "host_incarnation": "new"}
        before["serving"], after["serving"] = source, serving
        chat = {"id": "chat-old", "proof": source, "launch": {"session_id": "provider-issued-thread"}}
        for value in (before, after):
            value["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"].append({
                "card_id": "chat-pane", "workspace_id": "workspace", "ordinal": 1,
                "kind_tag": "chat", "existence": "open", "content": copy.deepcopy(chat)})
        census = {"version": 1, "transaction": "12345678-1234-1234-1234-123456789abc",
                  "source": source, "chats": [chat], "terminals": [], "checkpoint_before": None}
        receipt = {"version": 1, "transaction": census["transaction"],
                   "census_digest": hashlib.sha256(records.dump(census).encode()).hexdigest(),
                   "checkpoint_before": None, "outcome": {"state": "usable", "serving": serving,
                   "chats": 0, "terminals": 1, "paused_chats": ["chat-old"], "paused_terminals": []}}
        after["recovery"] = {"census": census, "digest": receipt["census_digest"],
                             "issued": receipt, "current": copy.deepcopy(receipt)}
        del after["sessions"]["lineage"]
        return before, after

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

    def test_exact_receipt_accounted_chat_is_paused_not_lost_or_live(self):
        before, after = self.paused_replacement()
        changes = records.compare(before, after)
        self.assertFalse(any(row["regression"] for row in changes), changes)
        self.assertTrue(any(row["change"] == "verified paused by update" for row in changes))
        after["sessions"]["lineage"] = {**before["sessions"]["lineage"], "state": "Stopped", "failure": "Update paused this chat"}
        changes = records.compare(before, after)
        self.assertFalse(any(row["regression"] for row in changes), changes)

    def test_paused_acceptance_rejects_unbound_foreign_or_stale_receipt(self):
        mutations = {
            "transaction": lambda b, a: a["recovery"]["current"].update(transaction="foreign"),
            "digest": lambda b, a: a["recovery"]["issued"].update(census_digest="foreign"),
            "checkpoint": lambda b, a: a["recovery"]["current"].update(checkpoint_before="foreign"),
            "not-ready": lambda b, a: a["recovery"]["current"]["outcome"].update(state="unverified"),
            "other-serving-owner": lambda b, a: a.update(serving={**a["serving"], "host_incarnation": "foreign"}),
            "other-source-owner": lambda b, a: b.update(serving={**b["serving"], "host_incarnation": "foreign"}),
            "foreign-chat": lambda b, a: a["recovery"]["census"]["chats"][0].update(id="foreign"),
            "foreign-pane-proof": lambda b, a: b["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][1]["content"].update(proof={"host_session_id": "foreign"}),
            "no-receipt": lambda b, a: a.pop("recovery"),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                before, after = self.paused_replacement()
                mutate(before, after)
                self.assertTrue(any(row["regression"] for row in records.compare(before, after)))

    def test_verified_pause_still_requires_saved_pane_and_original_terminal_child(self):
        before, after = self.paused_replacement()
        after["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"].pop()
        after["processes"] = []
        regressions = [row for row in records.compare(before, after) if row["regression"]]
        self.assertEqual({row["kind"] for row in regressions}, {"panes", "terminal-child"})

    def test_plain_restart_uses_fresh_verifier_on_the_original_census(self):
        before, after = self.paused_replacement()
        fresh = {**after['serving'],'host_session_id':'restarted','host_incarnation':'restarted-owner'}
        after['serving'] = fresh
        after['recovery']['current']['outcome']['serving'] = fresh
        self.assertEqual(records.verified_pauses(before,after),({'chat-old'},set()))
        for name, mutate in {
            'old-serving':lambda a:a.update(serving=before['serving']),
            'foreign-transaction':lambda a:a['recovery']['current'].update(transaction='foreign'),
            'foreign-digest':lambda a:a['recovery']['current'].update(census_digest='foreign'),
            'different-partition':lambda a:a['recovery']['current']['outcome'].update(paused_chats=[]),
            'different-count':lambda a:a['recovery']['current']['outcome'].update(chats=999),
            'unverified':lambda a:a['recovery']['current']['outcome'].update(state='unverified'),
            'wrong-boot':lambda a:a['recovery']['current']['outcome']['serving'].update(boot_id='foreign'),
        }.items():
            with self.subTest(name=name):
                candidate = copy.deepcopy(after)
                mutate(candidate)
                self.assertIsNone(records.verified_recovery(before,candidate))

    def test_verified_pause_does_not_accept_foreign_history_in_original_pane(self):
        before, after = self.paused_replacement()
        after["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][1]["content"]["launch"]["session_id"] = "foreign-thread"
        self.assertTrue(any(row["regression"] for row in records.compare(before, after)))

    def test_paused_history_requires_exact_verified_source_and_read_only_projection(self):
        before, after = self.paused_replacement()
        source = before['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content']
        source.update(native_continuity='ReconnectOnly',terminal=None,
                      owned_codex_listener={'socket':'/tmp/cagents-codex-chat-old/control.sock'})
        source['launch'].update(provider='Codex',recipe={'program':'codex',
                                'arguments':['--remote','unix:///tmp/cagents-codex-chat-old/control.sock']})
        after['recovery']['census']['chats'] = [copy.deepcopy(source)]
        after['recovery']['census']['chats'][0]['launch']['session_id'] = None
        projection = records.paused_history_projection(before,after,source)
        self.assertIsNotNone(projection)
        self.assertEqual(projection['launch']['recipe']['arguments'],['--remote','unix://'])
        target = after['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]
        target['content'] = projection
        self.assertFalse(any(row['regression'] for row in records.compare(before,after)))
        for field, value in [('proof',before['serving']),('terminal',{'id':'writer'}),
                             ('owned_codex_listener',source['owned_codex_listener'])]:
            with self.subTest(field=field):
                target['content'] = {**projection,field:value}
                self.assertTrue(any(row['regression'] for row in records.compare(before,after)))
        target['content'] = copy.deepcopy(projection)
        target['content']['launch']['recipe']['program'] = 'foreign'
        self.assertTrue(any(row['regression'] for row in records.compare(before,after)))
        target['content'] = projection
        after['recovery']['current']['outcome']['state'] = 'unverified'
        self.assertIsNone(records.paused_history_projection(before,after,source))

    def test_paused_agent_state_requires_its_exact_source_chat_and_keeps_agent(self):
        before, after = self.paused_replacement()
        chat = before["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][1]["content"]
        before["agents"]["agent"]["agent"]["session"] = {"Chat": copy.deepcopy(chat)}
        after["agents"]["agent"]["state"] = "Stopped"
        self.assertFalse(any(row["regression"] for row in records.compare(before, after)))
        before["agents"]["agent"]["agent"]["session"]["Chat"]["id"] = "foreign"
        self.assertTrue(any(row["regression"] for row in records.compare(before, after)))
        before["agents"]["agent"]["agent"]["session"]["Chat"]["id"] = "chat-old"
        del after["agents"]["agent"]
        self.assertTrue(any(row["regression"] for row in records.compare(before, after)))

    def test_history_socket_reset_uses_only_the_first_remote_argument(self):
        before, after = self.paused_replacement()
        source = before['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content']
        arguments = ['--remote','ssh://other','--remote','unix:///tmp/cagents-codex-chat-old/control.sock']
        source['launch'].update(provider='Codex',recipe={'program':'codex','arguments':arguments})
        after['recovery']['census']['chats'] = [copy.deepcopy(source)]
        projection = records.paused_history_projection(before,after,source)
        self.assertEqual(projection['launch']['recipe']['arguments'],arguments)

    def test_console_program_change_requires_exact_target_same_run_and_pane(self):
        before, after = self.paused_replacement()
        before['root'] = after['root'] = '/var/tmp/cr/test'
        after['target'] = {'version':'new'}
        after['update_status'] = {'phase':'Complete','version':'new','id':after['recovery']['census']['transaction']}
        program = '/var/tmp/cr/test/home/.local/share/cagents-install/versions/new/bin/cagents'
        recipe = {'program':'old/cagents','arguments':['agent','console','agent'],'environment':{},
                  'working_directory':'/private/project'}
        for value in (before,after):
            value['agents']['agent'].update(run='accepted-run',state='Running')
            value['agents']['agent']['agent']['console'] = {'id':'terminal','launch':copy.deepcopy(recipe)}
        after['agents']['agent']['agent']['console']['launch']['program'] = program
        before['terminals']['terminal']['program'] = 'old/cagents'
        after['terminals']['terminal']['program'] = program
        self.assertTrue(records.console_relaunch(before,after,'terminal'))
        changes = records.compare(before,after)
        self.assertFalse(any(row['regression'] for row in changes),changes)
        mutations = {
            'wrong-target':lambda a:a['target'].update(version='other'),
            'failed-update':lambda a:a['update_status'].update(phase='Failed'),
            'new-run':lambda a:a['agents']['agent'].update(run='new-run'),
            'interrupted':lambda a:a['agents']['agent'].update(state='Interrupted'),
            'new-recipe':lambda a:a['agents']['agent']['agent']['console']['launch'].update(environment={'X':'other'}),
            'new-pane':lambda a:a['stores']['windows/main/sessions.sqlite3']['rows']['terminal_recipes'][0].update(terminal_id='foreign'),
            'unverified':lambda a:a['recovery']['current']['outcome'].update(state='unverified')}
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                candidate = copy.deepcopy(after)
                mutate(candidate)
                self.assertFalse(records.console_relaunch(before,candidate,'terminal'))
                self.assertTrue(any(row['regression'] for row in records.compare(before,candidate)))

    def test_cancelled_run_requires_consent_and_the_exact_parent_interrupt_receipt(self):
        before, after = self.paused_replacement()
        after['target'] = {'working':'i','started':1.5}
        source = before['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content']
        source['launch']['provider'] = 'Codex'
        after['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content'] = copy.deepcopy(source)
        after['recovery']['census']['chats'] = [copy.deepcopy(source)]
        before['agents']['agent'].update(run='same-run',state='Running')
        before['agents']['agent']['agent']['session'] = {'Chat':source}
        after['agents']['agent'].update(run='same-run',state='Cancelled')
        identity = {'parent':True,'thread_id':'provider-issued-thread','turn_id':'parent-turn','socket':'/private/control.sock'}
        before['fixture_events'] = [{**identity,'event':'notification','method':'turn/started','time':1}]
        after['fixture_events'] = [{**identity,'event':'request','method':'turn/interrupt','time':2,'errors':[]},
                                   {**identity,'event':'notification','method':'turn/completed','status':'interrupted','time':3}]
        self.assertTrue(records.confirmed_interruption(before,after,'agent'))
        self.assertFalse(any(row['regression'] for row in records.compare(before,after)))
        for name, mutate in {
            'enter':lambda a:a['target'].update(working=''),
            'new-run':lambda a:a['agents']['agent'].update(run='other'),
            'no-receipt':lambda a:a['fixture_events'].pop(),
            'child-only':lambda a:a['fixture_events'][1].update(parent=False),
            'wrong-turn':lambda a:a['fixture_events'][1].update(turn_id='other'),
            'wrong-thread':lambda a:a['fixture_events'][1].update(thread_id='other'),
            'normal-completion':lambda a:a['fixture_events'][1].update(status='completed'),
            'no-request':lambda a:a['fixture_events'].pop(0),
            'request-error':lambda a:a['fixture_events'][0].update(errors=['refused']),
            'stale-receipt':lambda a:a['target'].update(started=4),
        }.items():
            with self.subTest(name=name):
                candidate = copy.deepcopy(after)
                mutate(candidate)
                self.assertFalse(records.confirmed_interruption(before,candidate,'agent'))
                self.assertTrue(any(row['regression'] for row in records.compare(before,candidate)))

    def test_recovery_capture_keeps_raw_digest_and_requires_fresh_verifier(self):
        before, after = self.paused_replacement()
        evidence = after["recovery"]
        with tempfile.TemporaryDirectory(prefix="rehearse-receipt-") as temporary:
            root = Path(temporary)
            state = root / "home/.local/share/cagents-install/.update"
            out = root / "records/after"
            state.mkdir(parents=True)
            out.mkdir(parents=True)
            job = evidence["census"]["transaction"]
            (state / "status.json").write_text(records.dump({"id": job, "version": "candidate"}))
            raw = records.dump(evidence["census"]).encode()
            (state / f"{job}.census.json").write_bytes(raw)
            (state / f"{job}.recovery.json").write_text(records.dump(evidence["issued"]))
            (state / f"{job}.census.json").chmod(0o600)
            (state / f"{job}.recovery.json").chmod(0o600)
            self.assertTrue(records.prepare_recovery(root, out))
            self.assertEqual((out / "recovery-census.json").read_bytes(), raw)
            self.assertEqual((out / "recovery-census.json").stat().st_mode & 0o777, 0o600)
            self.assertEqual((out / "recovery-issued.json").stat().st_mode & 0o777, 0o600)
            errors = []
            self.assertIsNone(records.recovery_evidence(out, errors))
            self.assertTrue(errors)
            (out / "recovery-current.json").write_text(records.dump(evidence["current"]))
            (out / "recovery-current.json").chmod(0o600)
            errors = []
            captured = records.recovery_evidence(out, errors)
            self.assertEqual(errors, [])
            self.assertEqual(captured, evidence)
            (state / "status.json").write_text(records.dump({"id": "../../foreign"}))
            with self.assertRaises(ValueError):
                records.prepare_recovery(root, out)
            (state / f"{job}.recovery.json").unlink()
            (state / "status.json").write_text(records.dump({"id": job, "phase": "Failed", "version": "candidate"}))
            self.assertFalse(records.prepare_recovery(root, out))
            (state / "status.json").write_text(records.dump({"id": job, "phase": "Complete", "version": "candidate"}))
            with self.assertRaises(ValueError):
                records.prepare_recovery(root, out)

    def test_recovery_evidence_rejects_public_oversized_and_linked_inputs(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-private-") as temporary:
            path = Path(temporary) / "receipt.json"
            path.write_text("{}")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                records.private_evidence(path)
            path.chmod(0o600)
            with path.open("r+b") as stream:
                stream.truncate(records.EVIDENCE_LIMIT + 1)
            with self.assertRaises(ValueError):
                records.private_evidence(path)
            alias = Path(temporary) / "alias.json"
            alias.symlink_to(path)
            with self.assertRaises(OSError):
                records.private_evidence(alias)

    def test_titles_that_follow_content_are_changes_and_blank_titles_are_losses(self):
        before, after = snapshot(), snapshot()
        after["windows"]["main"]["title"] = "~/s/alpha (2) \u2014 cagents"
        after["channels"]["channel"]["title"] = "Offline tool rows"
        after["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][0]["title"] = "bash \u00b7 alpha"
        changes = records.compare(before, after)
        self.assertEqual(len(changes), 3)
        self.assertFalse(any(row["regression"] for row in changes))
        after["channels"]["channel"]["title"] = ""
        self.assertTrue(any(row["regression"] and row["kind"] == "channels"
                            for row in records.compare(before, after)))

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
            with closing(sqlite3.connect(database)) as db, db:
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

    def test_process_executable_is_kernel_image_even_when_argv_claims_other_daemon(self):
        with tempfile.TemporaryDirectory(prefix="rehearse-exe-") as temporary:
            root = Path(temporary)
            sleep_image = str(Path(shutil.which("sleep")).resolve())
            peer = subprocess.Popen(["bash", "-c", 'exec -a /fake/old-version/bin/cagentsd sleep 60'],
                                    env={**os.environ, "HOME": str(root / "home")})
            try:
                deadline = time.monotonic() + 5
                while os.readlink(f"/proc/{peer.pid}/exe") != sleep_image:
                    self.assertIsNone(peer.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.01)
                row = next(row for row in records.processes(root) if row["pid"] == peer.pid)
                self.assertEqual(row["argv"][0], "/fake/old-version/bin/cagentsd")
                self.assertEqual(row["executable"], os.readlink(f"/proc/{peer.pid}/exe"))
                self.assertEqual(row["executable"], sleep_image)
                self.assertGreater(row["start_ticks"], 0)
            finally:
                peer.terminate()
                peer.wait(timeout=5)

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
            with closing(sqlite3.connect(path)) as db, db:
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


class ReopenedWindows(unittest.TestCase):
    def test_success_requires_the_new_ui_image_in_both_named_windows(self):
        spec = importlib.util.spec_from_file_location("rehearsal_run", REPOSITORY / "scripts/rehearse_update_run.py")
        scenario = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        rehearsal = scenario.Rehearsal('/var/tmp/rehearsal-test', str(SCRIPT))
        windows = {'main': {'pid': 1}, 'second': {'pid': 2}}
        expected = '/var/tmp/rehearsal-test/home/.local/share/cagents-install/versions/new/bin/cagents-ui'
        with patch.object(rehearsal, 'mapped', return_value=windows), \
                patch.object(scenario.os, 'readlink', side_effect=[expected, expected]):
            self.assertEqual(rehearsal.wait_windows(['main','second'], timeout=1, version='new'), windows)
        with patch.object(rehearsal, 'mapped', return_value=windows), \
                patch.object(scenario.os, 'readlink', side_effect=[expected, expected.replace('/new/', '/old/')]), \
                patch.object(scenario.time, 'monotonic', side_effect=[0, .1, 2]), \
                patch.object(scenario.time, 'sleep'):
            with self.assertRaisesRegex(RuntimeError, 'windows did not map on new'):
                rehearsal.wait_windows(['main','second'], timeout=1, version='new')


class ChatViews(unittest.TestCase):
    def module(self):
        spec = importlib.util.spec_from_file_location('rehearsal_chat_views', REPOSITORY/'scripts/rehearse_update_run.py')
        scenario = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        return scenario

    def test_channel_navigation_uses_sidebar_reference_not_header_or_tooltip(self):
        scenario = self.module()
        observation = {'runs':[
            {'ref':'header','text':'Codex rehearsal','x':269,'y':71},
            {'ref':'row','text':'© Codex rehearsal','x':12,'y':134},
            {'ref':'tooltip','text':'Codex rehearsal - 1 chat','x':40,'y':168}]}
        self.assertEqual(scenario.sidebar_channel_ref(observation,'Codex rehearsal'),'row')
        observation['runs'].append({'ref':'other','text':'Codex rehearsal','x':33,'y':200})
        with self.assertRaisesRegex(RuntimeError,'no unique sidebar reference'):
            scenario.sidebar_channel_ref(observation,'Codex rehearsal')

    def test_paused_capture_requires_connected_history_and_only_clicks_reconnect(self):
        scenario = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-history-') as directory:
            root = Path(directory)
            (root/'logs').mkdir()
            rehearsal = scenario.Rehearsal(root,str(SCRIPT))
            window = {'id':42,'focused':True}
            disconnected = {'window':window,'painted':True,'runs':[{'text':'Reconnect','ref':'reconnect','x':400}]}
            connected = {'window':window,'painted':True,'runs':[
                {'text':'Paused after an incomplete update','x':400},
                {'text':'Resume conversation','ref':'resume','x':400}]}
            replies = iter([disconnected,connected])
            calls = []
            def call(*args,**kwargs):
                calls.append(args)
                return 0,json.dumps(next(replies)) if args[:2]==('drive','--json') else ''
            with patch.object(rehearsal,'call',side_effect=call),patch.object(scenario.time,'sleep'):
                rehearsal.observe_paused(window,'after-codex-main')
            self.assertIn(('drive','click','--stale-ok','reconnect'),calls)
            self.assertFalse(any('resume' in args for args in calls))
            self.assertTrue(json.loads((root/'logs/after-codex-main-history.json').read_text())['reconnected'])
            incomplete = {**connected,'runs':[{'text':'Transcript preview could not attach','x':400}]}
            with patch.object(rehearsal,'call',return_value=(0,json.dumps(incomplete))), \
                    patch.object(scenario.time,'monotonic',side_effect=[0,1,31]), \
                    patch.object(scenario.time,'sleep'):
                with self.assertRaisesRegex(RuntimeError,'did not show'):
                    rehearsal.observe_paused(window,'failed-codex-main')
            self.assertTrue(scenario.visible_phrase('Resume Resu conversation','Resume conversation'))

    def test_expected_refusal_requires_bound_old_processes_install_and_inventory(self):
        scenario = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-refusal-') as directory:
            root = Path(directory)
            prefix = root/'home/.local/share/cagents-install'
            (prefix/'versions/old').mkdir(parents=True)
            (prefix/'current').symlink_to('versions/old')
            (prefix/'.update').mkdir()
            before = snapshot()
            proof = {'host_session_id':'old','boot_id':'boot','host_incarnation':'old-owner'}
            before['serving'] = proof
            before['processes'] += [
                {'pid':50,'start_ticks':2,'state':'S','executable':str(prefix/'versions/old/bin/cagentsd'),'argv':['cagentsd']},
                {'pid':51,'start_ticks':3,'state':'S','argv':['/private/cagents-ui']},
                {'pid':52,'start_ticks':4,'state':'S','argv':['/private/cagents-ui']}]
            after = copy.deepcopy(before)
            after['target'] = {'version':'new','started':10,'finished':40.5}
            after['fixture_events'] = [{'event':'request','method':'turn/interrupt','parent':True,'errors':[],'time':10.1}]
            status = {'version':'new','phase':'Failed','message':'did not stop in time; nothing was changed'}
            def write(after_value,status_value):
                for side,value in [('before',before),('after',after_value)]:
                    path = root/'records'/side
                    path.mkdir(parents=True,exist_ok=True)
                    (path/'snapshot.json').write_text(json.dumps(value))
                    (path/'terminals.json').write_text(json.dumps({'data':{'proof':value['serving']}}))
                (prefix/'.update/status.json').write_text(json.dumps(status_value))
            write(after,status)
            self.assertEqual(scenario.expected_refusal(root,'interrupt','old'),[])
            for name,mutate in {
                'late':lambda a:a['target'].update(finished=90),
                'new-daemon':lambda a:a['processes'][1].update(pid=60),
                'new-run':lambda a:a['agents']['agent'].update(run='new-run'),
                'missing-pane':lambda a:a['stores']['windows/main/sessions.sqlite3']['rows']['cards'].clear(),
                'unexpected-receipt':lambda a:a['fixture_events'].append({'event':'notification','method':'turn/completed','status':'interrupted','time':20}),
            }.items():
                with self.subTest(name=name):
                    candidate = copy.deepcopy(after)
                    mutate(candidate)
                    write(candidate,status)
                    self.assertTrue(scenario.expected_refusal(root,'interrupt','old'))
            cancelled = copy.deepcopy(after)
            cancelled['fixture_events'] = []
            write(cancelled,{**status,'phase':'Cancelled','message':'Update cancelled'})
            self.assertEqual(scenario.expected_refusal(root,'cancel','old'),[])
            write(after,{**status,'phase':'Cancelled'})
            self.assertTrue(scenario.expected_refusal(root,'cancel','old'))

    def test_selection_requires_exact_card_channel_provider_and_thread(self):
        scenario = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-selection-') as temporary:
            root = Path(temporary)
            path = root/'home/.local/share/cagents/sessions.sqlite3'
            path.parent.mkdir(parents=True)
            descriptor = {'id':'session','proof':{'host_session_id':'source','boot_id':'boot','host_incarnation':'first'},
                          'launch':{'provider':'Claude','session_id':'issued-thread'}}
            with closing(sqlite3.connect(path)) as db, db:
                db.executescript('CREATE TABLE workspaces(workspace_id TEXT,focused_card_id TEXT);'
                                 'CREATE TABLE cards(card_id TEXT,workspace_id TEXT,kind_tag TEXT,content_json TEXT,existence_json TEXT);'
                                 'CREATE TABLE metadata(key TEXT,value TEXT);')
                db.execute('INSERT INTO workspaces VALUES (?,?)', ['workspace','exact-card'])
                db.execute('INSERT INTO cards VALUES (?,?,?,?,?)',
                           ['exact-card','workspace','chat',json.dumps(descriptor),'"open"'])
                db.execute('INSERT INTO metadata VALUES (?,?)',
                           ['workspace:workspace:navigation:active_thread','"exact-channel"'])
                db.commit()
                original = path.read_bytes()
                selected = scenario.selected_chat(root,'main','exact-card','exact-channel',descriptor)
                self.assertEqual(selected['session'], descriptor)
                self.assertEqual(path.read_bytes(), original)
                for expected in ({**descriptor,'id':'foreign'},
                                 {**descriptor,'launch':{'provider':'Codex','session_id':'issued-thread'}},
                                 {**descriptor,'launch':{'provider':'Claude','session_id':'foreign-thread'}},
                                 {**descriptor,'launch':{**descriptor['launch'],'recipe':{'program':'foreign'}}},
                                 {**descriptor,'proof':{**descriptor['proof'],'host_incarnation':'foreign'}}):
                    self.assertIsNone(scenario.selected_chat(root,'main','exact-card','exact-channel',expected))
                # Frozen owners can publish a null provider thread after their
                # exact issued UI projection learns it. Bind that first view,
                # then require its actual provider thread on the second view.
                unpromoted = {**descriptor,'launch':{'provider':'Claude','session_id':None}}
                self.assertIsNotNone(scenario.selected_chat(root,'main','exact-card','exact-channel',unpromoted))
                self.assertIsNotNone(scenario.selected_chat(root,'main','exact-card','exact-channel',unpromoted,'issued-thread'))
                self.assertIsNone(scenario.selected_chat(root,'main','exact-card','exact-channel',unpromoted,'foreign-thread'))
                self.assertIsNone(scenario.selected_chat(root,'main','foreign-card','exact-channel',descriptor))
                self.assertIsNone(scenario.selected_chat(root,'main','exact-card','foreign-channel',descriptor))
                db.execute('UPDATE workspaces SET focused_card_id=?',['foreign-card'])
                db.commit()
                self.assertIsNone(scenario.selected_chat(root,'main','exact-card','exact-channel',descriptor))

    def test_capture_uses_seed_ids_and_labels_only_to_navigate(self):
        scenario = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-chat-view-') as temporary:
            root = Path(temporary)
            seed = {'id':'seed-agent','name':'old-name','placement':{'Channel':'seed-channel'},
                    'spec':{'provider':'Claude','target':{'repository':'/private/alpha/.git'}}}
            chat = {'id':'seed-session','proof':{'host_session_id':'source','boot_id':'boot','host_incarnation':'first'},
                    'launch':{'provider':'Claude','session_id':'issued-thread'}}
            current = {**seed,'name':'changed-name','session':{'Chat':chat}}
            (root/'seeds.json').write_text(json.dumps({'items':[
                {'state':'named-chat','response':{'data':{'agent':seed}}}]}))
            rehearsal = scenario.Rehearsal(root,str(SCRIPT))
            calls = []
            def cli(*args):
                calls.append(args)
                if args[1:]==('agent','list'):
                    foreign = {**current,'id':'foreign-agent','name':'old-name'}
                    return 0, {'data':{'agents':[{'agent':foreign},{'agent':current}]}}
                if args[1:]==('channel','list'):
                    return 0, {'data':{'channels':[{'id':'seed-channel','title':'Visible canonical channel'}]}}
                self.assertEqual(args,('--no-start','agent','status','seed-agent'))
                return 0, {'data':{'agent':current}}
            window = {'id':42}
            def call(*args,**kwargs):
                calls.append(args)
                return 0, json.dumps({'window':window,'painted':True,'runs':[
                    {'ref':'sidebar-row','text':'Visible canonical channel','x':33,'y':134}]})
            with patch.object(rehearsal,'cli',side_effect=cli), \
                    patch.object(rehearsal,'call',side_effect=call), \
                    patch.object(rehearsal,'mapped',return_value={'main':window,'second':window}), \
                    patch.object(scenario,'selected_chat',return_value={'session':chat}) as selected:
                evidence = rehearsal.capture_chats('before')
            self.assertEqual(len(evidence),2)
            self.assertTrue(all(item['agent']=='seed-agent' for item in evidence))
            for selection_call in selected.call_args_list:
                self.assertEqual(selection_call.args[2:4],('seed-agent','seed-channel'))
            self.assertIn(('drive','click','--stale-ok','sidebar-row'),calls)
            self.assertFalse(any('spawn' in call for call in calls))
            chat['launch']['session_id'] = None
            def conflicting_view(root,slot,*args):
                return {'session':{**chat,'launch':{**chat['launch'],'session_id':f'thread-{slot}'}}}
            with patch.object(rehearsal,'cli',side_effect=cli), \
                    patch.object(rehearsal,'call',side_effect=call), \
                    patch.object(rehearsal,'mapped',return_value={'main':window,'second':window}), \
                    patch.object(scenario,'selected_chat',side_effect=conflicting_view):
                with self.assertRaisesRegex(RuntimeError,'two windows disagree'):
                    rehearsal.capture_chats('before')


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

    def test_answers_the_working_choice_and_the_stop_list_separately(self):
        result = self.run_peer(
            "import sys; "
            "tty=open('/dev/tty','r'); "
            "sys.stdout.write('Update now? [Y/n] '); sys.stdout.flush(); "
            "assert tty.readline().strip() == 'yes'; "
            "sys.stdout.write('Enter: update when they finish   i: interrupt them now   n: cancel\\n'); sys.stdout.flush(); "
            "assert tty.readline() == '\\n'; "
            "sys.stdout.write('Type \"stop\" to stop these and update. Anything else cancels.\\n'); sys.stdout.flush(); "
            "assert tty.readline().strip() == 'stop'; "
            "print('all three answered')", "--answer", "yes", "--stop", "stop")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("[rehearsal prompt="), 3)
        self.assertIn("all three answered", result.stdout)

    def test_stop_list_is_cancelled_unless_asked(self):
        result = self.run_peer(
            "import sys; tty=open('/dev/tty','r'); "
            "sys.stdout.write('Type \"stop\" to stop these and update. Anything else cancels.\\n'); sys.stdout.flush(); "
            "raise SystemExit(0 if tty.readline() == '\\n' else 3)", "--answer", "yes")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_child_exit_status_is_preserved(self):
        result = self.run_peer("raise SystemExit(7)")
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)

    def test_timeout_allows_peer_to_finish_sigterm_handler_before_pty_closes(self):
        with tempfile.TemporaryDirectory(prefix="ru-pty-") as temporary:
            marker = Path(temporary) / "terminated"
            result = self.run_peer(
                "import signal,time,pathlib; "
                f"signal.signal(signal.SIGTERM, lambda *_: (time.sleep(.1), pathlib.Path({str(marker)!r}).write_text('done'), exit(0))); "
                "print('ready',flush=True); time.sleep(30)", "--timeout", ".3")
            self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
            self.assertEqual(marker.read_text(), "done")

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

    def test_help_is_available_with_inherited_authority(self):
        for arguments in (["-h"], ["--help"], ["--root", "/var/tmp/unused", "--help"],
                          ["--root", "/var/tmp/unused", "run", "--help"]):
            result = subprocess.run([str(SCRIPT), *arguments],
                                    env={**os.environ, AUTHORITY[0]: "fixture-authority"},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("usage:", result.stdout.lower())

    @unittest.skipIf(any(name in os.environ for name in AUTHORITY),
                     "stop is tested in a genuine user shell; never strip authority")
    def test_stop_restores_only_its_private_preservation_fault(self):
        for fault in ("marker", "interrupted-rename", "unrelated-file", "unrelated-symlink"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(prefix="ru-stop-", dir="/var/tmp") as temporary:
                root = Path(temporary)
                (root / ".rehearse-update").write_text(str(root)+"\n")
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
                          ["--root", "/var/tmp/nonexistent-rehearsal", "unknown"],
                          ["--root", "/home/rehearsal", "init"],
                          ["--root", "/dev/shm", "init"]):
            with self.subTest(arguments=arguments):
                result = subprocess.run([str(SCRIPT), *arguments], capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)


@unittest.skipIf(any(name in os.environ for name in AUTHORITY),
                 "harness actions require a genuine user shell; never strip authority")
class PrivateRoot(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ru-contract-", dir="/var/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def call(self, *args, **kwargs):
        return subprocess.run([str(SCRIPT), "--root", str(self.root), *args],
                              capture_output=True, text=True, timeout=20, **kwargs)

    def test_init_records_root_and_cleanup_refuses_mismatched_or_symlink_marker(self):
        result = self.call("init")
        self.assertEqual(result.returncode, 0, result.stderr)
        marker = self.root / ".rehearse-update"
        self.assertEqual(marker.read_text(), str(self.root)+"\n")
        for kind in ("mismatch", "symlink"):
            with self.subTest(kind=kind):
                marker.unlink()
                if kind == "mismatch":
                    marker.write_text("/var/tmp/somewhere-else\n")
                else:
                    target = self.root / "other-marker"
                    target.write_text(str(self.root)+"\n")
                    marker.symlink_to(target)
                result = self.call("cleanup")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("matching root marker", result.stderr)
                self.assertTrue(self.root.exists())

    def test_update_refuses_existing_noncurrent_version(self):
        self.assertEqual(self.call("init").returncode, 0)
        prefix = self.root / "home/.local/share/cagents-install"
        for version in ("old", "new"):
            (prefix / "versions" / version).mkdir(parents=True)
        (prefix / "current").symlink_to("versions/old")
        result = self.call("update", "/missing", "new", "/missing/install.sh")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exists but is not current; remove it to reinstall", result.stderr)
        self.assertEqual((prefix / "current").readlink(), Path("versions/old"))
        (prefix / "current").unlink()
        (prefix / "current").symlink_to("versions/new")
        result = self.call("update", "/missing", "new", "/missing/install.sh")
        self.assertIn("already current; skipping staging", result.stdout)
        self.assertNotIn("exists but is not current", result.stderr)

    def test_desktop_waiting_for_lock_exits_on_stop_without_starting_compositor(self):
        self.assertEqual(self.call("init").returncode, 0)
        desktop = self.root / "desktop"
        desktop.mkdir()
        with (self.root / "native.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with (self.root / "waiter.log").open("w") as output:
                peer = subprocess.Popen([str(SCRIPT), "--root", str(self.root), "__desktop", "60"],
                                        env={**os.environ, "CAGENTS_EVIDENCE_NATIVE_LOCK": str(self.root / "native.lock")},
                                        stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    deadline = time.monotonic()+5
                    while "waiting for the native lock" not in (self.root / "waiter.log").read_text():
                        self.assertIsNone(peer.poll())
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(.05)
                    (desktop / "stop").touch()
                    self.assertEqual(peer.wait(timeout=5), 0)
                    self.assertFalse((self.root / "logs/kwin.log").exists())
                finally:
                    if peer.poll() is None:
                        os.killpg(peer.pid, signal.SIGKILL)
                        peer.wait()

    def test_stop_reports_surviving_attributed_scope_without_stopping_other_members(self):
        self.assertEqual(self.call("init").returncode, 0)
        unit = self.root.name+".scope"
        # The wrapper and second child retain the real environment. Only the
        # first child's HOME attributes this scope to the private root.
        body = ('env HOME="$1/home" sleep 60 & echo $! >"$1/private.pid"; '
                'sleep 60 & echo $! >"$1/unrelated.pid"; wait')
        peer = subprocess.Popen(["systemd-run", "--user", "--scope", "--unit="+unit,
                                 "bash", "-c", body, "bash", str(self.root)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                start_new_session=True)
        try:
            deadline = time.monotonic()+5
            while not (self.root / "unrelated.pid").exists():
                if peer.poll() is not None:
                    self.fail("scope fixture failed: "+peer.communicate()[1])
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.05)
            unrelated = int((self.root / "unrelated.pid").read_text())
            result = self.call("stop")
            self.assertNotEqual(result.returncode, 0, result.stdout+result.stderr)
            self.assertIn("attributed scopes still running:\n"+unit, result.stderr)
            os.kill(unrelated, 0)
        finally:
            # This test created the whole group, unlike stop's attributed PID
            # cleanup. End it here even when an assertion failed.
            try:
                os.killpg(peer.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            peer.communicate(timeout=5)

    def test_drive_and_display_query_only_receive_allowlisted_environment(self):
        self.assertEqual(self.call("init").returncode, 0)
        desktop = self.root / "desktop"
        (desktop / "home").mkdir(parents=True)
        (desktop / "pid").write_text(str(os.getpid()))
        (desktop / "ready").touch()
        (desktop / "env").write_text("WAYLAND_DISPLAY=fixture-wayland\n"
                                     "DBUS_SESSION_BUS_ADDRESS=unix:path=/fixture\n"
                                     f"XDG_RUNTIME_DIR={desktop}/rt\n"
                                     "PRIVATE_SECRET=must-not-pass\n")
        tools = self.root / "caller-tools"
        tools.mkdir()
        harness = tools / "niri-harness"
        harness.write_text("#!/usr/bin/python3\nimport json,os,sys,pathlib\n"
                           "value=json.dumps(dict(os.environ))\n"
                           "if sys.argv[-2:]==['session','env']:\n"
                           " pathlib.Path(os.environ['HOME'],'seen.json').write_text(value)\n"
                           " print(\"export WAYLAND_DISPLAY='fixture-wayland'\")\n"
                           "else: print(value)\n")
        harness.chmod(0o755)
        caller = {**os.environ, "PATH": str(tools)+":"+os.environ["PATH"],
                  "PRIVATE_SECRET": "caller-secret", "CODEX_HOME": "/real-provider"}
        result = self.call("drive", "observe", env=caller)
        self.assertEqual(result.returncode, 0, result.stderr)
        environments = [json.loads(result.stdout)]
        result = self.call("app", "/usr/bin/true", env=caller)
        self.assertEqual(result.returncode, 0, result.stderr)
        environments.append(json.loads((desktop / "home/seen.json").read_text()))
        for environment in environments:
            self.assertEqual(set(environment) - {"LC_CTYPE"},
                             {"HOME", "PATH", "LANG", "TERM", "WAYLAND_DISPLAY",
                              "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"})
            self.assertEqual(environment["HOME"], str(desktop / "home"))
            self.assertEqual(environment["PATH"], str(desktop / "tools")+":/usr/local/bin:/usr/bin:/bin")


class RefusalIdentity(unittest.TestCase):
    def test_same_versions_cannot_hide_replaced_daemon_or_windows(self):
        spec = importlib.util.spec_from_file_location("rehearsal_run", REPOSITORY / "scripts/rehearse_update_run.py")
        scenario = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        with tempfile.TemporaryDirectory(prefix="ru-refusal-") as temporary:
            records = Path(temporary)
            proof = {'host_session_id':'original','host_incarnation':'first','boot_id':'boot'}
            processes = [{'pid':42,'start_ticks':100,'argv':['/private/bin/cagents-ui']},
                         {'pid':43,'start_ticks':101,'argv':['/private/bin/cagents-ui','--window','second']}]
            for name in ('before','after'):
                folder = records/name
                folder.mkdir()
                (folder/'terminals.json').write_text(json.dumps({'data':{'proof':proof}}))
                (folder/'snapshot.json').write_text(json.dumps({'processes':processes}))
            self.assertEqual(scenario.refusal_preserved_processes(records), [])
            (records/'after/terminals.json').write_text(json.dumps({'data':{'proof':dict(proof,host_incarnation='second')}}))
            processes[0]['start_ticks'] = 999
            (records/'after/snapshot.json').write_text(json.dumps({'processes':processes}))
            errors = scenario.refusal_preserved_processes(records)
            self.assertEqual(len(errors), 2)
            self.assertIn('original daemon', errors[0])
            self.assertIn('original windows', errors[1])


class StartupFault(unittest.TestCase):
    def module(self):
        spec = importlib.util.spec_from_file_location('rehearsal_fault', REPOSITORY/'scripts/rehearse_update_fault.py')
        fault = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fault)
        return fault

    def test_fault_archive_is_verified_before_staging_without_changing_shared_build(self):
        fault = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-fault-archive-') as temporary:
            base = Path(temporary)
            root, shared = base/'private', base/'shared'
            root.mkdir()
            shared.mkdir()
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'logs').mkdir()
            candidate = base/'tree/cagents-new'
            (candidate/'bin').mkdir(parents=True)
            daemon = candidate/'bin/cagentsd'
            daemon.write_text('#!/bin/sh\nprintf "maintenance %s\\n" "$*"\n')
            daemon.chmod(0o755)
            cli = candidate/'bin/cagents'
            cli.write_text('#!/bin/sh\nprintf "cagents new\\n"\n')
            cli.chmod(0o755)
            name = 'cagents-new-x86_64-unknown-linux-gnu.tar.gz'
            archive = shared/name
            with tarfile.open(archive,'w:gz') as output:
                output.add(candidate,arcname='cagents-new')
            original = archive.read_bytes()
            digest = hashlib.sha256(original).hexdigest()
            (shared/'SHA256SUMS').write_text(f'{digest}  {name}\n')
            destination = fault.failing_archive(root,'new',shared)
            self.assertEqual(archive.read_bytes(),original)
            self.assertEqual((shared/'SHA256SUMS').read_text(),f'{digest}  {name}\n')
            actual = hashlib.sha256((destination/name).read_bytes()).hexdigest()
            self.assertEqual((destination/'SHA256SUMS').read_text(),f'{actual}  {name}\n')
            self.assertNotEqual(actual,digest)
            extracted = root/'inspect'
            extracted.mkdir()
            with tarfile.open(destination/name,'r:gz') as output:
                with tarfile.open(archive,'r:gz') as original_archive:
                    self.assertEqual(set(output.getnames()),set(original_archive.getnames()))
                output.extractall(extracted,filter='data')
            wrapped = extracted/'cagents-new/bin/cagentsd'
            result = subprocess.run([wrapped],capture_output=True,text=True)
            self.assertEqual(result.returncode,1)
            self.assertIn('forced rehearsal daemon startup failure',result.stderr)
            self.assertEqual((root/'logs/daemon-start-failure.txt').read_text().split()[0],'new')
            result = subprocess.run([wrapped,'--stored-schema'],capture_output=True,text=True)
            self.assertEqual((result.returncode,result.stdout),(0,'maintenance --stored-schema\n'))
            result = subprocess.run([wrapped,'--ceditor-notifications'],capture_output=True,text=True)
            self.assertEqual(result.returncode,1)
            with self.assertRaises(FileExistsError):
                fault.failing_archive(root,'new',shared)

    def test_fault_archive_refuses_unverified_or_escaping_members(self):
        fault = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-fault-escape-') as temporary:
            base = Path(temporary)
            root, shared = base/'private', base/'shared'
            root.mkdir()
            shared.mkdir()
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'logs').mkdir()
            name = 'cagents-new-x86_64-unknown-linux-gnu.tar.gz'
            archive = shared/name
            with tarfile.open(archive,'w:gz') as output:
                entry = tarfile.TarInfo('cagents-new/../../escaped')
                entry.size = 3
                output.addfile(entry,io.BytesIO(b'bad'))
            (shared/'SHA256SUMS').write_text(f'{"0"*64}  {name}\n')
            with self.assertRaisesRegex(ValueError,'checksum-verified'):
                fault.failing_archive(root,'new',shared)
            self.assertFalse((root/'startup-fault-archive').exists())
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (shared/'SHA256SUMS').write_text(f'{digest}  {name}\n')
            with self.assertRaises(tarfile.TarError):
                fault.failing_archive(root,'new',shared)
            self.assertFalse((root/'escaped').exists())

    def test_fault_only_changes_marked_private_candidate_and_preserves_maintenance(self):
        spec = importlib.util.spec_from_file_location("rehearsal_fault", REPOSITORY / "scripts/rehearse_update_fault.py")
        fault = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fault)
        with tempfile.TemporaryDirectory(prefix="ru-fault-") as temporary:
            root = Path(temporary)
            prefix = root / 'home/.local/share/cagents-install'
            candidate = prefix / 'versions/new'
            (candidate / 'bin').mkdir(parents=True)
            (root / 'logs').mkdir()
            (prefix / 'current').symlink_to('versions/new')
            daemon = candidate / 'bin/cagentsd'
            daemon.write_text('#!/bin/sh\nprintf "maintenance %s\\n" "$*"\n')
            daemon.chmod(0o755)
            with self.assertRaises(FileNotFoundError):
                fault.fail_start(root, 'new')
            (root / '.rehearse-update').write_text(str(root))
            with self.assertRaises(ValueError):
                fault.fail_start(root, '../../outside')
            fault.fail_start(root, 'new')
            result = subprocess.run([daemon], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn('forced rehearsal daemon startup failure', result.stderr)
            receipt = (root / 'logs/daemon-start-failure.txt').read_text().split()
            self.assertEqual(receipt[0], 'new')
            self.assertGreater(int(receipt[1]), 1)
            result = subprocess.run([daemon, '--stored-schema'], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(result.stdout, 'maintenance --stored-schema\n')
            self.assertTrue((root / 'logs/startup-fault.txt').exists())
            with self.assertRaises(ValueError):
                fault.fail_start(root, 'new')


class FailedInitialization(unittest.TestCase):
    def test_partial_init_still_records_failure_and_stops(self):
        spec = importlib.util.spec_from_file_location("rehearsal_run", REPOSITORY / "scripts/rehearse_update_run.py")
        scenario = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        with tempfile.TemporaryDirectory(prefix="ru-run-") as temporary:
            root = Path(temporary) / "fresh"
            rehearsal = scenario.Rehearsal(root, str(SCRIPT))
            commands = []
            def call(command, *args, **kwargs):
                commands.append(command)
                if command == "init":
                    root.mkdir()
                    raise RuntimeError("partly failed init")
                return 1, "not initialized"
            args = type("Args", (), dict(old_version="old", new_version="new", ceditor=None))()
            with patch.object(rehearsal, "call", side_effect=call):
                self.assertEqual(rehearsal.run(args), 1)
            self.assertEqual(commands, ["init", "record", "stop"])
            self.assertIn("Outcome: FAIL", (root / "report.txt").read_text())
            self.assertIn("partly failed init", (root / "report.txt").read_text())
            with patch.object(rehearsal, "call") as call:
                with self.assertRaisesRegex(RuntimeError, "fresh, absent root"):
                    rehearsal.run(args)
                call.assert_not_called()


if __name__ == "__main__":
    unittest.main()
