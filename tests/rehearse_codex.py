#!/usr/bin/env python3
"""Focused offline checks for the update rehearsal's Codex compatibility peer."""
import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import threading
import unittest
import uuid
from unittest import mock



FIXTURES = Path(__file__).resolve().parent / "fixtures"


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, FIXTURES / filename)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


adapter = module("rehearsal_codex", "rehearsal-codex.py")
peer = module("rehearsal_peer", "codex-tool-rows.py")


class ConfigurationTests(unittest.TestCase):
    def test_private_configuration_restores_existing_peer_knobs(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ):
            root = Path(directory)
            (root / "codex-fixture.json").write_text(json.dumps({
                "pause": 90, "never_confirm_interrupt": True, "never_confirm_close": False}))
            adapter.configure_peer(root)
            self.assertEqual(os.environ["CAGENTS_TOOL_ROWS_PAUSE"], "90")
            scenario = peer.Scenario()
            self.assertTrue(scenario.never_confirm_interrupt)
            self.assertFalse(scenario.never_confirm_close)
            configured = module("configured_rehearsal_peer", "codex-tool-rows.py").Scenario()
            replies = configured.request({'id':1,'method':'turn/start','params':{
                'threadId':configured.thread}},str(root))
            self.assertIn({'_sleep':90},replies)

    def test_history_opens_only_the_first_turn_with_earlier_messages(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ):
            root = Path(directory)
            (root / "codex-fixture.json").write_text(json.dumps({"pause": 1, "history": [5, 200]}))
            adapter.configure_peer(root)
            self.assertEqual(os.environ["CAGENTS_TOOL_ROWS_HISTORY"], "5 200")
            scenario = peer.Scenario()
            def history(turn):
                replies = scenario.notifications(turn)
                completed = next(reply for reply in replies if reply.get("method") == "turn/completed"
                                 and reply["params"]["threadId"] == scenario.thread)
                return [item for item in completed["params"]["turn"]["items"]
                        if item["id"].startswith("history-")], replies
            first, replies = history("first")
            self.assertEqual([item["id"] for item in first], [f"history-{at}" for at in range(5)])
            self.assertTrue(all(len(item["text"]) == len(f"Step {at}: ") + 200
                                for at, item in enumerate(first)))
            started = [reply["params"]["item"]["id"] for reply in replies
                       if reply.get("method") == "item/completed"][:5]
            self.assertEqual(started, [item["id"] for item in first])
            self.assertEqual(history("second")[0], [])

    def test_rejects_unknown_invalid_and_symlink_configuration(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ):
            root = Path(directory)
            path = root / "codex-fixture.json"
            for settings in ({"arbitrary_env": "value"}, {"pause": True}, {"pause": 0},
                             {"pause": 301}, {"never_confirm_close": "1"}, {"history": 5},
                             {"history": [5]}, {"history": [5, -1]}, {"history": [5, True]}):
                path.write_text(json.dumps(settings))
                with self.subTest(settings=settings), self.assertRaises(ValueError):
                    adapter.configure_peer(root)
            path.unlink()
            target = root / "other.json"
            target.write_text('{}')
            path.symlink_to(target)
            with self.assertRaises(OSError):
                adapter.configure_peer(root)


