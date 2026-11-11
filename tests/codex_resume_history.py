#!/usr/bin/python3
"""Codex rollout producer contracts; optional real-provider parser proof.

Set CAGENTS_TEST_CODEX to an absolute native Codex executable to exercise
thread/resume and thread/turns/list against it. This never starts a model turn.
"""
import json
import os
from pathlib import Path
import queue
import runpy
import signal
import subprocess
import tempfile
import threading
import time
import unittest
from unittest import mock


FIXTURE = runpy.run_path(str(Path(__file__).resolve().parent / "fixtures/codex-resume-history.py"))


class CodexResumeHistory(unittest.TestCase):
    def test_every_user_message_belongs_to_one_complete_turn(self):
        with tempfile.TemporaryDirectory(prefix="codex-history-") as directory:
            result = FIXTURE["seed"](directory, 7)
            rows = [json.loads(line) for line in Path(result["rollout"]).read_text().splitlines()]
            active = None
            completed = []
            messages = []
            history_events = []
            for row in rows:
                kind, payload = row["type"], row["payload"]
                if kind == "event_msg" and payload["type"] == "task_started":
                    self.assertIsNone(active)
                    active = payload["turn_id"]
                    messages = []
                    history_events = []
                elif kind == "response_item":
                    self.assertIsNotNone(active, "fixture messages must follow their task_started")
                    messages.append(payload["role"])
                elif kind == "event_msg" and payload["type"] in ("user_message", "agent_message"):
                    self.assertIsNotNone(active)
                    history_events.append(payload["type"])
                elif kind == "event_msg" and payload["type"] == "item_completed":
                    self.assertEqual(payload["thread_id"], result["thread_id"])
                    self.assertEqual(payload["turn_id"], active)
                elif kind == "event_msg" and payload["type"] == "task_complete":
                    self.assertEqual(payload["turn_id"], active)
                    self.assertEqual(messages, ["user", "assistant"])
                    self.assertEqual(history_events, ["user_message", "agent_message"])
                    completed.append(active)
                    active = None
            self.assertIsNone(active)
            self.assertEqual(len(set(completed)), 7)
            self.assertTrue(Path(result["codex_home"]).is_relative_to(Path(directory)))
            self.assertTrue(Path(result["working_directory"]).is_dir())

    def test_rerun_cannot_overwrite_a_provider_home(self):
        with tempfile.TemporaryDirectory(prefix="codex-history-") as directory:
            result = FIXTURE["seed"](directory)
            path = Path(result["rollout"])
            before = path.read_bytes()
            with self.assertRaises(FileExistsError):
                FIXTURE["seed"](directory)
            self.assertEqual(path.read_bytes(), before)

    def test_account_home_is_refused_before_any_output(self):
        with tempfile.TemporaryDirectory(prefix="codex-history-") as directory:
            root = Path(directory)
            with mock.patch.object(FIXTURE["pwd"], "getpwuid",
                                   return_value=mock.Mock(pw_dir=str(root / "home"))):
                with self.assertRaisesRegex(ValueError, "real Codex home"):
                    FIXTURE["seed"](root)
            self.assertFalse((root / "home").exists())

    @unittest.skipUnless(os.environ.get("CAGENTS_TEST_CODEX"), "set CAGENTS_TEST_CODEX for real Codex proof")
    def test_real_codex_reconstructs_framed_turns_and_omits_the_old_fixture(self):
        binary = Path(os.environ["CAGENTS_TEST_CODEX"])
        self.assertTrue(binary.is_absolute() and binary.is_file(), "name an absolute native Codex executable")
        for framing, expected in [("framed", 3), ("missing-starts", 3), ("old-fixture", 0)]:
            with self.subTest(framing=framing), tempfile.TemporaryDirectory(prefix="codex-history-") as directory:
                result = FIXTURE["seed"](directory)
                home = Path(result["codex_home"])
                # An unreachable local-only model endpoint and no credentials:
                # model traffic would fail instead of reaching a live service.
                (home / "config.toml").write_text('''model = "fixture-model"
model_provider = "fixture"
[model_providers.fixture]
name = "Offline fixture"
base_url = "http://127.0.0.1:1"
wire_api = "responses"
requires_openai_auth = false
''')
                path = Path(result["rollout"])
                rows = [json.loads(line) for line in path.read_text().splitlines()]
                # Newer Codex refuses a thread from a different model
                # provider, even when resuming without starting a turn.
                rows[0]["payload"]["model_provider"] = "fixture"
                turn_ids = {row["payload"]["turn_id"] for row in rows
                            if row["type"] == "event_msg" and row["payload"]["type"] == "task_started"}
                if framing != "framed":
                    omitted = {"task_started"}
                    if framing == "old-fixture":
                        # The old generator had model response_items and turn
                        # item_completed records, but neither turn starts nor
                        # the message events consumed by Codex's history API.
                        omitted.update({"user_message", "agent_message"})
                    rows = [row for row in rows if not (row["type"] == "event_msg"
                                                       and row["payload"]["type"] in omitted)]
                path.write_text("".join(json.dumps(row) + "\n" for row in rows))
                with tempfile.TemporaryFile(mode="w+") as errors:
                    child = subprocess.Popen([str(binary), "app-server"], env={
                        "PATH": "/usr/bin:/bin", "HOME": str(Path(directory) / "home"),
                        "CODEX_HOME": str(home), "XDG_CONFIG_HOME": str(Path(directory) / "config"),
                        "XDG_DATA_HOME": str(Path(directory) / "data"),
                        "XDG_CACHE_HOME": str(Path(directory) / "cache"),
                    }, cwd=result["working_directory"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                       stderr=errors, text=True, start_new_session=True)
                    replies = queue.Queue()

                    def read():
                        try:
                            for line in child.stdout:
                                replies.put(json.loads(line))
                        except BaseException as error:
                            replies.put(error)
                        finally:
                            replies.put(None)

                    reader = threading.Thread(target=read)
                    reader.start()

                    def send(frame):
                        child.stdin.write(json.dumps(frame) + "\n")
                        child.stdin.flush()

                    def call(identifier, method, params):
                        send({"id": identifier, "method": method, "params": params})
                        deadline = time.monotonic() + 15
                        while True:
                            remaining = deadline - time.monotonic()
                            self.assertGreater(remaining, 0, "Codex did not reply before the deadline")
                            response = replies.get(timeout=remaining)
                            if isinstance(response, BaseException):
                                raise response
                            self.assertIsNotNone(response, "Codex exited before replying")
                            if response.get("id") == identifier:
                                self.assertNotIn("error", response, response)
                                return response["result"]

                    try:
                        call(1, "initialize", {"clientInfo": {"name": "cagents-fixture-test", "version": "1"},
                                               "capabilities": {"experimentalApi": True}})
                        send({"method": "initialized", "params": {}})
                        resumed = call(2, "thread/resume", {"threadId": result["thread_id"],
                            "excludeTurns": True, "initialTurnsPage": {"limit": 20, "sortDirection": "desc", "itemsView": "full"}})
                        self.assertEqual(resumed["thread"]["id"], result["thread_id"])
                        page = call(3, "thread/turns/list", {"threadId": result["thread_id"],
                                    "limit": 20, "sortDirection": "desc", "itemsView": "full"})
                        self.assertEqual(len(page["data"]), expected, page)
                        if framing != "old-fixture":
                            self.assertTrue(all(turn["status"] == "completed" for turn in page["data"]))
                            items = [item for turn in page["data"] for item in turn["items"]]
                            self.assertEqual(sum(item["type"] == "userMessage" for item in items), 3, page)
                            self.assertEqual(sum(item["type"] == "agentMessage" for item in items), 3, page)
                            self.assertEqual({item["content"][0]["text"] for item in items
                                              if item["type"] == "userMessage"},
                                             {f"Check the resume fixture, turn {number}." for number in range(1, 4)})
                            self.assertEqual({item["text"] for item in items if item["type"] == "agentMessage"},
                                             {f"Resume fixture turn {number} is preserved." for number in range(1, 4)})
                            if framing == "framed":
                                self.assertEqual({turn["id"] for turn in page["data"]}, turn_ids)
                                self.assertTrue(all(turn["startedAt"] is not None for turn in page["data"]))
                            else:
                                self.assertTrue(turn_ids.isdisjoint(turn["id"] for turn in page["data"]))
                                self.assertTrue(all(turn["startedAt"] is None for turn in page["data"]))
                    finally:
                        try:
                            os.killpg(child.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                        try:
                            child.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            try:
                                os.killpg(child.pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                            child.wait(timeout=5)
                        reader.join(timeout=5)
                        self.assertFalse(reader.is_alive())
                        child.stdin.close()
                        child.stdout.close()


if __name__ == "__main__":
    unittest.main()
