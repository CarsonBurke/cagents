#!/usr/bin/python3
"""Black-box contracts for the opt-in offline SDK peer, not cagents admission.

Only the copied Python fixture is executed, with finite synthetic stdin and EOF.
No cagents/helper/provider binary, network, or authority is supplied by this test.
The ordinary env exec overlays only owned fake HOME/account inputs; inherited
authority is neither read, copied, cleared, nor replaced.
"""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SESSION = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
REQUEST = "post-resume-permission"


def initialize(identifier="initialize-1"):
    return {
        "type": "control_request",
        "request_id": identifier,
        "request": {"subtype": "initialize"},
    }


def denial():
    return {
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": REQUEST,
            "response": {
                "behavior": "deny",
                "message": "The user denied this request",
            },
        },
    }


class ResumeAnswerFixtureContracts(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="updater1513-", dir="/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.root.chmod(0o700)
        self.account = self.root / "fixture-inputs/workload/account-alpha"
        self.project = self.root / "fixture-inputs/workload/project"
        for path in (
            self.root / "bin",
            self.root / "home",
            self.account,
            self.account.with_name("account-beta"),
            self.project,
        ):
            path.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.script = self.root / "bin/claude"
        shutil.copyfile(Path(__file__).with_name("claude-upgrade-continuity.py"), self.script)
        self.script.chmod(0o700)
        key = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(self.project))
        self.history = self.account / "projects" / key / (SESSION + ".jsonl")
        self.history.parent.mkdir(parents=True, mode=0o700)
        # Fake SDK inputs only: these are not HOST rows, history proof, or User authority.
        self.before = (
            json.dumps({"type": "user", "sessionId": SESSION, "message": "synthetic prior prompt"})
            + "\n"
        ).encode()
        self.history.write_bytes(self.before)
        self.history.chmod(0o600)
        (self.root / "provider-done").write_text("done")
        (self.root / "prompts").write_text("prompt\n")
        retry = self.root / "history-retry-start-failed-once.json"
        retry.write_text(json.dumps({"thread": SESSION, "account": str(self.account)}, sort_keys=True))
        retry.chmod(0o600)
        self.receipt = self.root / "resume-answer-receipts.jsonl"

    def run_fixture(self, frames, *, opt_in=True):
        if opt_in:
            mode = self.root / "resume-answer-mode"
            mode.write_bytes(b"deny-only-v1\n")
            mode.chmod(0o600)
        # env execs the one finite Python peer with ordinary inheritance and
        # only two fake-path overlays. No daemon or lifecycle process is launched.
        return subprocess.run(
            [
                "/usr/bin/env",
                "HOME=" + str(self.root / "home"),
                "CLAUDE_CONFIG_DIR=" + str(self.account),
                sys.executable,
                "-I",
                "-B",
                str(self.script),
                "--resume",
                SESSION,
            ],
            input="".join(json.dumps(frame) + "\n" for frame in frames),
            capture_output=True,
            text=True,
            cwd=self.project,
            timeout=5,
            check=False,
        )

    def assert_no_prompt_or_history_change(self, result):
        self.assertEqual(self.history.read_bytes(), self.before)
        self.assertEqual((self.root / "prompts").read_text(), "prompt\n")
        self.assertFalse((self.root / "prompt-bodies").exists())
        frames = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertFalse(any(frame["type"] in ("user", "assistant", "result") for frame in frames))
        return frames

    def assert_one_denial(self, input_count=1):
        lines = self.receipt.read_text().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertEqual(
            json.loads(lines[0]),
            {"request_id": REQUEST, "behavior": "deny", "session_id": SESSION},
        )
        self.assertEqual(self.receipt.stat().st_mode & 0o777, 0o600)
        inputs = [
            json.loads(line)
            for line in (self.root / "provider-trace.jsonl").read_text().splitlines()
        ]
        self.assertEqual(
            sum(value.get("event") == "resume-answer-input" for value in inputs),
            input_count,
        )

    def test_absent_opt_in_keeps_existing_resume_protocol(self):
        unrelated = {"type": "control_response", "response": {"request_id": "unrelated"}}
        result = self.run_fixture([initialize(), unrelated], opt_in=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.assert_no_prompt_or_history_change(result),
            [
                {"type": "system", "subtype": "init", "session_id": SESSION},
                {
                    "type": "control_response",
                    "response": {"subtype": "success", "request_id": "initialize-1", "response": {}},
                },
            ],
        )
        self.assertFalse(self.receipt.exists())

    def test_exact_resumed_request_and_denial_are_recorded_once(self):
        result = self.run_fixture([initialize(), initialize("initialize-2"), denial()])
        self.assertEqual(result.returncode, 0, result.stderr)
        frames = self.assert_no_prompt_or_history_change(result)
        requests = [frame for frame in frames if frame["type"] == "control_request"]
        self.assertEqual(
            requests,
            [{
                "type": "control_request",
                "request_id": REQUEST,
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Read",
                    "tool_use_id": "post-resume-read",
                    "input": {"file_path": "/fixture/deny-only"},
                },
            }],
        )
        self.assert_one_denial()

    def test_wrong_request_or_approval_is_refused(self):
        wrong_id = denial()
        wrong_id["response"]["request_id"] = "not-the-request"
        approval = denial()
        approval["response"]["response"] = {"behavior": "allow", "updatedInput": {}}
        for frame, error in (
            (wrong_id, "wrong resume Answer request"),
            (approval, "only the exact denial is supported"),
        ):
            with self.subTest(error=error):
                result = self.run_fixture([initialize(), frame])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)
                self.assert_no_prompt_or_history_change(result)
                self.assertFalse(self.receipt.exists())

    def test_duplicate_denial_is_refused_without_a_second_receipt(self):
        result = self.run_fixture([initialize(), denial(), denial()])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected or duplicate resume Answer", result.stderr)
        self.assert_no_prompt_or_history_change(result)
        self.assert_one_denial(input_count=2)

    def test_second_user_prompt_is_refused_without_rewriting_history(self):
        result = self.run_fixture([
            initialize(),
            denial(),
            {"type": "user", "uuid": "not-replayed", "message": {"role": "user", "content": "LIVE_WORK_1513"}},
        ])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected second prompt or replacement provider", result.stderr)
        self.assert_no_prompt_or_history_change(result)
        self.assert_one_denial()


if __name__ == "__main__":
    unittest.main()