class ListenerTests(unittest.TestCase):
    def test_accepts_private_runtime_and_old_build_convention(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            path = runtime / "control.sock"
            self.assertEqual(adapter.listener_path(["--listen", f"unix://{path}"], runtime), path)
            old = Path("/tmp") / f"cagents-codex-{uuid.uuid4()}"
            old.mkdir(mode=0o700)
            try:
                path = old / "control.sock"
                self.assertEqual(adapter.listener_path([f"--listen=unix://{path}"], runtime), path)
            finally:
                old.rmdir()

    def test_rejects_public_symlink_existing_and_unrelated_sockets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / "runtime"
            runtime.mkdir(mode=0o700)
            public = runtime / "public"
            public.mkdir(mode=0o755)
            unrelated = root / "other"
            unrelated.mkdir(mode=0o700)
            link = runtime / "link"
            link.symlink_to(unrelated, target_is_directory=True)
            existing = runtime / "control.sock"
            existing.touch()
            for path in (public / "control.sock", unrelated / "control.sock",
                         link / "control.sock", existing, runtime / "other.sock"):
                with self.subTest(path=path), self.assertRaises(ValueError):
                    adapter.listener_path(["--listen", f"unix://{path}"], runtime)


class PersistenceTests(unittest.TestCase):
    def start_paused(self, scenario, home):
        replies = scenario.request({"id": 2, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, str(home))
        generations = scenario.playback(replies)
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        scenario.play(replies[:pause], lambda _: None, threading.Lock(), generations)
        return replies[pause + 1:], generations

    def interrupt(self, scenario, thread, home):
        replies = scenario.request({"id": 3, "method": "turn/interrupt", "params": {
            "threadId": thread, "turnId": scenario._latest(thread)["id"]}}, str(home))
        scenario.play(replies, lambda _: None, threading.Lock(), scenario.playback(replies))

    def test_resume_restores_exact_thread_and_history_across_listener_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            first.request({"id": 1, "method": "thread/start"}, directory)
            first.request({"id": 2, "method": "turn/start", "params": {"threadId": first.thread}}, directory)
            restarted = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.assertEqual(restarted.state(), first.state())

            successor = adapter.scenario_class(peer, storage, Path("/second/control.sock"))()
            self.assertNotEqual(successor.thread, first.thread)
            resumed = successor.request({"id": 3, "method": "thread/resume",
                                         "params": {"threadId": first.thread}}, directory)[0]["result"]
            self.assertEqual(resumed["thread"]["id"], first.thread)
            self.assertEqual(resumed["initialTurnsPage"]["data"], [first.latest])
            self.assertEqual(successor.child, first.child)

    def test_observed_completion_is_saved_without_a_later_request(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            remaining, generations = self.start_paused(first, directory)
            first.play(remaining, lambda _: None, threading.Lock(), generations)
            restarted = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.assertEqual(restarted.state(), first.state())
            for thread in (first.thread, first.child):
                resumed = restarted.request({"id": 4, "method": "thread/resume", "params": {
                    "threadId": thread}}, directory)[0]["result"]
                self.assertEqual(resumed["thread"]["status"]["type"], "idle")
                self.assertEqual(resumed["initialTurnsPage"]["data"][0]["status"], "completed")

    def test_interrupted_parent_and_running_child_survive_listener_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            remaining, generations = self.start_paused(first, directory)
            self.interrupt(first, first.thread, directory)
            replacement = adapter.scenario_class(peer, storage, Path("/second/control.sock"))()
            parent = replacement.request({"id": 4, "method": "thread/resume", "params": {
                "threadId": first.thread}}, directory)[0]["result"]
            self.assertEqual(parent["initialTurnsPage"]["data"][0]["status"], "interrupted")
            child = replacement.request({"id": 5, "method": "thread/resume", "params": {
                "threadId": first.child}}, directory)[0]["result"]
            self.assertEqual(child["thread"]["status"]["type"], "active")
            self.assertEqual(child["initialTurnsPage"]["data"][0]["status"], "inProgress")
            # Explicitly stop the child in the still-owning original listener;
            # queued frames then cannot overwrite either terminal outcome.
            self.interrupt(first, first.child, directory)
            first.play(remaining, lambda _: None, threading.Lock(), generations)
            final = adapter.scenario_class(peer, storage, Path("/third/control.sock"))()
            final.request({"id": 6, "method": "thread/resume", "params": {
                "threadId": first.thread}}, directory)
            self.assertEqual(final.latest["status"], "interrupted")
            self.assertEqual(final.child_latest["status"], "interrupted")
            states = [item["agentsStates"][final.child]["status"] for item in final.latest["items"]
                      if item["type"] == "collabAgentToolCall"]
            self.assertEqual(states, ["interrupted", "interrupted"])

    def test_unconfirmed_interrupt_survives_listener_replacement(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ, {
                "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT": "1"}):
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            remaining, _ = self.start_paused(first, directory)
            self.interrupt(first, first.thread, directory)
            replacement = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.assertEqual(replacement.state(), first.state())
            sent = []
            replacement.play(remaining, sent.append, threading.Lock(), replacement.playback(remaining))
            self.assertEqual(replacement.latest["status"], "inProgress")
            self.assertFalse(any(frame.get("method") == "turn/completed"
                                 and frame["params"]["threadId"] == first.thread for frame in sent))

    def test_loading_named_history_fences_playback_of_the_previous_allocation(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            remaining, generations = self.start_paused(first, directory)
            other = adapter.scenario_class(peer, storage, Path("/other/control.sock"))()
            self.start_paused(other, directory)
            self.interrupt(other, other.thread, directory)
            self.interrupt(other, other.child, directory)
            first.request({"id": 4, "method": "thread/resume", "params": {
                "threadId": other.thread}}, directory)
            sent = []
            first.play(remaining, sent.append, threading.Lock(), generations)
            self.assertEqual(sent, [])
            self.assertEqual(first.state(), other.state())

    def test_legacy_parent_only_state_does_not_invent_a_child_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.start_paused(first, directory)
            legacy = first.state()
            del legacy["child_latest"]
            for path in storage.glob("*.json"):
                path.write_text(json.dumps(legacy))
            restarted = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.assertEqual(restarted.latest, first.latest)
            self.assertIsNone(restarted.child_latest)

    def test_probe_listener_does_not_replace_conversation_state(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            conversation = adapter.scenario_class(peer, storage, Path("/chat/control.sock"))()
            conversation.request({"id": 1, "method": "thread/start"}, directory)
            probe = adapter.scenario_class(peer, storage, Path("/probe/control.sock"))()
            probe.request({"id": 2, "method": "account/read"}, directory)
            resumed = adapter.scenario_class(peer, storage, Path("/chat/control.sock"))()
            self.assertEqual(resumed.thread, conversation.thread)
            self.assertNotEqual(resumed.thread, probe.thread)


class StdioProbeTests(unittest.TestCase):
    def test_account_probe_answers_without_stderr_or_persisted_conversation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, runtime = root / "home", root / "rt"
            home.mkdir(mode=0o700)
            runtime.mkdir(mode=0o700)
            frames = [{"id": 1, "method": "initialize", "params": {}},
                      {"method": "initialized"},
                      {"id": 2, "method": "account/read", "params": {}},
                      {"id": 3, "method": "account/rateLimits/read"},
                      {"id": 4, "method": "model/list", "params": {"limit": 100}}]
            result = subprocess.run([sys.executable, "-I", str(FIXTURES / "rehearsal-codex.py"), "app-server"],
                                    env={"PATH": "/usr/bin:/bin", "HOME": str(home),
                                         "CAGENTS_TOOL_ROWS_FIXTURE_ROOT": str(root),

                                         "CODEX_HOME": str(home), "XDG_RUNTIME_DIR": str(runtime)},
                                    input="".join(json.dumps(frame) + "\n" for frame in frames),
                                    text=True, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            responses = {frame["id"]: frame["result"] for frame in map(json.loads, result.stdout.splitlines())}
            self.assertEqual(set(responses), {1, 2, 3, 4})
            self.assertEqual(responses[2]["account"]["email"], "fixture@invalid")
            self.assertTrue(responses[3]["ordinaryUsageAllowed"])
            self.assertFalse((home / "rehearsal-app-server").exists())
            events = list(map(json.loads, (root / "logs/codex-provider.jsonl").read_text().splitlines()))
            self.assertEqual([event["method"] for event in events if event["event"] == "probe-request"],
                             [frame["method"] for frame in frames])


if __name__ == "__main__":
    unittest.main()
