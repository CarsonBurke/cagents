#!/usr/bin/python3
"""Contract for the synthetic Claude provider's start-up and exit switches,
run directly: no app, daemon or real provider."""

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parent / "fixtures/chat-composer-claude.sh"


def prompt(number):
    return {"type": "user", "uuid": f"prompt-{number}", "origin": {"kind": "human"},
            "message": {"role": "user", "content": "Run the check."}}


class Provider(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="chat-provider-contract-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.config = self.root / "home/.claude"
        self.config.mkdir(parents=True)
        self.project = self.root / "project"
        self.project.mkdir()

    def run_provider(self, *arguments, frames=(prompt(1),)):
        started = time.monotonic()
        result = subprocess.run(
            [str(SCRIPT), *arguments], cwd=self.project, capture_output=True, text=True,
            input="".join(json.dumps(frame) + "\n" for frame in frames), timeout=60,
            env={"HOME": str(self.root / "home"), "CLAUDE_CONFIG_DIR": str(self.config),
                 "PATH": "/usr/local/bin:/usr/bin:/bin"})
        output = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        return result, output, time.monotonic() - started

    def frames(self, *lines):
        (self.config / "fixture-frames.jsonl").write_text(
            "".join(json.dumps(line) + "\n" for line in lines))

    def texts(self, output):
        return [block.get("text") for frame in output if frame.get("type") == "assistant"
                for block in frame["message"]["content"] if block.get("type") == "text"]

    def test_a_resumed_transcript_with_damaged_lines_is_read_past_them(self):
        session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        mangled = "".join(c if c.isalnum() else "-" for c in str(self.project.resolve()))
        history = self.config / "projects" / mangled / f"{session}.jsonl"
        history.parent.mkdir(parents=True)
        good = {**prompt(0), "sessionId": session, "parentUuid": None}
        # A record that parses, one that doesn't, a value that isn't a record,
        # a record without content, and a last line a crash cut short.
        history.write_text(json.dumps(good) + "\n" + '{"type": "assistant", "uuid": \n'
                           + "42\n" + json.dumps({"type": "user", "uuid": "bare"}) + "\n"
                           + '{"type": "user", "uuid": "cut')
        result, output, _ = self.run_provider("--resume", session)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any(frame.get("type") == "result" for frame in output), output)
        lines = history.read_text().splitlines()
        self.assertEqual(lines[4], '{"type": "user", "uuid": "cut')
        appended = [json.loads(line) for line in lines[5:]]
        self.assertTrue(appended, lines)
        # The fixture goes on from the last record it could read.
        self.assertEqual(appended[0]["parentUuid"], "bare")

    def test_a_transcript_ending_in_nuls_gets_its_next_record_on_a_line_of_its_own(self):
        # A crash can leave a file's last block zero-filled.
        session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        mangled = "".join(c if c.isalnum() else "-" for c in str(self.project.resolve()))
        history = self.config / "projects" / mangled / f"{session}.jsonl"
        history.parent.mkdir(parents=True)
        history.write_bytes(json.dumps({**prompt(0), "sessionId": session}).encode() + b"\n" + b"\0" * 64)
        result, _, _ = self.run_provider("--resume", session)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = history.read_bytes().split(b"\n")
        self.assertEqual(lines[1], b"\0" * 64)
        self.assertEqual(json.loads(lines[2])["parentUuid"], "prompt-0")

    def test_an_exit_line_ends_the_provider_mid_turn_without_a_result(self):
        self.frames({"type": "assistant", "uuid": "partial-@TURN@",
                     "message": {"id": "partial-@TURN@", "role": "assistant",
                                 "content": [{"type": "text", "text": "Starting the migration."}]}},
                    {"_exit": 3},
                    {"type": "assistant", "uuid": "never", "message": {
                        "id": "never", "role": "assistant", "content": [{"type": "text", "text": "never"}]}})
        result, output, _ = self.run_provider()
        self.assertEqual(result.returncode, 3)
        self.assertIn("exited with status 3 mid-turn", result.stderr)
        self.assertEqual(self.texts(output), ["Starting the migration."])
        self.assertFalse(any(frame.get("type") == "result" for frame in output))
        # Every turn that reaches it exits again.
        again, _, _ = self.run_provider()
        self.assertEqual(again.returncode, 3)

    def test_an_exit_line_once_lets_the_resumed_provider_go_on(self):
        self.frames({"_exit": 1, "_once": True})
        first, _, _ = self.run_provider()
        self.assertEqual(first.returncode, 1)
        self.assertTrue((self.config / "fixture-frames.exited").exists())
        resumed, output, _ = self.run_provider("--resume", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertTrue(any(frame.get("type") == "result" for frame in output), output)
        # Each once-only line exits once: a second goes on until it has.
        self.frames({"_exit": 1, "_once": True}, {"_exit": 4, "_once": True})
        (self.config / "fixture-frames.exited").unlink()
        self.assertEqual(self.run_provider()[0].returncode, 1)
        self.assertEqual(self.run_provider()[0].returncode, 4)
        self.assertEqual(self.run_provider()[0].returncode, 0)
        self.frames({"_exit": 1, "_once": True})
        # Removing the record arms it again.
        (self.config / "fixture-frames.exited").unlink()
        rearmed, _, _ = self.run_provider()
        self.assertEqual(rearmed.returncode, 1)

    def test_a_status_that_is_not_one_is_refused_with_its_line(self):
        for status in ("x", 256, -1, 1.5):
            with self.subTest(status=status):
                self.frames({"type": "assistant", "uuid": "u", "message": {
                    "id": "u", "role": "assistant", "content": []}}, {"_exit": status})
                result, _, _ = self.run_provider()
                self.assertEqual(result.returncode, 2)
                self.assertIn("line 2 of", result.stderr)
                self.assertIn("_exit must be a status from 0 to 255", result.stderr)

    def test_a_slow_start_that_is_not_seconds_is_refused(self):
        (self.config / "fixture-slow-start").write_text("soon\n")
        result, _, _ = self.run_provider()
        self.assertEqual(result.returncode, 2)
        self.assertIn("fixture-slow-start must hold seconds", result.stderr)

    def test_a_slow_start_holds_a_conversation_but_not_the_account_check(self):
        (self.config / "fixture-slow-start").write_text("1.5\n")
        result, output, elapsed = self.run_provider()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertGreaterEqual(elapsed, 1.5)
        # Then it takes the prompt as usual.
        self.assertTrue(any(frame.get("subtype") == "init" for frame in output), output)
        probe, _, elapsed = self.run_provider("--no-session-persistence")
        self.assertEqual(probe.returncode, 0, probe.stderr)
        self.assertLess(elapsed, 1.5)


if __name__ == "__main__":
    unittest.main()
