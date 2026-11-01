#!/usr/bin/env python3
"""Offline fixture contract only; this does not establish native UI acceptance."""

import json
import os
from pathlib import Path
import runpy
import socket
import subprocess
import sys
import tempfile
import threading
import unittest


FIXTURE = Path(__file__).resolve().parent / "fixtures/chat-chronology-claude.py"
SESSION = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
CODEX = runpy.run_path(str(FIXTURE.with_name("chat-chronology-codex.py")))


class ChatChronologyFixture(unittest.TestCase):
    def run_turns(self, root, numbers, resume=False):
        config = root / "account"
        config.mkdir(exist_ok=True)
        (config / "fixture-chronology-delay").write_text("0")
        project = root / "project"
        project.mkdir(exist_ok=True)
        frames = [{"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}}]
        frames.extend({"type": "user", "uuid": f"user-{number}", "message": {
            "role": "user", "content": [{"type": "text", "text": "." if number > 1 else "Check the saved state."}]}}
            for number in numbers)
        result = subprocess.run(
            [sys.executable, str(FIXTURE), "--resume" if resume else "--session-id", SESSION],
            cwd=project, env=dict(os.environ, HOME=str(root / "home"), CLAUDE_CONFIG_DIR=str(config)),
            input="".join(json.dumps(frame) + "\n" for frame in frames),
            text=True, capture_output=True, timeout=10, check=True)
        return [json.loads(line) for line in result.stdout.splitlines()]

    def test_distinct_identical_failures_keep_exact_history_chain_across_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = self.run_turns(root, (1, 2))
            resumed = self.run_turns(root, (3, 4), resume=True)
            records = [json.loads(line) for line in next((root / "account/projects").glob("*/*.jsonl")).read_text().splitlines()]
            failures = [row for row in records if row.get("error") == "overloaded_error"]
            self.assertEqual([row["uuid"] for row in failures], ["chronology-capacity-1", "chronology-capacity-3"])
            self.assertEqual(failures[0]["message"]["content"], failures[1]["message"]["content"])
            self.assertIn("Original provider diagnostics remain available.", failures[0]["message"]["content"][0]["text"])
            for rows, number in ((first, 1), (resumed, 3)):
                self.assertEqual(sum(row.get("uuid") == f"chronology-capacity-{number}" for row in rows), 2)
                self.assertEqual(sum(row.get("uuid") == f"chronology-result-{number}" for row in rows), 2)
            self.assertEqual([row["uuid"] for row in records if row.get("fixture_prompt")], [f"user-{number}" for number in range(1, 5)])
            self.assertIsNone(records[0]["parentUuid"])
            self.assertEqual(records[0]["cwd"], str((root / "project").resolve()))
            for previous, current in zip(records, records[1:]):
                self.assertEqual(current["parentUuid"], previous["uuid"])
                self.assertEqual(current["sessionId"], SESSION)

    def test_recovery_exposes_real_deltas_before_aggregate_and_records_tool_result(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = self.run_turns(root, (1, 2))
            start = next(index for index, row in enumerate(rows) if row.get("uuid") == "user-2")
            recovery = rows[start:]
            final = next(index for index, row in enumerate(recovery) if row.get("uuid") == "chronology-recovery-2")
            deltas = [row["event"]["delta"]["text"] for row in recovery[:final]
                      if row.get("event", {}).get("delta", {}).get("type") == "text_delta"]
            self.assertEqual(deltas, ["Recovery resumed. ", "I am checking the saved state. ",
                                      "The earlier failure should remain above your follow-up."])
            self.assertEqual("".join(deltas), recovery[final]["message"]["content"][0]["text"])
            streamed_input = "".join(row["event"]["delta"]["partial_json"] for row in recovery
                                     if row.get("event", {}).get("delta", {}).get("type") == "input_json_delta")
            tool = next(row for row in recovery if row.get("uuid") == "chronology-tool-message-2")
            self.assertEqual(json.loads(streamed_input), tool["message"]["content"][0]["input"])
            result = next(row for row in recovery if row.get("uuid") == "chronology-tool-result-2")
            self.assertEqual(result["message"]["content"][0]["tool_use_id"], tool["message"]["content"][0]["id"])
            self.assertFalse(recovery[-1]["is_error"])
            self.assertEqual(recovery[-1]["subtype"], "success")


class CodexChronologyFixture(unittest.TestCase):
    def fixture(self, root):
        home = root / "codex-home"
        home.mkdir(mode=0o700, exist_ok=True)
        (home / "fixture-chronology-enabled").touch()
        return CODEX["Fixture"](home, root, delay=0)

    def test_terminal_failures_and_exact_history_reload_keep_distinct_turn_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = self.fixture(root)
            rows = []
            def send(value):
                rows.append(json.loads(json.dumps(value)))
            fixture.request({"id": 1, "method": "thread/start", "params": {}}, send)
            thread = rows[-1]["result"]["thread"]["id"]
            for number, prompt in enumerate(("Check the saved state.", ".", "Try the check again.", "Continue."), 2):
                fixture.request({"id": number, "method": "turn/start", "params": {
                    "threadId": thread, "input": [{"type": "text", "text": prompt}]}}, send)
            errors = [row["params"] for row in rows if row.get("method") == "error"]
            self.assertEqual([error["turnId"] for error in errors], ["chronology-turn-1"] * 2 + ["chronology-turn-3"] * 2)
            self.assertEqual(len({error["error"]["message"] for error in errors}), 1)
            self.assertEqual(errors[0]["error"]["message"].splitlines()[0],
                             "Selected model is at capacity. Please try a different model.")
            self.assertTrue(all(error["willRetry"] is False for error in errors))
            recovered = [row for row in rows if row.get("params", {}).get("turnId") == "chronology-turn-2"]
            aggregate = next(index for index, row in enumerate(recovered) if row.get("method") == "item/completed"
                             and row["params"]["item"]["id"] == "answer")
            self.assertEqual("".join(row["params"]["delta"] for row in recovered[:aggregate]
                                     if row.get("method") == "item/agentMessage/delta"),
                             recovered[aggregate]["params"]["item"]["text"])
            self.assertEqual([row["params"]["delta"] for row in recovered
                              if row.get("method") == "item/commandExecution/outputDelta"],
                             ["Synthetic recovery ", "check passed\n"])
            restored = self.fixture(root)
            restored.request({"id": 8, "method": "thread/resume", "params": {"threadId": thread}}, send)
            self.assertEqual(rows[-1]["result"]["thread"]["id"], thread)
            page = rows[-1]["result"]["initialTurnsPage"]["data"]
            self.assertEqual([turn["id"] for turn in page], [f"chronology-turn-{number}" for number in (4, 3, 2, 1)])
            self.assertEqual([turn["status"] for turn in page], ["completed", "failed", "completed", "failed"])
            self.assertEqual(page[1]["error"], page[3]["error"])
            restored.request({"id": 9, "method": "thread/turns/list", "params": {"threadId": thread}}, send)
            self.assertEqual(rows[-1]["result"]["data"], page)
            catalog = next((root / "codex-home/sessions").glob("rollout-*.jsonl")).read_text().splitlines()
            self.assertIn('"type":"session_meta"', catalog[0])
            self.assertEqual(json.loads(catalog[0])["payload"]["id"], thread)
            self.assertEqual(json.loads(catalog[1])["payload"]["message"], "Check the saved state.")

    def test_private_enable_marker_is_required_and_wrong_resume_cannot_take_over(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, "synthetic CODEX_HOME"):
                CODEX["Fixture"](root, root, delay=0)
            self.assertFalse((root / "fixture-chronology-history.json").exists())
            fixture = self.fixture(root)
            rows = []
            fixture.request({"id": 1, "method": "thread/resume", "params": {"threadId": "some-other-thread"}}, rows.append)
            self.assertEqual(rows[0]["error"]["message"], "fixture requires the exact saved thread")
            self.assertFalse(fixture.path.exists())

    def test_actual_websocket_handshake_and_request_share_the_saved_thread(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = self.fixture(Path(directory))
            reader, writer = socket.socketpair()
            reader.settimeout(3)
            writer.settimeout(3)
            failures = []
            def worker():
                try:
                    CODEX["serve"](writer, fixture)
                except EOFError:
                    pass
                except BaseException as error:
                    failures.append(error)
                finally:
                    writer.close()
            thread = threading.Thread(target=worker)
            thread.start()
            try:
                reader.sendall(b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
                header = bytearray()
                while b"\r\n\r\n" not in header:
                    header.extend(reader.recv(1))
                self.assertIn(b"101 Switching Protocols", header)
                self.assertIn(b"Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", header)
                reader.sendall(CODEX["WIRE"]["frame"]({"id": 3, "method": "thread/start", "params": {}}))
                reply = CODEX["WIRE"]["receive"](reader, lambda value: None)
                self.assertEqual(reply["result"]["thread"]["id"], fixture.data["threadId"])
                self.assertEqual(reply["result"]["approvalPolicy"], "on-request")
                self.assertEqual(reply["result"]["sandbox"]["type"], "workspaceWrite")
            finally:
                reader.close()
                thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            if failures:
                raise failures[0]


if __name__ == "__main__":
    unittest.main()
