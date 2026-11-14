#!/usr/bin/env python3
"""Pure contracts for the update rehearsal; no compositor or installed daemon."""
from __future__ import annotations

import importlib.util
import io
import copy
import hashlib
import json
import fcntl
from contextlib import closing, contextmanager
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
import threading
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
        before['fixture_events'] = [
            {'event': 'listening', 'pid': 21, 'socket': '/private/original.sock'},
            {'event': 'notification', 'method': 'turn/started', 'thread_id': 'provider-issued-thread',
             'parent': True, 'pid': 21, 'socket': '/private/original.sock'},
            {'event': 'notification', 'method': 'turn/started', 'thread_id': 'provider-child-thread',
             'parent': False, 'pid': 21, 'socket': '/private/original.sock'},
        ]
        after['fixture_events'] = copy.deepcopy(before['fixture_events'])
        del after["sessions"]["lineage"]
        return before, after

    def paused_codex_replacement(self):
        before, after = self.paused_replacement()
        original = before['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content']
        original['launch']['provider'] = 'Codex'
        after['recovery']['census']['chats'] = [copy.deepcopy(original)]
        after['stores']['windows/main/sessions.sqlite3']['rows']['cards'][1]['content'] = records.paused_history_projection(before, after, original)
        return before, after

    def test_receipt_paused_codex_parent_or_bound_child_cannot_resume(self):
        for thread in ('provider-issued-thread', 'provider-child-thread'):
            with self.subTest(thread=thread):
                before, after = self.paused_codex_replacement()
                self.assertFalse(any(row['regression'] for row in records.compare(before, after)))
                after['fixture_events'] += [
                    {'event': 'listening', 'pid': 31, 'socket': '/private/successor.sock'},
                    {'event': 'request', 'method': 'thread/resume', 'thread_id': thread,
                     'parent': thread == 'provider-issued-thread', 'pid': 31,
                     'socket': '/private/successor.sock', 'errors': []}]
                failures = [row for row in records.compare(before, after) if row['regression']]
                self.assertEqual({row['kind'] for row in failures}, {'paused-history'})
                self.assertEqual(failures[0]['thread_id'], thread)

    def test_paused_codex_binding_is_fail_closed_when_missing_or_ambiguous(self):
        for case in ('thread', 'listener', 'missing-listening', 'late-listening', 'ambiguous-pane', 'ambiguous-parent', 'truncated-events'):
            with self.subTest(case=case):
                before, after = self.paused_codex_replacement()
                rows = before['stores']['windows/main/sessions.sqlite3']['rows']['cards']
                if case == 'thread':
                    rows[1]['content']['launch']['session_id'] = None
                elif case == 'listener':
                    before['fixture_events'] = []
                    after['fixture_events'] = []
                elif case in ('missing-listening', 'late-listening'):
                    listening = before['fixture_events'].pop(0)
                    if case == 'late-listening':
                        before['fixture_events'].append(listening)
                    after['fixture_events'] = copy.deepcopy(before['fixture_events'])
                elif case == 'ambiguous-pane':
                    alias = copy.deepcopy(rows[1])
                    alias['card_id'] = 'other-pane'
                    alias['content']['launch']['session_id'] = 'foreign-thread'
                    rows.append(alias)
                elif case == 'ambiguous-parent':
                    before['fixture_events'].append({'event': 'notification', 'method': 'turn/started',
                        'thread_id': 'other-parent', 'parent': True, 'pid': 21, 'socket': '/private/original.sock'})
                    after['fixture_events'] = copy.deepcopy(before['fixture_events'])
                else:
                    after['fixture_events'] = after['fixture_events'][1:]
                self.assertTrue(any(row['regression'] and row['kind'] == 'paused-history'
                                    for row in records.compare(before, after)))

    def test_paused_codex_does_not_block_a_different_retained_listener_thread(self):
        before, after = self.paused_codex_replacement()
        before['fixture_events'] += [{'event': 'listening', 'pid': 41, 'socket': '/private/retained.sock'},
            {'event': 'notification', 'method': 'turn/started', 'thread_id': 'retained-thread',
             'parent': True, 'pid': 41, 'socket': '/private/retained.sock'}]
        after['fixture_events'] = copy.deepcopy(before['fixture_events']) + [
            {'event': 'request', 'method': 'thread/resume', 'thread_id': 'retained-thread',
             'parent': True, 'pid': 41, 'socket': '/private/retained.sock', 'errors': []}]
        self.assertFalse(any(row['regression'] for row in records.compare(before, after)))

    def test_first_receipt_paused_history_stays_blocked_through_later_retry_events(self):
        before, restored = self.paused_codex_replacement()
        original_receipt = copy.deepcopy(restored['recovery'])
        later = copy.deepcopy(restored['fixture_events']) + [
            {'event': 'listening', 'pid': 51, 'socket': '/private/retry.sock'},
            {'event': 'request', 'method': 'thread/resume', 'thread_id': 'provider-child-thread',
             'parent': False, 'pid': 51, 'socket': '/private/retry.sock', 'errors': []}]
        paused, _ = records.verified_pauses(before, restored)
        changes = records.paused_codex_reattachments(before, restored, paused, observed_events=later)
        self.assertEqual(len(changes), 1)
        self.assertTrue(changes[0]['regression'])
        self.assertEqual(changes[0]['thread_id'], 'provider-child-thread')
        self.assertEqual(restored['recovery'], original_receipt)

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

    @staticmethod
    def strip(snapshot: dict, store: str, ordinals: dict) -> None:
        rows = snapshot["stores"].setdefault(store, {"rows": {"cards": []}})["rows"]
        rows["cards"] = [{"card_id": card, "workspace_id": "workspace", "ordinal": ordinal,
                          "kind_tag": "chat", "title": card, "existence": "open"}
                         for card, ordinal in ordinals.items()]

    def pane_changes(self, before: dict, after: dict) -> list:
        return [(row["id"].rpartition(":")[2], row["change"], row["regression"])
                for row in records.compare(before, after) if row["kind"] == "panes"]

    def test_a_window_closing_a_hole_in_its_pane_ordinals_keeps_their_order(self):
        before, after = snapshot(), snapshot()
        store = "windows/second/sessions.sqlite3"
        self.strip(before, store, {"a": 0, "b": 1, "c": 3, "d": 4})
        self.strip(after, store, {"a": 0, "b": 1, "c": 2, "d": 3})
        self.assertEqual(self.pane_changes(before, after), [])

    def test_a_reordered_pane_strip_is_a_regression(self):
        before, after = snapshot(), snapshot()
        store = "windows/second/sessions.sqlite3"
        self.strip(before, store, {"a": 0, "b": 1, "c": 2})
        self.strip(after, store, {"a": 0, "b": 2, "c": 1})
        self.assertEqual(self.pane_changes(before, after),
                         [("b", "position", True), ("c", "position", True)])

    def test_a_lost_or_added_pane_does_not_shift_the_panes_after_it(self):
        before, after = snapshot(), snapshot()
        store = "windows/second/sessions.sqlite3"
        self.strip(before, store, {"a": 0, "lost": 1, "b": 2, "c": 3})
        self.strip(after, store, {"a": 0, "b": 1, "added": 2, "c": 3})
        self.assertEqual(self.pane_changes(before, after),
                         [("lost", "disappeared", True), ("added", "added", False)])

    def test_each_window_store_orders_its_own_panes(self):
        before, after = snapshot(), snapshot()
        # The main store keeps its shell at ordinal 0; the second store's panes
        # sort after it numerically but are ranked only among themselves.
        self.strip(before, "windows/second/sessions.sqlite3", {"a": 5, "b": 7})
        self.strip(after, "windows/second/sessions.sqlite3", {"a": 0, "b": 1})
        self.assertEqual(self.pane_changes(before, after), [])
        self.strip(after, "windows/second/sessions.sqlite3", {"a": 1, "b": 0})
        self.assertEqual(self.pane_changes(before, after),
                         [("a", "position", True), ("b", "position", True)])

    def test_a_pane_moved_to_another_workspace_does_not_shift_its_new_neighbours(self):
        before, after = snapshot(), snapshot()
        store = "windows/second/sessions.sqlite3"
        self.strip(before, store, {"a": 0, "moved": 1, "b": 0})
        self.strip(after, store, {"a": 0, "moved": 0, "b": 1})
        for snapshot_value, workspaces in ((before, ("one", "one", "two")), (after, ("one", "two", "two"))):
            for card, workspace in zip(snapshot_value["stores"][store]["rows"]["cards"], workspaces):
                card["workspace_id"] = workspace
        self.assertEqual(self.pane_changes(before, after), [("moved", "workspace_id", True)])

    def test_card_ids_containing_a_colon_are_ranked_within_their_store(self):
        before, after = snapshot(), snapshot()
        store = "windows/second/sessions.sqlite3"
        self.strip(before, store, {"p:1": 0, "q:2": 1})
        self.strip(after, store, {"p:1": 1, "q:2": 0})
        self.assertEqual(self.pane_changes(before, after),
                         [("1", "position", True), ("2", "position", True)])

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
        identity = {'parent':True,'thread_id':'provider-issued-thread','turn_id':'parent-turn','socket':'/private/control.sock','pid':21}
        before['fixture_events'] = [{'event':'listening','pid':21,'socket':'/private/control.sock','time':0.5},
                                   {**identity,'event':'notification','method':'turn/started','time':1}]
        after['fixture_events'] = copy.deepcopy(before['fixture_events']) + [
            {**identity,'event':'request','method':'turn/interrupt','time':2,'errors':[]},
            {**identity,'event':'notification','method':'turn/completed','status':'interrupted','time':3}]
        self.assertTrue(records.confirmed_interruption(before,after,'agent'))
        self.assertFalse(any(row['regression'] for row in records.compare(before,after)))
        for name, mutate in {
            'enter':lambda a:a['target'].update(working=''),
            'new-run':lambda a:a['agents']['agent'].update(run='other'),
            'no-receipt':lambda a:a['fixture_events'].pop(),
            'child-only':lambda a:a['fixture_events'][3].update(parent=False),
            'wrong-turn':lambda a:a['fixture_events'][3].update(turn_id='other'),
            'wrong-thread':lambda a:a['fixture_events'][3].update(thread_id='other'),
            'normal-completion':lambda a:a['fixture_events'][3].update(status='completed'),
            'no-request':lambda a:a['fixture_events'].pop(2),
            'request-error':lambda a:a['fixture_events'][2].update(errors=['refused']),
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


class ScaleSeed(unittest.TestCase):
    def module(self):
        spec = importlib.util.spec_from_file_location('rehearsal_scale', REPOSITORY/'scripts/rehearse_update_run.py')
        scenario = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scenario)
        return scenario

    def test_scale_options_reach_the_seed_command(self):
        import argparse
        scenario = self.module()
        parser = argparse.ArgumentParser()
        scenario.seed_options(parser)
        args = parser.parse_args(['--scale-chats', '120', '--scale-entries', '300', '--scale-bytes', '2048'])
        self.assertEqual(parser.parse_args(scenario.seed_flags(args)), args)
        self.assertNotIn('--scale-chats', scenario.seed_flags(parser.parse_args([])))

    def test_scale_chats_open_long_and_idle_then_their_switches_are_removed(self):
        scenario = self.module()
        with tempfile.TemporaryDirectory(prefix='ru-scale-') as temporary:
            root = Path(temporary)
            claude = root/'home/.claude'
            claude.mkdir(parents=True)
            original = json.dumps({'pause': 6}, sort_keys=True)+'\n'
            (root/'codex-fixture.json').write_text(original)
            rehearsal = scenario.Rehearsal(str(root), str(SCRIPT))
            spawned, channels = [], []
            states = {}
            def seed(name, argv, project=None):
                self.assertEqual((claude/'fixture-long-history').read_text(), '300 2048\n')
                self.assertEqual((claude/'fixture-first-turn-seconds').read_text(), '1\n')
                self.assertEqual(json.loads((root/'codex-fixture.json').read_text()),
                                 {'pause': 6, 'history': [300, 2048]})
                if argv[:2] == ['channel','spawn']:
                    channels.append(argv[argv.index('--title')+1])
                    return {'response': {'data': {'channel': {'id': f'channel-{len(channels)}'}}}}
                self.assertLess(sum(state == 'Running' for state in states.values()), 8)
                spawned.append((argv[argv.index('--provider')+1], project, argv[argv.index('--channel')+1]))
                states[argv[argv.index('--name')+1]] = 'Running'
            def cli(*arguments, **options):
                listing = {'data': {'agents': [{'agent': {'name': name, 'origin': 'User'}, 'state': state}
                                               for name, state in states.items()]}}
                # Each listing lets one scale run finish; the seed's own chats never do.
                finished = next((name for name, state in states.items()
                                 if state == 'Running' and name.startswith('scale-')), None)
                if finished:
                    states[finished] = 'Succeeded'
                return 0, listing
            # Seven held slots leave room for one scale chat at a time.
            states.update({f'held-{n}': 'Running' for n in range(7)})
            args = type('Args', (), {'scale_chats': 4, 'scale_entries': 300, 'scale_bytes': 2048})
            with patch.object(rehearsal, 'cli', side_effect=cli), patch.object(scenario.time, 'sleep'):
                rehearsal.seed_scale(args, seed)
            self.assertEqual(channels, ['Scale alpha', 'Scale beta'])
            self.assertEqual(spawned, [('codex','alpha','channel-1'),('claude','beta','channel-2'),
                                       ('claude','alpha','channel-1'),('codex','beta','channel-2')])
            self.assertEqual(rehearsal.errors, [])
            self.assertEqual({states[f'scale-{n}'] for n in range(1, 5)}, {'Succeeded'})
            self.assertFalse((claude/'fixture-long-history').exists())
            self.assertFalse((claude/'fixture-first-turn-seconds').exists())
            self.assertEqual((root/'codex-fixture.json').read_text(), original)
            self.assertEqual(json.loads((root/'scale-seed.json').read_text())['codex'], 2)


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
            daemon = str(prefix/'versions/old/bin/cagentsd')
            before['processes'] += [
                {'pid':50,'start_ticks':2,'state':'S','executable':daemon,'argv':[daemon]},
                # Frozen builds run native actors and holders from the daemon ELF.
                {'pid':53,'start_ticks':5,'state':'S','executable':daemon,'argv':['cagents-native','--cagents-native-actor','47']},
                {'pid':54,'start_ticks':6,'state':'S','executable':daemon,'argv':['cagents-holder','--cagents-holder','48']},
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
                'reused-daemon-pid':lambda a:a['processes'][1].update(start_ticks=20),
                'daemon-became-holder':lambda a:a['processes'][1].update(argv=['cagents-holder','--cagents-holder','49']),
                'extra-daemon':lambda a:a['processes'].append({'pid':60,'start_ticks':20,'state':'S','executable':daemon,'argv':[daemon]}),
                'new-run':lambda a:a['agents']['agent'].update(run='new-run'),
                'missing-pane':lambda a:a['stores']['windows/main/sessions.sqlite3']['rows']['cards'].clear(),
                # A refusal leaves storage untouched, so even a closed ordinal hole is a change.
                'moved-pane':lambda a:a['stores']['windows/main/sessions.sqlite3']['rows']['cards'][0].update(ordinal=1),
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

            # The current installer's --stage-only selects the candidate before the
            # update; a refusal must keep that staged current and its deferred owner.
            (prefix/'versions/new').mkdir()
            (prefix/'current').unlink()
            (prefix/'current').symlink_to('versions/new')
            write(after,status)
            self.assertIn('refusal changed the installed current version',
                          scenario.expected_refusal(root,'interrupt','old'))
            (root/'staged-candidate.json').write_text(json.dumps({'current':str((prefix/'versions/new').resolve())}))
            owner = prefix/'versions/new/.deferred-owner.json'
            owner.write_text(json.dumps({'version':1,'owner_version':'old'}))
            self.assertEqual(scenario.expected_refusal(root,'interrupt','old'),[])
            owner.write_text(json.dumps({'version':1,'owner_version':'new'}))
            self.assertEqual(scenario.expected_refusal(root,'interrupt','old'),
                             ['refusal lost the deferred owner of the running service'])
            owner.unlink()
            (prefix/'current').unlink()
            (prefix/'current').symlink_to('versions/old')
            self.assertIn('refusal changed the installed current version',
                          scenario.expected_refusal(root,'interrupt','old'))

    def test_restart_presentation_rejects_third_owner_and_zombie_provider(self):
        spec = importlib.util.spec_from_file_location('rehearsal_restart',REPOSITORY/'scripts/rehearse_update_restart.py')
        restart = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(restart)
        root = Path('/var/tmp/cr/test')
        executable = root/'home/.local/share/cagents-install/versions/new/bin/cagentsd'
        proof = {'host_session_id':'new','boot_id':'boot','host_incarnation':'new-owner'}
        with patch.object(restart,'owned_daemon',return_value=50):
            restart.verify_same_daemon(root,executable,proof,50,proof)
            with self.assertRaisesRegex(RuntimeError,'ownership changed'):
                restart.verify_same_daemon(root,executable,proof,50,{**proof,'host_incarnation':'third-owner'})
        with patch.object(restart,'owned_daemon',return_value=51):
            with self.assertRaisesRegex(RuntimeError,'ownership changed'):
                restart.verify_same_daemon(root,executable,proof,50,proof)
        provider = {'pid':42,'start_ticks':123,'state':'S','argv':[str(root/'bin/claude')]}
        self.assertEqual(restart.provider_identities({'processes':[provider]},root),{(42,123)})
        for state in ('Z','X'):
            self.assertEqual(restart.provider_identities({'processes':[{**provider,'state':state}]},root),set())

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


class WindowCloseTracing(unittest.TestCase):
    @contextmanager
    def private_locked_process(self, sanitized=False):
        """A copied sleep models only process/lease ownership, never an application."""
        with tempfile.TemporaryDirectory(prefix='ru-window-trace-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            (root/'rt').mkdir()
            executable=root/'home/.local/share/cagents-install/versions/fixture/bin/cagents-ui'
            executable.parent.mkdir(parents=True)
            shutil.copyfile(shutil.which('sleep'),executable)
            executable.chmod(0o700)
            slot=root/'rt/cagents-ui.lock.primary'
            owner=slot.open('w')
            fcntl.flock(owner,fcntl.LOCK_EX)
            peer=subprocess.Popen([executable,'60'],pass_fds=(owner.fileno(),),
                env={} if sanitized else {**os.environ,'HOME':str(root/'home'),'XDG_RUNTIME_DIR':str(root/'rt')})
            try:
                fields=(Path('/proc')/str(peer.pid)/'stat').read_text().rsplit(')',1)[1].split()
                row={'pid':peer.pid,'start_ticks':int(fields[19]),'executable':str(executable),
                     'argv':[str(executable),'60']}
                before=root/'records/before/snapshot.json'
                before.parent.mkdir(parents=True)
                before.write_text(json.dumps({'processes':[row]}))
                yield root,slot,owner,peer,row
            finally:
                if peer.poll() is None:
                    peer.terminate()
                peer.wait(timeout=5)
                owner.close()

    def test_real_window_lease_observation_cannot_retain_or_acquire_the_lock(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            with records.WindowCloseMonitor(root) as monitor:
                first=json.loads((root/'window-close-trace.jsonl').read_text().splitlines()[1])
                sampled=next(item for item in first['snapshot']['processes'] if item['pid']==peer.pid)
                self.assertTrue(sampled['original_ui'])
                self.assertTrue(any('FLOCK' in item['fdinfo'] for item in sampled['slot_descriptors']))
                self.assertEqual(first['snapshot']['slots'][0]['inode'],slot.stat().st_ino)
                peer.terminate();peer.wait(timeout=5);owner.close()
                # This succeeds while the observer and its pidfd are still alive.
                with slot.open('r') as probe:
                    fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
                self.assertTrue(monitor.thread.is_alive())
                descriptors=list(monitor.pidfds)
            self.assertFalse(monitor.thread.is_alive())
            self.assertTrue(monitor.output.closed)
            for descriptor in descriptors:
                with self.assertRaises(OSError):os.fstat(descriptor)

    def test_original_slot_identity_survives_path_replacement(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            first=records.window_close_snapshot(root,[row])
            old=slot.stat().st_ino
            slot.unlink();slot.touch()
            self.assertNotEqual(slot.stat().st_ino,old)
            second=records.window_close_snapshot(root,[row],original_slots=first['slots'])
            process=next(item for item in second['processes'] if item['pid']==peer.pid)
            self.assertTrue(any(item['inode']==old and 'FLOCK' in item['fdinfo']
                                for item in process['slot_descriptors']))
            self.assertEqual(second['original_slots'][0]['inode'],old)
            self.assertEqual(second['slots'][0]['inode'],slot.stat().st_ino)

    def test_sanitized_inheritor_is_found_by_its_actual_private_slot_descriptor(self):
        with self.private_locked_process(sanitized=True) as (root,slot,owner,peer,row):
            self.assertNotIn(peer.pid,[item['pid'] for item in records.processes(root,require_seeded_child=False)])
            captured=records.window_close_snapshot(root,[])
            process=next(item for item in captured['processes'] if item['pid']==peer.pid)
            self.assertEqual(process['discovery'],'matching_private_slot_descriptor')
            self.assertTrue(any('FLOCK' in item['fdinfo'] for item in process['slot_descriptors']))

    def test_kernel_lock_identity_excludes_foreign_devices_and_binds_btrfs_bridge(self):
        slot={'device':os.makedev(1,2),'inode':123}
        local='1: FLOCK ADVISORY WRITE 42 01:02:123 0 EOF'
        foreign='2: FLOCK ADVISORY WRITE 43 01:03:123 0 EOF'
        btrfs='3: FLOCK ADVISORY WRITE 42 00:45:123 0 EOF'
        self.assertEqual(records.traced_kernel_locks([local,foreign,btrfs],[slot],[]),[local])
        descriptor={**slot,'fdinfo':'ino:\t123\nlock: 1: FLOCK ADVISORY WRITE 42 00:45:123 0 EOF'}
        self.assertEqual(records.traced_kernel_locks([local,foreign,btrfs],[slot],[descriptor]),[local,btrfs])
        descriptor['device']=os.makedev(1,9)
        self.assertEqual(records.traced_kernel_locks([local,foreign,btrfs],[slot],[descriptor]),[local])

    def test_reused_process_diagnostics_are_discarded(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            identity=(row['start_ticks'],row['executable'])
            original=records.trace_process_identity
            calls=[]
            def identity_read(path):
                if path==Path('/proc')/str(peer.pid):
                    calls.append(path)
                    return identity if len(calls)==1 else (identity[0]+1,identity[1])
                return original(path)
            with patch.object(records,'trace_process_identity',side_effect=identity_read):
                captured=records.window_close_snapshot(root,[row])
            process=next(item for item in captured['processes'] if item['pid']==peer.pid)
            self.assertEqual(process['state'],'ended_or_reused')
            self.assertNotIn('threads',process)
            self.assertNotIn('slot_descriptors',process)

    def test_reused_descriptor_is_not_attributed_to_the_slot(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            target=Path('/proc')/str(peer.pid)/'fd'/str(owner.fileno())
            original=Path.stat
            reads=[]
            def metadata(path,*args,**kwargs):
                value=original(path,*args,**kwargs)
                if path==target:
                    reads.append(path)
                    if len(reads)>1:
                        changed=list(value);changed[2]+=1
                        return os.stat_result(changed)
                return value
            with patch.object(Path,'stat',metadata):
                captured=records.window_close_snapshot(root,[row])
            process=next(item for item in captured['processes'] if item['pid']==peer.pid)
            self.assertGreaterEqual(len(reads),2)
            self.assertEqual(process['slot_descriptors'],[])

    def test_body_exception_cancels_and_joins_the_observer(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            ready=threading.Event()
            monitor=records.WindowCloseMonitor(root)
            def observe():
                ready.set()
                monitor.stop.wait(5)
            with patch.object(monitor,'observe',side_effect=observe):
                with self.assertRaisesRegex(RuntimeError,'update failed'):
                    with monitor:
                        self.assertTrue(ready.wait(2))
                        raise RuntimeError('update failed')
            self.assertFalse(monitor.thread.is_alive())
            self.assertTrue(monitor.stop.is_set())
            self.assertTrue(monitor.output.closed)
            events=[json.loads(line)['event'] for line in (root/'window-close-trace.jsonl').read_text().splitlines()]
            self.assertEqual(events[-1],'update_command_exited')

    def test_thread_start_failure_closes_every_pinned_pidfd(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            monitor=records.WindowCloseMonitor(root)
            descriptors=[]
            original_open=os.pidfd_open
            def opened(pid):
                descriptor=original_open(pid)
                descriptors.append(descriptor)
                return descriptor
            with patch.object(records.os,'pidfd_open',side_effect=opened), \
                    patch.object(threading.Thread,'start',side_effect=RuntimeError('thread start failed')):
                with self.assertRaisesRegex(RuntimeError,'thread start failed'):
                    monitor.__enter__()
            self.assertTrue(monitor.output.closed)
            self.assertTrue(monitor.stop.is_set())
            self.assertTrue(descriptors)
            for descriptor in descriptors:
                with self.assertRaises(OSError):os.fstat(descriptor)

    def test_interruption_during_exit_still_cancels_and_joins_before_reraising(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            ready,release=threading.Event(),threading.Event()
            monitor=records.WindowCloseMonitor(root)
            def observe():
                ready.set()
                monitor.stop.wait(5)
                release.wait(2)
            waited=monitor.finished.wait
            interruptions=[]
            def interrupted_wait(timeout):
                if not interruptions:
                    interruptions.append(True);release.set()
                    raise KeyboardInterrupt()
                return waited(timeout)
            with patch.object(monitor,'observe',side_effect=observe), \
                    patch.object(monitor.finished,'wait',side_effect=interrupted_wait):
                with self.assertRaises(KeyboardInterrupt):
                    with monitor:
                        self.assertTrue(ready.wait(2))
            self.assertTrue(monitor.finished.is_set())
            self.assertFalse(monitor.thread.is_alive())
            self.assertTrue(monitor.output.closed)
            self.assertEqual(monitor.pidfds,[])

    def test_failed_post_pin_identity_check_closes_and_excludes_the_pidfd(self):
        with self.private_locked_process() as (root,slot,owner,peer,row):
            monitor=records.WindowCloseMonitor(root)
            identity=(row['start_ticks'],row['executable'])
            opened=os.pidfd_open(peer.pid)
            with patch.object(records,'trace_process_identity',side_effect=[identity,(identity[0]+1,identity[1])]), \
                    patch.object(records.os,'pidfd_open',return_value=opened), \
                    patch.object(records,'window_close_snapshot',return_value={'slots':[]}):
                with monitor:
                    self.assertEqual(monitor.pidfds,[])
                    self.assertEqual(monitor.pinned,[])
                    with self.assertRaises(OSError):os.fstat(opened)
            begin=json.loads((root/'window-close-trace.jsonl').read_text().splitlines()[0])
            self.assertIn('pin_error',begin['pinned'][0])
            self.assertNotIn('pidfd_pinned',begin['pinned'][0])

    def test_phase_milestones_are_observed_without_asserting_commit_ack(self):
        with tempfile.TemporaryDirectory(prefix='ru-trace-timeline-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            status=root/'home/.local/share/cagents-install/.update/status.json'
            status.parent.mkdir(parents=True)
            status.write_text(json.dumps({'id':'job','phase':'ClosingWindows'}))
            monitor=records.WindowCloseMonitor(root)
            monitor.output=io.StringIO()
            clock=[0]
            reasons=[]
            def advance(_):
                clock[0]+=1
                if clock[0]>45:monitor.stop.set()
            with patch.object(records.time,'monotonic',side_effect=lambda:clock[0]), \
                    patch.object(monitor.stop,'wait',side_effect=advance), \
                    patch.object(monitor,'sample',side_effect=reasons.append):
                monitor.observe()
            for seconds in (1,5,15,40,45):
                self.assertIn(f'ClosingWindows+{seconds}s (not a commit ACK timestamp)',reasons)
            events=[json.loads(line) for line in monitor.output.getvalue().splitlines()]
            self.assertEqual(events[0]['event'],'phase')
            self.assertEqual(events[-1]['event'],'observer_stopped')

    def test_trace_size_and_private_root_bounds(self):
        with tempfile.TemporaryDirectory(prefix='ru-trace-bound-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            with self.assertRaises(FileNotFoundError):
                records.WindowCloseMonitor(root).__enter__()
            self.assertFalse((root/'window-close-trace.jsonl').exists())
            monitor=records.WindowCloseMonitor(root)
            monitor.output=io.StringIO()
            monitor.write({'event':'sample','snapshot':'x'*(512*1024)})
            self.assertEqual(json.loads(monitor.output.getvalue())['event'],'sample_truncated')
            monitor.bytes=16*1024*1024-1024
            monitor.write({'event':'sample','snapshot':'x'*2048})
            self.assertTrue(monitor.stop.is_set())
            size=len(monitor.output.getvalue())
            monitor.write({'event':'ignored_after_bound'})
            self.assertEqual(len(monitor.output.getvalue()),size)


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

    def test_idle_stop_review_uses_ordinary_consent_before_typed_stop(self):
        for answer in ('yes', 'no'):
            with self.subTest(answer=answer):
                result = self.run_peer(
                    "import sys,time; tty=open('/dev/tty','r'); "
                    "sys.stdout.write('Enter: review what stops'); sys.stdout.flush(); "
                    "time.sleep(.05); sys.stdout.write('   n: cancel\\n'); sys.stdout.flush(); "
                    f"assert tty.readline().strip() == {answer!r}; "
                    + ("sys.stdout.write('Type \\\"stop\\\" to stop these and update. Anything else cancels.\\n'); "
                       "sys.stdout.flush(); assert tty.readline().strip() == 'stop'; " if answer == 'yes' else '')
                    + "print('idle review answered')", '--answer', answer, '--working', 'i', '--stop', 'stop', '--timeout', '1')
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(result.stdout.count('[rehearsal prompt='), 2 if answer == 'yes' else 1)
                self.assertIn('idle review answered', result.stdout)

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


class WindowStallUnderLock(unittest.TestCase):
    """The lock-held stall stops a real writer inside its final_flush append."""

    WRITER = r"""
import fcntl, json, os, sys, time
log, lock = sys.argv[1:]
descriptor = os.open(lock, os.O_RDWR | os.O_CREAT, 0o600)
sys.stdin.read(1)
fcntl.flock(descriptor, fcntl.LOCK_EX)
with open(log, 'a') as stream:
    stream.write(json.dumps({'pid': os.getpid(), 'at_unix_ms': 1,
                             'event': {'kind': 'final_flush', 'complete': True}}) + '\n')
time.sleep(.05)  # the append's sync_data and directory sync
fcntl.flock(descriptor, fcntl.LOCK_UN)
time.sleep(30)
"""

    def test_stop_lands_while_the_window_holds_its_diagnostics_lock(self):
        with tempfile.TemporaryDirectory(prefix='ru-lock-', dir='/var/tmp') as directory:
            root = Path(directory)
            diagnostics = root/'home/.local/state/cagents/diagnostics'
            diagnostics.mkdir(parents=True)
            log, lock = diagnostics/'cagents-ui.0.jsonl', diagnostics/'cagents-ui.lock'
            log.write_text('')
            writer = subprocess.Popen([sys.executable, '-I', '-c', self.WRITER, str(log), str(lock)],
                                      stdin=subprocess.PIPE, text=True)
            self.addCleanup(writer.wait)
            self.addCleanup(writer.kill)
            descriptor = os.pidfd_open(writer.pid)
            self.addCleanup(os.close, descriptor)
            monitor = records.WindowCloseMonitor(root, 'main', under_lock=True)
            monitor.pinned, monitor.pidfds = [{'pid': writer.pid}], [descriptor]
            monitor.originals_by_pin = [{'argv': ['cagents-ui']}]
            events = []
            monitor.write = events.append
            stall = threading.Thread(target=monitor.stall_after_flush)
            stall.start()
            time.sleep(.1)
            writer.stdin.write('x')
            writer.stdin.flush()
            stall.join(10)
            self.assertFalse(stall.is_alive())
            [event] = events
            self.assertEqual(event['event'], 'stalled_window_after_flush')
            self.assertTrue(event['diagnostics_lock_held_before_stop'])
            self.assertTrue(event['stopped_under_lock'], event)
            self.assertEqual(records.WindowCloseMonitor.flock_holder(lock), writer.pid)
            self.assertFalse(records.WindowCloseMonitor.diagnostics_unlocked(lock))


if __name__ == "__main__":
    unittest.main()
