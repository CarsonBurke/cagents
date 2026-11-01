#!/usr/bin/python3
"""Offline producer contracts only; no native UI, daemon, or real provider."""

import itertools
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


SCRIPT = Path(__file__).resolve().parent / "fixtures/chat-composer-claude.sh"
SESSION = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
DIAGNOSTIC = "Invalid fixture-agent-task-receipt.json\n"
UNSETTLED = []  # Retain original Child objects and directories, never signal a PID.


def prompt(number):
    return {
        "type": "user",
        "uuid": f"prompt-{number}",
        "origin": {"kind": "human"},
        "message": {"role": "user", "content": "Run the offline receipt check."},
    }


def control(name):
    return {
        "type": "control_request",
        "request_id": name,
        "request": {"subtype": name},
    }


class PrivateFixture:
    def __init__(self):
        self.root = Path(tempfile.mkdtemp(prefix="chat-task-receipt-contract-"))
        self.home = self.root / "home"
        self.config = self.home / ".claude"
        self.project = self.root / "project"
        self.config.mkdir(parents=True)
        self.project.mkdir()
        self.children = []

    def __enter__(self):
        return self

    def __exit__(self, kind, error, traceback):
        unsettled = []
        for child, deadline in self.children:
            try:
                if child.returncode is None:
                    child.communicate(timeout=max(0.001, deadline - time.monotonic()))
                child.wait(timeout=max(0.001, deadline - time.monotonic()))
            except (subprocess.TimeoutExpired, OSError) as cleanup_error:
                unsettled.append((child, self.root))
                if error is not None:
                    error.add_note(f"Original fixture cleanup failed: {cleanup_error}")
        if unsettled:
            UNSETTLED.extend(unsettled)
            detail = f"Original fixture child not settled; directory retained at {self.root}"
            if error is not None:
                error.add_note(detail)
                return False
            raise AssertionError(detail)
        shutil.rmtree(self.root)
        return False

    @property
    def selector(self):
        return self.config / "fixture-agent-task-receipt.json"

    def select(self, situation, status):
        self.selector.write_text(json.dumps({"situation": situation, "status": status}))

    def run(self, frames=(), args=()):
        # env=None inherits opaque authority without reading/copying it. Only
        # these synthetic fixture paths and documented fixture switches differ.
        argv = [
            "/usr/bin/env",
            f"HOME={self.home}",
            f"CLAUDE_CONFIG_DIR={self.config}",
            f"CAGENTS_FIXTURE_IMAGE={self.home / 'absent-image.png'}",
            "CAGENTS_FIXTURE_FAST_MODE=0",
            "CAGENTS_FIXTURE_VERBOSITY=0",
            str(SCRIPT),
            *args,
        ]
        child = subprocess.Popen(
            argv, cwd=self.project, env=None, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.children.append((child, time.monotonic() + 10))
        payload = "".join(json.dumps(frame) + "\n" for frame in frames)
        stdout, stderr = child.communicate(payload, timeout=5)
        status = child.wait(timeout=1)  # Original Child wait, even on a nonzero result.
        return status, stdout, stderr

    def history(self):
        paths = list(self.config.glob("projects/*/*.jsonl"))
        if not paths:
            return []
        if len(paths) != 1:
            raise AssertionError(f"Expected one fixture transcript, found {len(paths)}")
        return [json.loads(line) for line in paths[0].read_text().splitlines()]


class TaskReceiptContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        for program in ("/bin/sh", "/usr/bin/env", "jq"):
            if shutil.which(program) is None:
                raise RuntimeError(f"Required fixture tool unavailable: {program}")

    def test_absent_scene_preserves_ordinary_busy_queue_and_metadata(self):
        with PrivateFixture() as fixture:
            status, stdout, stderr = fixture.run([
                control("initialize"), prompt(1), prompt("queued"),
                control("interrupt"), prompt(2),
            ])
            self.assertEqual((status, stderr), (0, ""))
            frames = [json.loads(line) for line in stdout.splitlines()]
            self.assertFalse(any("fixture-agent-" in f.get("uuid", "") for f in frames))
            self.assertIn({"type": "command_lifecycle", "command_uuid": "prompt-queued", "state": "queued"}, frames)
            results = [f for f in frames if f["type"] == "result"]
            self.assertEqual([f["uuid"] for f in results], ["fixture-result-1", "fixture-result-2"])
            interrupt = next(i for i, f in enumerate(frames) if f["type"] == "control_response" and f["response"]["request_id"] == "interrupt")
            self.assertGreater(frames.index(results[0]), interrupt)
            self.assertEqual([f["uuid"] for f in fixture.history()], ["prompt-1", "fixture-message-1", "prompt-2", "fixture-message-2"])
        for args, expected in [
            (("--version",), "2.1.286 (Claude Code fixture)\n"),
            (("auth",), '{"loggedIn":true,"email":"fixture@example.test","subscriptionType":"pro"}\n'),
        ]:
            with self.subTest(args=args), PrivateFixture() as fixture:
                fixture.selector.write_text("malformed scene is not a metadata probe")
                self.assertEqual(fixture.run(args=args), (0, expected, ""))
                self.assertEqual(fixture.history(), [])

    def test_four_scenes_emit_bound_receipt_before_ack_and_preserve_history(self):
        for situation, terminal in itertools.product(("text", "task-only"), ("completed", "failed")):
            with self.subTest(situation=situation, terminal=terminal), PrivateFixture() as fixture:
                fixture.select(situation, terminal)
                status, stdout, stderr = fixture.run([control("initialize"), prompt(1)])
                self.assertEqual((status, stderr), (0, ""))
                frames = [json.loads(line) for line in stdout.splitlines()]
                self.assertEqual(len(frames), 10)
                launch, started, receipt, ack, answer, result = frames[4:]
                tool, task = "fixture-agent-tool-1", "fixture-agent-task-1"
                self.assertEqual(launch["message"]["content"], [{"type": "tool_use", "id": tool, "name": "Agent", "input": {"description": "Check receipt settlement", "run_in_background": True}}])
                self.assertEqual((started["subtype"], started["task_id"], started["tool_use_id"], started["task_type"]), ("task_started", task, tool, "local_agent"))
                if situation == "text":
                    expected = f"<task-notification>\n<task-id>{task}</task-id>\n<tool-use-id>{tool}</tool-use-id>\n<status>{terminal}</status>\n<summary>Offline Agent task {terminal}</summary>\n<result>Offline report.</result>\n</task-notification>"
                    self.assertEqual((receipt["type"], receipt["origin"], receipt["message"]), ("user", {"kind": "task-notification"}, {"role": "user", "content": expected}))
                else:
                    self.assertEqual(receipt, {"type": "system", "subtype": "task_notification", "uuid": "fixture-agent-notice-1", "task_id": task, "status": terminal, "summary": f"Offline Agent task {terminal}"})
                    self.assertNotIn("tool_use_id", receipt)
                self.assertEqual(ack["tool_use_result"], {"status": "async_launched", "agentId": task})
                self.assertEqual(ack["message"]["content"], [{"type": "tool_result", "tool_use_id": tool, "content": "Launched"}])
                self.assertEqual(answer["type"], "assistant")
                self.assertEqual(result, {"type": "result", "uuid": "fixture-agent-result-1", "subtype": "success", "is_error": False, "session_id": SESSION})
                history = fixture.history()
                live = [f for f in frames if "uuid" in f]
                self.assertEqual(len(history), 7)
                previous = None
                for saved, emitted in zip(history, live, strict=True):
                    self.assertEqual(saved.pop("sessionId"), SESSION)
                    self.assertEqual(saved.pop("parentUuid"), previous)
                    self.assertEqual(saved, emitted)
                    previous = saved["uuid"]

    def test_malformed_scene_refuses_before_output_or_history_without_fallback(self):
        invalid = ["", "{", "[]", "null", "{}", '{"situation":"text"}',
                   '{"situation":"unknown","status":"completed"}',
                   '{"situation":"text","status":"running"}',
                   '{"situation":1,"status":"completed"}',
                   '{"situation":"text","status":"completed","extra":true}',
                   '{"situation":"text","status":"completed"}\n{}']
        for content in invalid:
            with self.subTest(content=content), PrivateFixture() as fixture:
                fixture.selector.write_text(content)
                self.assertEqual(fixture.run([control("initialize"), prompt(1)]), (2, "", DIAGNOSTIC))
                self.assertFalse((fixture.config / "projects").exists())
        for kind in ("directory", "symlink"):
            with self.subTest(kind=kind), PrivateFixture() as fixture:
                if kind == "directory":
                    fixture.selector.mkdir()
                else:
                    fixture.selector.symlink_to(fixture.config / "absent-scene.json")
                self.assertEqual(fixture.run([prompt(1)]), (2, "", DIAGNOSTIC))
                self.assertFalse((fixture.config / "projects").exists())

    def test_selected_turns_finish_and_resume_with_distinct_correlated_ids(self):
        with PrivateFixture() as fixture:
            fixture.select("text", "completed")
            status, stdout, stderr = fixture.run([prompt(1), prompt(2)])
            self.assertEqual((status, stderr), (0, ""))
            frames = [json.loads(line) for line in stdout.splitlines()]
            self.assertEqual([f["uuid"] for f in frames if f["type"] == "result"], ["fixture-agent-result-1", "fixture-agent-result-2"])
            self.assertFalse(any(f.get("state") == "queued" for f in frames))
            status, stdout, stderr = fixture.run([prompt(3)])
            self.assertEqual((status, stderr), (0, ""))
            frames = [json.loads(line) for line in stdout.splitlines()]
            launch = next(f for f in frames if f.get("uuid") == "fixture-agent-launch-3")
            self.assertEqual(launch["message"]["content"][0]["id"], "fixture-agent-tool-3")
            history = fixture.history()
            self.assertEqual(len(history), 21)
            self.assertEqual(len({f["uuid"] for f in history}), 21)
            self.assertEqual(history[-1]["uuid"], "fixture-agent-result-3")


if __name__ == "__main__":
    unittest.main()
