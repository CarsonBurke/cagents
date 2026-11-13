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
from unittest import mock


SCRIPT = Path(__file__).resolve().parent / "fixtures/chat-composer-claude.sh"
SESSION = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
DIAGNOSTIC = "Invalid fixture-agent-task-receipt.json\n"


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
        first_cleanup_error = None
        for ownership in self.children:
            # A timeout bounds an individual observation, never ownership.
            # Keep this root and original Child locally until its wait settles;
            # no later test, directory removal or exit can abandon that wait.
            while True:
                try:
                    child = ownership["child"]
                    deadline = ownership["deadline"]
                    if not ownership["drained"]:
                        try:
                            child.communicate(timeout=max(0.001, deadline - time.monotonic()))
                            ownership["drained"] = True
                        except BaseException as cleanup_error:
                            if first_cleanup_error is None:
                                first_cleanup_error = cleanup_error
                    # A drain failure must not hide an observable original wait.
                    child.wait(timeout=max(0.001, deadline - time.monotonic()))
                    break
                except BaseException as cleanup_error:
                    if first_cleanup_error is None:
                        first_cleanup_error = cleanup_error
                try:
                    time.sleep(0.05)
                except BaseException as cleanup_error:
                    if first_cleanup_error is None:
                        first_cleanup_error = cleanup_error
        try:
            shutil.rmtree(self.root)
        except BaseException as cleanup_error:
            if first_cleanup_error is None:
                first_cleanup_error = cleanup_error
        if error is not None:
            if first_cleanup_error is not None:
                try:
                    error.add_note("A cleanup error also occurred; original fixture children were settled before propagation.")
                except BaseException:
                    pass  # Preserve the original failure object after settlement.
            return False
        if first_cleanup_error is not None:
            raise first_cleanup_error
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
        ownership = {"child": child, "deadline": time.monotonic() + 10, "drained": False}
        self.children.append(ownership)
        payload = "".join(json.dumps(frame) + "\n" for frame in frames)
        stdout, stderr = child.communicate(payload, timeout=5)
        ownership["drained"] = True
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

    def test_cleanup_keeps_original_wait_and_first_failure_before_further_work(self):
        class DrainFailure(BaseException):
            pass

        class WaitFailure(BaseException):
            pass

        class Ownership(dict):
            # Also model the original tuple record: an old-helper failure
            # must concern ownership, rather than its record representation.
            def __iter__(self):
                yield self["child"]
                yield self["deadline"]

        for original, permanent_drain in [(AssertionError("original assertion"), False), (None, False), (None, True)]:
            with self.subTest(original=original is not None, permanent_drain=permanent_drain):
                events = []
                drain_error, wait_error = DrainFailure("drain"), WaitFailure("wait")

                class FakeChild:
                    returncode = None
                    drained = False
                    settled = False
                    waits = 0

                    def communicate(self, timeout):
                        events.append("drain")
                        if not self.drained or permanent_drain:
                            self.drained = True
                            raise drain_error
                        return "", ""

                    def wait(self, timeout):
                        events.append("wait")
                        self.waits += 1
                        if self.waits == 1 and not permanent_drain:
                            raise wait_error
                        self.returncode = 0
                        self.settled = True
                        events.append("settled")
                        return 0

                child = FakeChild()
                fixture = object.__new__(PrivateFixture)
                fixture.root = Path("/synthetic-fixture-not-created")
                fixture.children = [Ownership(child=child, deadline=10, drained=False)]

                def remove_root(path):
                    events.append("remove")
                    self.assertEqual(path, fixture.root)
                    self.assertTrue(child.settled, "root must outlive its original wait")

                expected = original if original is not None else drain_error
                with mock.patch.object(time, "monotonic", return_value=0), mock.patch.object(time, "sleep", side_effect=lambda seconds: events.append("sleep")) as sleep, mock.patch.object(shutil, "rmtree", side_effect=remove_root):
                    with self.assertRaises(type(expected)) as caught:
                        with fixture:
                            if original is not None:
                                raise original
                self.assertIs(caught.exception, expected)
                events.append("further-work")
                self.assertTrue(child.settled)
                if permanent_drain:
                    self.assertEqual(events, ["drain", "wait", "settled", "remove", "further-work"])
                    sleep.assert_not_called()
                else:
                    self.assertEqual(events, ["drain", "wait", "sleep", "drain", "wait", "settled", "remove", "further-work"])
                    sleep.assert_called_once_with(0.05)


if __name__ == "__main__":
    unittest.main()
