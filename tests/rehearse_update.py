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
import select
import struct
import termios
import shutil
import unittest
from unittest.mock import patch

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

    def test_verified_pause_does_not_accept_foreign_history_in_original_pane(self):
        before, after = self.paused_replacement()
        after["stores"]["windows/main/sessions.sqlite3"]["rows"]["cards"][1]["content"]["launch"]["session_id"] = "foreign-thread"
        self.assertTrue(any(row["regression"] for row in records.compare(before, after)))

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
                return 0, json.dumps({'window':window,'runs':[
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


class CandidateNamespace(unittest.TestCase):
    def module(self):
        spec=importlib.util.spec_from_file_location('candidate_namespace_rehearsal',REPOSITORY/'scripts/rehearse_update_run.py')
        scenario=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        return scenario

    def evidence(self):
        source={'ok':True,'data':{'instance':'production','running':True,'daemon_version':'old',
                                  'socket':'/var/tmp/private/rt/cagents/cagentsd.sock','cli_version':'old'}}
        candidate=copy.deepcopy(source)
        candidate['data']['cli_version']='new'
        proof={'host_session_id':'source','boot_id':'boot','host_incarnation':'original'}
        return {'source':source,'candidate_status':{'exit':0,'payload':candidate},'candidate_version':'cagents new\n',
                'source_snapshot':{'root':'/var/tmp/private','errors':[],'serving':proof,'processes':[
                    {'pid':100,'start_ticks':200,'executable':'/var/tmp/private/home/.local/share/cagents-install/versions/old/bin/cagentsd',
                     'argv':['/var/tmp/private/home/.local/share/cagents-install/versions/old/bin/cagentsd']}]},
                'census':{'transaction':'transaction','source':proof},'census_digest':'digest',
                'receipt':{'transaction':'transaction','census_digest':'digest','outcome':{'state':'usable','serving':proof}}}

    def test_candidate_must_observe_the_exact_serving_production_namespace(self):
        scenario=self.module()
        evidence=self.evidence()
        scenario.candidate_namespace_contract(evidence,'old','new')
        for field,value in (('instance','development'),('running',False),('daemon_version','new'),
                            ('socket','/var/tmp/private/rt/cagents-development/cagentsd.sock'),('cli_version','wrong')):
            with self.subTest(field=field):
                changed=copy.deepcopy(evidence)
                changed['candidate_status']['payload']['data'][field]=value
                with self.assertRaisesRegex(RuntimeError,'disagrees'):
                    scenario.candidate_namespace_contract(changed,'old','new')

    def test_protocol_rejected_status_requires_exact_ready_source_and_kernel_owner(self):
        scenario=self.module()
        evidence=self.evidence()
        evidence['candidate_status']={'exit':1,'payload':{'ok':False,'error':{'kind':'protocol','message':'unsupported source protocol'}}}
        scenario.candidate_namespace_contract(evidence,'old','new')
        cases=[]
        for value in (None,{},dict(evidence['census']['source'],host_incarnation='foreign')):
            changed=copy.deepcopy(evidence);changed['census']['source']=value;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['receipt']['outcome']={'state':'no_source'};cases.append(changed)
        changed=copy.deepcopy(evidence);changed['candidate_version']='cagents wrong';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['source_snapshot']['processes'][0]['executable']='/usr/bin/foreign';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['source_snapshot']['processes'].append(
            {'pid':101,'start_ticks':201,'executable':'/other/bin/cagentsd','argv':['/other/bin/cagentsd']});cases.append(changed)
        changed=copy.deepcopy(evidence);changed['source']['data']['instance']='development';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['receipt']['census_digest']='foreign';cases.append(changed)
        for changed in cases:
            with self.subTest(evidence=changed):
                with self.assertRaisesRegex(RuntimeError,'same namespace'):
                    scenario.candidate_namespace_contract(changed,'old','new')

    def test_probe_extracts_only_a_verified_regular_cli_inside_the_private_root(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-namespace-') as temporary:
            base=Path(temporary)
            root,shared=base/'private',base/'shared'
            root.mkdir();shared.mkdir()
            (root/'.rehearse-update').write_text(str(root)+'\n')
            archive=shared/'cagents-new-x86_64-unknown-linux-gnu.tar.gz'
            payload=b'#!/bin/sh\nexit 0\n'
            with tarfile.open(archive,'w:gz') as output:
                info=tarfile.TarInfo('cagents-new/bin/cagents');info.size=len(payload)
                output.addfile(info,io.BytesIO(payload))
                other=tarfile.TarInfo('cagents-new/bin/cagentsd');other.size=100
                output.addfile(other,io.BytesIO(b'x'*100))
            digest=hashlib.sha256(archive.read_bytes()).hexdigest()
            (shared/'SHA256SUMS').write_text(f'{digest}  {archive.name}\n')
            cli=scenario.candidate_namespace_cli(root,shared,'new')
            self.assertEqual(cli.read_bytes(),payload)
            self.assertEqual(cli.stat().st_mode&0o777,0o700)
            self.assertEqual({p.name for p in cli.parent.iterdir()},{'cagents','archive.json'})
            manifest=json.loads((cli.parent/'archive.json').read_text())
            self.assertEqual(manifest['archive_sha256'],digest)
            self.assertEqual(manifest['cli_sha256'],hashlib.sha256(payload).hexdigest())
            self.assertEqual((cli.parent/'archive.json').stat().st_mode&0o777,0o600)
            (shared/'SHA256SUMS').write_text(f'{"0"*64}  {archive.name}\n')
            with self.assertRaisesRegex(RuntimeError,'checksum-verified'):
                scenario.candidate_namespace_cli(root,shared,'new')

    def test_probe_refuses_a_linked_archive_cli(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-namespace-link-') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            archive=root/'cagents-new-x86_64-unknown-linux-gnu.tar.gz'
            with tarfile.open(archive,'w:gz') as output:
                info=tarfile.TarInfo('cagents-new/bin/cagents')
                info.type=tarfile.SYMTYPE;info.linkname='/usr/bin/false'
                output.addfile(info)
            digest=hashlib.sha256(archive.read_bytes()).hexdigest()
            (root/'SHA256SUMS').write_text(f'{digest}  {archive.name}\n')
            with self.assertRaisesRegex(RuntimeError,'regular CLI'):
                scenario.candidate_namespace_cli(root,root,'new')

    def test_headless_namespace_capture_does_not_claim_full_window_or_store_evidence(self):
        with tempfile.TemporaryDirectory(prefix='ru-namespace-headless-') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            proof={'host_session_id':'source','boot_id':'boot','host_incarnation':'original'}
            terminals=root/'baseline-terminals.json'
            terminals.write_text(json.dumps({'ok':True,'data':{'proof':proof,'terminals':[]}}))
            terminals.chmod(0o600)
            owner={'pid':100,'start_ticks':200,'executable':str(root/'old/bin/cagentsd')}
            with patch.object(records,'processes',return_value=[owner]):
                captured=records.namespace_source(root,terminals)
                self.assertEqual(captured,{'root':str(root),'serving':proof,'processes':[owner],'errors':[]})
                out=root/'full-record';out.mkdir()
                for collection in records.COLLECTIONS:
                    (out/f'{collection}.json').write_text(json.dumps({'ok':True,'data':{collection:[],'proof':proof}}))
                full=records.record(root,out,None)
                self.assertIn('window observations unavailable',full['errors'])
                self.assertTrue(any('store' in error or 'sessions.sqlite3' in error for error in full['errors']))
            terminals.write_text(json.dumps({'ok':True,'data':{'proof':None,'terminals':[]}}))
            with self.assertRaisesRegex(ValueError,'socket proof'):
                records.namespace_source(root,terminals)


class NativeLockWaiter(unittest.TestCase):
    def waiting_child(self, peer):
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            self.assertIsNone(peer.poll())
            try:
                children=Path(f'/proc/{peer.pid}/task/{peer.pid}/children').read_text().split()
                for name in children:
                    p=Path('/proc')/name
                    args=p.joinpath('cmdline').read_bytes().split(b'\0')
                    if args and Path(args[0].decode()).name=='flock':
                        stat=p.joinpath('stat').read_text().rsplit(')',1)[1].split()
                        return int(name),int(stat[19])
            except OSError:
                pass
            time.sleep(.02)
        self.fail('the single blocking native-lock waiter did not enter flock')

    def assert_child_gone(self, identity):
        pid,start=identity
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            try:
                stat=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
                if int(stat[19])!=start:
                    return
            except OSError:
                return
            time.sleep(.02)
        self.fail(f'owned lock waiter {pid}/{start} survived cancellation')

    def exercise(self, mode):
        with tempfile.TemporaryDirectory(prefix='ru-native-lock-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'desktop').mkdir()
            path=root/'native.lock'
            with path.open('w') as holder,path.open('w') as inherited,path.open('w') as probe:
                fcntl.flock(holder,fcntl.LOCK_EX)
                command=[sys.executable,str(REPOSITORY/'scripts/rehearse_update_run.py'),str(root),str(SCRIPT),
                         'wait-native-lock','--fd',str(inherited.fileno()),'--parent-pid',str(os.getpid())]
                peer=subprocess.Popen(command,pass_fds=(inherited.fileno(),),stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE,text=True,start_new_session=True)
                try:
                    child=self.waiting_child(peer)
                    child_pidfd=os.pidfd_open(child[0])
                    time.sleep(1.2)
                    self.assertEqual(self.waiting_child(peer),child)
                    if mode=='acquire':
                        fcntl.flock(holder,fcntl.LOCK_UN)
                        output,error=peer.communicate(timeout=5)
                        self.assertEqual(peer.returncode,0,output+error)
                        self.assert_child_gone(child)
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
                        inherited.close()
                    else:
                        if mode=='stop':
                            (root/'desktop/stop').touch()
                        else:
                            peer.send_signal(signal.SIGTERM if mode=='signal' else signal.SIGKILL)
                        output,error=peer.communicate(timeout=5)
                        self.assertEqual(peer.returncode,{'stop':75,'signal':143,'abandon':-signal.SIGKILL}[mode],output+error)
                        if mode=='abandon':
                            self.assertEqual(select.select([child_pidfd],[],[],5)[0],[child_pidfd])
                            self.assertFalse(any(Path(f'/proc/{child[0]}/fd').glob('*')))
                        else:
                            self.assert_child_gone(child)
                        fcntl.flock(holder,fcntl.LOCK_UN)
                    # A forgotten child would later lock the inherited open
                    # description, which remains open here after cancellation.
                    for _ in range(2):
                        fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
                        fcntl.flock(probe,fcntl.LOCK_UN)
                        time.sleep(.1)
                finally:
                    if 'child_pidfd' in locals():
                        os.close(child_pidfd)
                    if peer.poll() is None:
                        os.killpg(peer.pid,signal.SIGKILL)
                        peer.communicate(timeout=5)

    def test_one_stable_waiter_transfers_the_lock_to_the_inherited_description(self):
        self.exercise('acquire')

    def test_stop_marker_cancels_and_joins_without_any_later_lock_grab(self):
        self.exercise('stop')

    def test_termination_cancels_and_joins_without_any_later_lock_grab(self):
        self.exercise('signal')

    def test_abandoned_supervisor_cannot_leave_a_future_lock_owner(self):
        self.exercise('abandon')

    def test_supervisor_refuses_an_already_missing_expected_parent(self):
        with tempfile.TemporaryDirectory(prefix='ru-lock-parent-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            with (root/'native.lock').open('w') as inherited:
                result=subprocess.run([sys.executable,str(REPOSITORY/'scripts/rehearse_update_run.py'),str(root),str(SCRIPT),
                    'wait-native-lock','--fd',str(inherited.fileno()),'--parent-pid',str(os.getpid()+10000000)],
                    pass_fds=(inherited.fileno(),),capture_output=True,text=True,timeout=5)
                self.assertEqual(result.returncode,75,result.stdout+result.stderr)

    def test_parent_death_before_child_arms_pdeathsig_cannot_enter_flock(self):
        with tempfile.TemporaryDirectory(prefix='ru-lock-arm-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'desktop').mkdir();(root/'bin').mkdir()
            ready,release=root/'ready',root/'release'
            real_setpriv=shutil.which('setpriv')
            shim=root/'bin/setpriv'
            shim.write_text(f'#!{sys.executable}\nimport os,sys,time\nfrom pathlib import Path\n'
                f'Path({str(ready)!r}).touch()\nwhile not Path({str(release)!r}).exists(): time.sleep(.01)\n'
                f'os.execv({real_setpriv!r},[{real_setpriv!r},*sys.argv[1:]])\n')
            shim.chmod(0o755)
            path=root/'native.lock'
            with path.open('w') as holder,path.open('w') as inherited,path.open('w') as probe:
                fcntl.flock(holder,fcntl.LOCK_EX)
                command=[sys.executable,str(REPOSITORY/'scripts/rehearse_update_run.py'),str(root),str(SCRIPT),
                         'wait-native-lock','--fd',str(inherited.fileno()),'--parent-pid',str(os.getpid())]
                peer=subprocess.Popen(command,pass_fds=(inherited.fileno(),),stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                    text=True,start_new_session=True,env={**os.environ,'PATH':str(root/'bin')+':'+os.environ['PATH']})
                child_fd=None
                try:
                    deadline=time.monotonic()+5
                    child=None
                    while time.monotonic()<deadline:
                        self.assertIsNone(peer.poll())
                        if ready.exists():
                            children=Path(f'/proc/{peer.pid}/task/{peer.pid}/children').read_text().split()
                            if children:
                                child=int(children[0])
                                fd=os.open(f'/proc/{child}/fd/0',os.O_RDONLY|os.O_NONBLOCK)
                                try:
                                    available=struct.unpack('I',fcntl.ioctl(fd,termios.FIONREAD,struct.pack('I',0)))[0]
                                finally:
                                    os.close(fd)
                                if available==1:
                                    break
                        time.sleep(.02)
                    else:self.fail('the lock child was not gated before its delayed prctl')
                    child_fd=os.pidfd_open(child)
                    peer.kill();peer.wait(timeout=5)
                    release.touch()
                    self.assertEqual(select.select([child_fd],[],[],5)[0],[child_fd])
                    self.assertFalse(any(Path(f'/proc/{child}/fd').glob('*')))
                    peer.communicate(timeout=5)
                    fcntl.flock(holder,fcntl.LOCK_UN)
                    fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
                    fcntl.flock(probe,fcntl.LOCK_UN)
                finally:
                    release.touch()
                    if peer.poll() is None:
                        peer.kill();peer.communicate(timeout=5)
                    if child_fd is not None:
                        try:signal.pidfd_send_signal(child_fd,signal.SIGTERM)
                        except ProcessLookupError:pass
                        os.close(child_fd)


class NativeTerminalAfterRestore(unittest.TestCase):
    def module(self):
        spec=importlib.util.spec_from_file_location('native_terminal_rehearsal',REPOSITORY/'scripts/rehearse_update_run.py')
        scenario=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        return scenario

    def evidence(self):
        root=Path('/var/tmp/native-action')
        version=root/'home/.local/share/cagents-install/versions/new'
        old={'host_session_id':'old','boot_id':'boot','host_incarnation':'old'}
        new={'host_session_id':'new','boot_id':'boot','host_incarnation':'new'}
        session={'id':'new-terminal','launch':{'program':'/bin/bash','working_directory':str(root)},'continuity':{'live':new}}
        def process(pid,image,argv,**fields):
            return {'pid':pid,'start_ticks':pid+1000,'executable':image,'argv':argv,**fields}
        windows={'main':10,'second':20}
        uis=[process(100,str(version/'bin/cagents-ui'),[str(version/'bin/cagents-ui'),'--restore-existing-only']),
             process(200,str(version/'bin/cagents-ui'),[str(version/'bin/cagents-ui'),'--restore-existing-only','--window','second'])]
        before={'root':str(root),'errors':[],'processes':uis+[process(300,str(version/'bin/cagentsd'),[str(version/'bin/cagentsd')])]}
        after={'root':str(root),'errors':[],'processes':copy.deepcopy(uis)+[
            process(400,str(version/'bin/cagentsd'),[str(version/'bin/cagentsd')]),
            process(500,str(Path('/bin/bash').resolve()),['/bin/bash'],terminal_id='new-terminal')]}
        activity={'claude_chat_launches':1,'claude_user_prompts':1,'codex_chat_launches':2,'codex_thread_starts':1,'codex_turn_starts':1}
        return version,{'before_processes':before,'after_processes':after,'before_windows':windows,'after_windows':windows,
            'before_serving':old,'after_status':{'ok':True,'data':{'instance':'production','running':True,'daemon_version':'new',
                'socket':str(root/'rt/cagents/cagentsd.sock')}},'terminal':{'ok':True,'data':{'terminal':{'session':session},'proof':new}},
            'census':{'transaction':'tx','source':new,'terminals':[session]},'census_digest':'digest',
            'receipt':{'transaction':'tx','census_digest':'digest','outcome':{'state':'usable','serving':new}},
            'selected_card':{'focused':True,'card':'new-card','terminal_id':'new-terminal','recipe':session['launch'],'continuity':{'live':new}},
            'cards_before_input':['old-card'],'token':'UNIQUE_OUTPUT','output_line':'UNIQUE_OUTPUT pid=500','shell_pid':500,
            'terminal_snapshot':{'data':{'snapshot':{'terminal_id':'new-terminal','lines':['$ command','UNIQUE_OUTPUT pid=500']}}},
            'activity_before':copy.deepcopy(activity),'activity_after':copy.deepcopy(activity)}

    def test_normal_action_requires_native_focus_real_shell_and_new_ready_owner(self):
        scenario=self.module()
        version,evidence=self.evidence()
        result=scenario.new_terminal_action_contract(evidence,version)
        self.assertEqual(result['shell']['pid'],500)
        self.assertEqual(result['daemon']['pid'],400)
        cases=[]
        changed=copy.deepcopy(evidence);changed['after_processes']['processes'][0]['pid']=101;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['after_windows']={'main':11,'second':20};cases.append(changed)
        changed=copy.deepcopy(evidence);changed['before_serving']=changed['terminal']['data']['proof'];cases.append(changed)
        changed=copy.deepcopy(evidence);changed['selected_card']['card']='old-card';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['selected_card']['focused']=False;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['after_processes']['processes'][-1]['terminal_id']='foreign';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['after_processes']['processes'][-1]['executable']='/usr/bin/foreign';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['receipt']['census_digest']='foreign';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['census']['terminals']=[];cases.append(changed)
        changed=copy.deepcopy(evidence);changed['activity_after']['codex_turn_starts']=2;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['activity_after']['claude_user_prompts']=2;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['terminal_snapshot']['data']['snapshot']['lines']=['printf UNIQUE_OUTPUT pid=500'];cases.append(changed)
        changed=copy.deepcopy(evidence);changed['terminal_snapshot']['data']['snapshot']['terminal_id']='foreign';cases.append(changed)
        for changed in cases:
            with self.subTest(evidence=changed),self.assertRaises(RuntimeError):
                scenario.new_terminal_action_contract(changed,version)

    def test_live_and_detached_terminal_variants_require_the_entire_current_proof(self):
        scenario=self.module()
        for shown_state in ('live','detached_same_boot'):
            for captured_state in ('live','detached_same_boot'):
                version,evidence=self.evidence()
                proof=evidence['terminal']['data']['proof']
                evidence['terminal']['data']['terminal']['session']=copy.deepcopy(evidence['terminal']['data']['terminal']['session'])
                evidence['terminal']['data']['terminal']['session']['continuity']={shown_state:proof}
                evidence['census']['terminals'][0]['continuity']={captured_state:proof}
                with self.subTest(shown=shown_state,captured=captured_state):
                    scenario.new_terminal_action_contract(evidence,version)
        for field in ('host_session_id','boot_id','host_incarnation'):
            version,evidence=self.evidence()
            proof=evidence['terminal']['data']['proof']
            evidence['selected_card']['continuity']={'detached_same_boot':{**proof,field:'stale'}}
            with self.subTest(stale=field),self.assertRaises(RuntimeError):
                scenario.new_terminal_action_contract(evidence,version)
        for mutation in ('non-live','foreign-terminal','changed-recipe'):
            version,evidence=self.evidence()
            if mutation=='non-live':evidence['terminal']['data']['terminal']['session']['continuity']={'relaunch_available':{}}
            elif mutation=='foreign-terminal':evidence['census']['terminals']=[{**evidence['census']['terminals'][0],'id':'foreign'}]
            else:evidence['census']['terminals']=[{**evidence['census']['terminals'][0],'launch':{'program':'/usr/bin/foreign'}}]
            with self.subTest(mutation=mutation),self.assertRaises(RuntimeError):
                scenario.new_terminal_action_contract(evidence,version)

    def test_expected_before_failure_requires_typed_failure_and_absent_daemon(self):
        scenario=self.module()
        version,evidence=self.evidence()
        evidence['after_status']['data'].update(running=False,daemon_present=False)
        evidence['after_processes']['processes']=evidence['before_processes']['processes'][:2]
        evidence['selected_card']['continuity']={'relaunch_available':{'recipe':{'program':'/bin/bash'},'last_exit':None}}
        evidence['failure_diagnostic']='Code: runtime_hydration_failed\nMessage: The terminal could not be restored.\nTerminal: new-terminal\nDirectory: /var/tmp/native-action'
        evidence['failure_observation']={'window':{'id':10}}
        result=scenario.terminal_restart_failure_contract(evidence,version)
        self.assertEqual(result['outcome'],'expected_terminal_restart_failure')
        for mutation in ('missing-error','foreign-error','daemon','old-card','replayed-prompt','claude-replayed-prompt'):
            changed=copy.deepcopy(evidence)
            if mutation=='missing-error':changed['failure_diagnostic']=''
            elif mutation=='foreign-error':changed['failure_diagnostic']=evidence['failure_diagnostic'].replace('new-terminal','foreign-terminal')
            elif mutation=='daemon':changed['after_status']['data']['running']=True
            elif mutation=='old-card':changed['selected_card']['card']='old-card'
            elif mutation=='replayed-prompt':changed['activity_after']['codex_turn_starts']=2
            else:changed['activity_after']['claude_user_prompts']=2
            with self.subTest(mutation=mutation),self.assertRaises(RuntimeError):
                scenario.terminal_restart_failure_contract(changed,version)

    def test_pending_before_failure_requires_exact_bounded_native_and_typed_absence_evidence(self):
        scenario=self.module()
        version,evidence=self.evidence()
        evidence['after_status']['data'].update(running=False,daemon_present=False)
        evidence['after_processes']['processes']=evidence['before_processes']['processes'][:2]
        evidence['selected_card']['continuity']={'relaunch_available':{'recipe':{'program':'/bin/bash'},'last_exit':None}}
        evidence['failure_kind']='pending_without_daemon'
        observation={'window':{'id':10},'runs':[{'text':'Starting terminal…'},{'text':'The daemon is unreachable'}]}
        evidence['failure_observation']=observation
        sample={'monotonic':100,'card':copy.deepcopy(evidence['selected_card']),'snapshot_terminal_id':'new-terminal',
                'snapshot_exit':1,'snapshot_refusal':{'ok':False,'error':{'kind':'daemon_not_running'}},
                'status':copy.deepcopy(evidence['after_status']),'observation':observation,
                'processes':copy.deepcopy(evidence['after_processes']),'windows':dict(evidence['after_windows'])}
        evidence['pending_observations']=[copy.deepcopy(sample),{**copy.deepcopy(sample),'monotonic':104}]
        self.assertEqual(scenario.terminal_restart_failure_contract(evidence,version)['observed_state'],'pending_without_daemon')
        cases=[]
        changed=copy.deepcopy(evidence);changed['pending_observations']=[];cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['monotonic']=101;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['observation']['runs']=[{'text':'Unknown state'}];cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['snapshot_refusal']['error']['kind']='protocol';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['snapshot_terminal_id']='foreign';cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['status']['data']['running']=True;cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['processes']['processes'].append(
            {'pid':500,'start_ticks':1500,'terminal_id':'new-terminal','executable':'/usr/bin/bash','argv':['bash']});cases.append(changed)
        changed=copy.deepcopy(evidence);changed['pending_observations'][1]['windows']['main']=99;cases.append(changed)
        for changed in cases:
            with self.subTest(evidence=changed),self.assertRaises(RuntimeError):
                scenario.terminal_restart_failure_contract(changed,version)

    def test_shortcut_is_read_from_the_actual_private_settings_without_a_default(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-action-key-') as temporary:
            root=Path(temporary)
            path=root/'home/.config/cagents/settings.toml';path.parent.mkdir(parents=True)
            path.write_text('[keyboard_shortcuts]\nnew_terminal=["Ctrl+Shift+N"]\n')
            result=scenario.terminal_shortcut(root)
            self.assertEqual(result['chord'],'ctrl+shift+n')
            self.assertEqual(result['sha256'],hashlib.sha256(path.read_bytes()).hexdigest())
            path.write_text('[keyboard_shortcuts]\nnew_thread=["Ctrl+T"]\n')
            with self.assertRaisesRegex(RuntimeError,'unavailable'):
                scenario.terminal_shortcut(root)

    def test_output_probe_uses_the_issued_shell_syntax_and_refuses_unknown_programs(self):
        scenario=self.module()
        for program,variable in (('/usr/bin/fish','$fish_pid'),('/bin/bash','$$'),('/bin/sh','$$')):
            probe=scenario.terminal_probe_command({'launch':{'program':program}},'TOKEN','/var/tmp/private/output.txt')
            self.assertEqual(probe['pid_variable'],variable)
            self.assertIn(f'"{variable}"',probe['command'])
            self.assertNotIn('\n',probe['command'])
        with self.assertRaisesRegex(RuntimeError,'unsupported'):
            scenario.terminal_probe_command({'launch':{'program':'/usr/bin/unknown-shell'}},'TOKEN','/var/tmp/private/output.txt')

    def test_native_probe_submits_the_same_command_with_a_separate_return(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-input-') as temporary:
            root=Path(temporary)
            rehearsal=scenario.Rehearsal(root,str(SCRIPT))
            receipt=root/'typed-output.txt'
            probe=scenario.terminal_probe_command({'launch':{'program':'/bin/fish'}},'TOKEN',receipt)
            evidence={'events':[]}
            saved=[]
            actions=[]
            def call(*args,**kwargs):
                if args[0]=='drive':
                    actions.append(args)
                    self.assertEqual(saved[-1]['typed_command'],probe['command'])
                    if args==('drive','key','Return'):
                        receipt.write_text('TOKEN pid=123\n')
                return 0,''
            with patch.object(rehearsal,'call',side_effect=call), \
                    patch.object(rehearsal,'cli',return_value=(0,{'data':{}})), \
                    patch.object(rehearsal,'private_processes',return_value={'processes':[]}):
                result=rehearsal.type_terminal_probe({'terminal_id':'issued'},probe,'TOKEN',receipt,
                    evidence,lambda:saved.append(copy.deepcopy(evidence)))
            self.assertEqual(actions,[('drive','type',probe['command']),('drive','key','Return')])
            self.assertEqual(result,('TOKEN pid=123',123))
            self.assertIn('before-type',saved[-1])
            self.assertEqual([event['event'] for event in evidence['events']],
                ['native_terminal_text_requested','native_terminal_text_completed',
                 'native_terminal_return_requested','native_terminal_return_completed'])

    def test_input_process_diagnostics_discard_fields_if_pid_is_reused(self):
        scenario=self.module()
        original='123 (fish) '+' '.join(['0']*19+['456'])
        reused='123 (other) '+' '.join(['0']*19+['789'])
        process={'pid':123,'start_ticks':456}
        with patch.object(Path,'read_text',side_effect=[original,'wait_woken','read',original]):
            self.assertEqual(scenario.terminal_process_input_state(process),
                {'state':'same_process','wchan':'wait_woken','syscall':'read'})
        with patch.object(Path,'read_text',side_effect=[original,'unrelated_wait','other_syscall',reused]):
            self.assertEqual(scenario.terminal_process_input_state(process),{'state':'ended_or_reused'})
        with patch.object(Path,'read_text',side_effect=FileNotFoundError('exited')):
            self.assertEqual(scenario.terminal_process_input_state(process)['state'],'unavailable')

    def test_native_probe_failure_preserves_command_and_exact_terminal_evidence(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-input-failure-') as temporary:
            root=Path(temporary)
            rehearsal=scenario.Rehearsal(root,str(SCRIPT))
            receipt=root/'typed-output.txt'
            probe=scenario.terminal_probe_command({'launch':{'program':'/bin/fish'}},'TOKEN',receipt)
            evidence={'events':[]}
            saved=[]
            def call(*args,**kwargs):
                if args==('drive','key','Return'):
                    raise RuntimeError('native Return failed')
                return 0,''
            with patch.object(rehearsal,'call',side_effect=call), \
                    patch.object(rehearsal,'cli',return_value=(0,{'exact_terminal':'issued'})) as cli, \
                    patch.object(rehearsal,'private_processes',return_value={'processes':[]}):
                with self.assertRaisesRegex(RuntimeError,'native Return failed'):
                    rehearsal.type_terminal_probe({'terminal_id':'issued'},probe,'TOKEN',receipt,
                        evidence,lambda:saved.append(copy.deepcopy(evidence)))
            failure=saved[-1]
            self.assertEqual(failure['typed_command'],probe['command'])
            self.assertIn('native Return failed',failure['input_failure'])
            self.assertEqual(failure['input-failed']['show']['value'],{'exact_terminal':'issued'})
            self.assertFalse(failure['input-failed']['receipt_exists'])
            cli.assert_any_call('--no-start','terminal','snapshot','issued',check=False)
            cli.assert_any_call('--no-start','terminal','show','issued',check=False)

    def test_successful_native_input_without_shell_output_still_fails_with_evidence(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-input-no-output-') as temporary:
            root=Path(temporary)
            rehearsal=scenario.Rehearsal(root,str(SCRIPT))
            receipt=root/'typed-output.txt'
            probe=scenario.terminal_probe_command({'launch':{'program':'/bin/fish'}},'TOKEN',receipt)
            evidence={'events':[]}
            saved=[]
            with patch.object(rehearsal,'call',return_value=(0,'')), \
                    patch.object(rehearsal,'cli',return_value=(0,{'data':{}})), \
                    patch.object(rehearsal,'private_processes',return_value={'processes':[]}), \
                    patch.object(scenario.time,'monotonic',side_effect=[0,1,2,3,4,20]):
                with self.assertRaisesRegex(RuntimeError,'did not execute'):
                    rehearsal.type_terminal_probe({'terminal_id':'issued'},probe,'TOKEN',receipt,
                        evidence,lambda:saved.append(copy.deepcopy(evidence)))
            self.assertFalse(saved[-1]['input-failed']['receipt_exists'])
            self.assertIn('did not execute',saved[-1]['input_failure'])

    def test_terminal_card_is_bound_through_normalized_recipe_and_live_proof(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-action-card-') as temporary:
            root=Path(temporary)
            database=root/'home/.local/share/cagents/sessions.sqlite3'
            database.parent.mkdir(parents=True)
            recipe={'program':'/bin/bash'}
            continuity={'live':{'host_session_id':'new','boot_id':'boot','host_incarnation':'new'}}
            with closing(sqlite3.connect(database)) as connection, connection:
                connection.executescript('CREATE TABLE cards(card_id TEXT,workspace_id TEXT,kind_tag TEXT,existence_json TEXT);'
                    'CREATE TABLE workspaces(workspace_id TEXT,focused_card_id TEXT);'
                    'CREATE TABLE terminal_recipes(card_id TEXT,terminal_id TEXT,recipe_json TEXT,continuity_json TEXT);')
                connection.execute('INSERT INTO cards VALUES(?,?,?,?)',['card','workspace','terminal',json.dumps('open')])
                connection.execute('INSERT INTO workspaces VALUES(?,?)',['workspace','card'])
                connection.execute('INSERT INTO terminal_recipes VALUES(?,?,?,?)',['card','terminal',json.dumps(recipe),json.dumps(continuity)])
            cards=scenario.terminal_cards(root)
            self.assertEqual(len(cards),1)
            self.assertEqual((cards[0]['card'],cards[0]['terminal_id'],cards[0]['focused']),('card','terminal',True))
            self.assertEqual(cards[0]['recipe'],recipe)
            self.assertEqual(cards[0]['continuity'],continuity)

    def test_only_new_terminal_flags_require_the_complete_healthy_restore_flow(self):
        scenario=self.module()
        base=['script','/var/tmp/unused',str(SCRIPT),'run','--old-dir','old','--old-version','old',
              '--old-installer','old/install.sh','--new-dir','new','--new-version','new','--new-installer','new/install.sh']
        for flags in (['--new-terminal-after-restore'],['--expect-terminal-restart-failure']):
            with self.subTest(flags=flags),patch.object(sys,'argv',base+flags),patch('sys.stderr',new=io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    scenario.main()
                self.assertEqual(error.exception.code,2)

    def test_ended_original_child_is_allowed_only_after_the_deliberate_stop(self):
        with tempfile.TemporaryDirectory(prefix='ru-action-ended-child-') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'child.pid').write_text(str(int(Path('/proc/sys/kernel/pid_max').read_text())+1))
            strict=records.private_process_snapshot(root)
            self.assertTrue(any('child.pid' in error for error in strict['errors']))
            after_stop=records.private_process_snapshot(root,after_intentional_stop=True)
            self.assertEqual(after_stop['errors'],[])
            self.assertIs(after_stop['after_intentional_stop'],True)
            errors=[]
            records.processes(root,errors)
            self.assertTrue(any('child.pid' in error for error in errors))


class ExistingOnlyReopen(unittest.TestCase):
    def module(self):
        spec=importlib.util.spec_from_file_location('existing_only_rehearsal',REPOSITORY/'scripts/rehearse_update_run.py')
        scenario=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        return scenario

    def samples(self):
        root=Path('/var/tmp/private-reopen')
        version=root/'home/.local/share/cagents-install/versions/new'
        def process(pid,image,argv,**fields):
            return {'pid':pid,'start_ticks':pid+1000,'executable':image,'argv':argv,**fields}
        before={'root':str(root),'errors':[],'serving':{'host_session_id':'serving','boot_id':'boot','host_incarnation':'one'},
            'processes':[
                process(1,str(version/'bin/cagentsd'),[str(version/'bin/cagentsd')]),
                process(2,str(version/'bin/cagents-ui'),[str(version/'bin/cagents-ui')]),
                process(3,str(version/'bin/cagents-ui'),[str(version/'bin/cagents-ui'),'--window','second']),
                process(4,'/usr/bin/sh',['sh',str(root/'bin/claude')]),
                process(5,'/usr/bin/python3',['python3',str(root/'libexec/codex/rehearsal-codex.py'),'app-server','--listen','unix:///private/control.sock']),
                process(6,'/usr/bin/sleep',['sleep','86400'],role='long-lived-child'),
            ]}
        after=copy.deepcopy(before)
        for row in after['processes'][1:3]:
            row['pid']+=10;row['start_ticks']+=10;row['argv'].append('--restore-existing-only')
        activity={'claude_chat_launches':1,'codex_chat_launches':1,'codex_thread_starts':1,'codex_turn_starts':1}
        return before,after,activity,version

    def test_actual_new_ui_policy_preserves_serving_owners(self):
        scenario=self.module()
        before,after,activity,version=self.samples()
        result=scenario.existing_only_contract(before,after,activity,activity,version)
        self.assertEqual(result['ui_after'],[(12,1012),(13,1013)])
        self.assertEqual(result['daemon_processes'],[(1,1001)])
        self.assertEqual(result['provider_processes'],{'claude':[(4,1004)],'codex':[(5,1005)]})
        self.assertEqual(result['terminal_children'],[(6,1006)])

    def test_reopen_rejects_hidden_turn_replay_provider_restart_or_ordinary_ui(self):
        scenario=self.module()
        mutations={
            'same-process-turn-replay':lambda a,c:c.update(codex_turn_starts=2),
            'extra-provider-start':lambda a,c:c.update(claude_chat_launches=2),
            'daemon-owner-changed':lambda a,c:a['serving'].update(host_incarnation='foreign'),
            'daemon-process-replaced':lambda a,c:a['processes'][0].update(pid=101),
            'provider-process-replaced':lambda a,c:a['processes'][3].update(pid=104),
            'child-pid-reused':lambda a,c:a['processes'][5].update(start_ticks=2000),
            'ordinary-ui-focused':lambda a,c:a['processes'][1]['argv'].remove('--restore-existing-only'),
            'old-ui-not-replaced':lambda a,c:a['processes'][1].update(pid=2,start_ticks=1002),
            'wrong-kernel-ui-image':lambda a,c:a['processes'][1].update(executable='/unrelated/cagents-ui'),
        }
        for name,change in mutations.items():
            with self.subTest(name=name):
                before,after,activity,version=self.samples()
                current=copy.deepcopy(activity);change(after,current)
                with self.assertRaises(RuntimeError):
                    scenario.existing_only_contract(before,after,activity,current,version)

    def test_launch_counters_distinguish_probes_from_conversation_resume(self):
        scenario=self.module()
        with tempfile.TemporaryDirectory(prefix='ru-provider-counts-') as temporary:
            root=Path(temporary)
            (root/'home/.claude').mkdir(parents=True);(root/'logs').mkdir()
            (root/'home/.claude/fixture-launches').write_text('new\nresumed-thread\n')
            history=root/'home/.claude/projects/project/session.jsonl'
            history.parent.mkdir(parents=True)
            rows=[{'type':'user','message':{'content':'first prompt'}},
                  {'type':'user','message':{'content':[{'type':'text','text':'second prompt'}]}},
                  {'type':'user','message':{'content':[{'type':'tool_result','content':'tool output'}]}},
                  {'type':'user','isMeta':True,'message':{'content':'metadata'}},
                  {'type':'user','origin':{'kind':'task-notification'},'message':{'content':'background'}}]
            history.write_text(''.join(json.dumps(row)+'\n' for row in rows))
            events=[
                {'event':'launch','pid':1,'argv':['app-server','--listen','unix:///conversation/control.sock']},
                {'event':'request','pid':1,'method':'thread/start'},
                {'event':'request','pid':1,'method':'turn/start'},
                {'event':'launch','pid':2,'argv':['app-server']},
                {'event':'probe-request','pid':2,'method':'account/read'},
                {'event':'launch','pid':3,'argv':['app-server']},
                {'event':'probe-request','pid':3,'method':'thread/resume'},
            ]
            path=root/'logs/codex-provider.jsonl'
            path.write_text(''.join(json.dumps(event)+'\n' for event in events))
            expected={'claude_chat_launches':2,'claude_user_prompts':2,'codex_chat_launches':2,'codex_thread_starts':1,'codex_turn_starts':1}
            self.assertEqual(scenario.provider_activity(root),expected)
            with path.open('a') as stream:
                stream.write(json.dumps({'event':'launch','pid':4,'argv':['app-server']})+'\n')
                stream.write(json.dumps({'event':'probe-request','pid':4,'method':'model/list'})+'\n')
            self.assertEqual(scenario.provider_activity(root),expected)
            with history.open('a') as stream:
                stream.write(json.dumps(rows[0])+'\n')
            self.assertEqual(scenario.provider_activity(root)['claude_user_prompts'],3)

    def test_option_refuses_failure_or_missing_codex_before_actions(self):
        with tempfile.TemporaryDirectory(prefix='ru-policy-args-') as temporary:
            root=Path(temporary)/'absent'
            command=[sys.executable,str(REPOSITORY/'scripts/rehearse_update_run.py'),str(root),str(SCRIPT),'run',
                     '--old-dir','/unused','--old-version','old','--old-installer','/unused/install.sh',
                     '--new-dir','/unused','--new-version','new','--new-installer','/unused/install.sh',
                     '--reopen-existing-only']
            for option in (['--fail-new-daemon'],['--no-codex'],['--ceditor','/unused']):
                result=subprocess.run([*command,*option],capture_output=True,text=True)
                self.assertEqual(result.returncode,2,result.stdout+result.stderr)
                self.assertIn('requires a healthy update',result.stderr)
                self.assertFalse(root.exists())


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

    def test_counted_daemon_stop_is_private_and_leaves_other_private_processes_alive(self):
        self.assertEqual(self.call('init').returncode,0)
        prefix=self.root/'home/.local/share/cagents-install'
        cli=prefix/'versions/new/bin/cagents';cli.parent.mkdir(parents=True)
        cli.write_text('''#!/usr/bin/python3
import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args==['--no-start','daemon','preflight']:
 print('fixture-private-counted-capability')
elif args==['--no-start','--json','daemon','stop','--confirm','fixture-private-counted-capability']:
 Path(os.environ['CAGENTS_REHEARSAL_ROOT'],'logs','stop-only.json').write_text(json.dumps({'home':os.environ['HOME']}))
 print(json.dumps({'ok':True,'data':{'stopped':True}}))
else:sys.exit(2)
''')
        cli.chmod(0o755)
        (prefix/'current').symlink_to('versions/new')
        launcher=self.root/'home/.local/bin/cagents';launcher.symlink_to(prefix/'current/bin/cagents')
        peer=subprocess.Popen(['sleep','60'],env={**os.environ,'HOME':str(self.root/'home')})
        try:
            result=self.call('stop-daemon')
            self.assertEqual(result.returncode,0,result.stdout+result.stderr)
            self.assertNotIn('fixture-private-counted-capability',result.stdout+result.stderr)
            self.assertIsNone(peer.poll())
            self.assertEqual(json.loads((self.root/'logs/stop-only.json').read_text())['home'],str(self.root/'home'))
            launcher.unlink();launcher.symlink_to('/usr/bin/false')
            refused=self.call('stop-daemon')
            self.assertEqual(refused.returncode,2)
            self.assertIn('installed CLI',refused.stderr)
        finally:
            peer.terminate();peer.wait(timeout=5)

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

    def test_existing_only_launch_executes_the_ui_directly_with_private_environment(self):
        self.assertEqual(self.call('init').returncode,0)
        desktop=self.root/'desktop'
        (desktop/'home').mkdir(parents=True)
        (desktop/'pid').write_text(str(os.getpid()))
        (desktop/'ready').touch()
        (desktop/'env').write_text('WAYLAND_DISPLAY=fixture-wayland\n'
                                  'DBUS_SESSION_BUS_ADDRESS=unix:path=/fixture\n'
                                  f'XDG_RUNTIME_DIR={desktop}/rt\n')
        tools=self.root/'caller-tools';tools.mkdir()
        harness=tools/'niri-harness'
        harness.write_text('#!/bin/sh\nprintf "%s\\n" "export WAYLAND_DISPLAY=fixture-wayland"\n')
        harness.chmod(0o755)
        prefix=self.root/'home/.local/share/cagents-install'
        binary=prefix/'versions/new/bin/cagents-ui'
        binary.parent.mkdir(parents=True)
        output=self.root/'ui-launch.json'
        binary.write_text('#!/usr/bin/python3\nimport json,os,pathlib,sys\n'
            f'pathlib.Path({str(output)!r}).write_text(json.dumps({{"argv":sys.argv[1:],"home":os.environ["HOME"],"secret":os.environ.get("PRIVATE_SECRET")}}))\n')
        binary.chmod(0o755)
        (prefix/'current').symlink_to('versions/new')
        cli=self.root/'home/.local/bin/cagents'
        cli.write_text('#!/bin/sh\nexit 91\n');cli.chmod(0o755)
        result=self.call('launch','--restore-existing-only','second',
            env={**os.environ,'PATH':str(tools)+':'+os.environ['PATH'],'PRIVATE_SECRET':'must-not-pass'})
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        deadline=time.monotonic()+5
        while not output.exists():
            self.assertLess(time.monotonic(),deadline)
            time.sleep(.01)
        value=json.loads(output.read_text())
        self.assertEqual(value,{'argv':['--restore-existing-only','--window','second'],
                                'home':str(self.root/'home'),'secret':None})

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
