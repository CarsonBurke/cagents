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

    def test_resume_can_branch_from_an_earlier_record_without_removing_turns(self):
        session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        interrupt = {"type": "control_request", "request_id": "stop",
                     "request": {"subtype": "interrupt"}}
        result, _, _ = self.run_provider("--session-id", session,
                                        frames=(prompt(1), interrupt, prompt(2)))
        self.assertEqual(result.returncode, 0, result.stderr)
        history = next((self.config / "projects").glob(f"*/{session}.jsonl"))
        before = history.read_text().splitlines()
        (self.config / "fixture-resume-parent").write_text("prompt-1\n")
        result, _, _ = self.run_provider("--resume", session, frames=(prompt(3),))
        self.assertEqual(result.returncode, 0, result.stderr)
        after = history.read_text().splitlines()
        self.assertEqual(after[:len(before)], before)
        self.assertEqual(json.loads(after[len(before)])["parentUuid"], "prompt-1")
        uuids = [json.loads(line)["uuid"] for line in after if "uuid" in json.loads(line)]
        self.assertEqual(len(uuids), len(set(uuids)))

    def test_resume_parent_refuses_a_missing_record(self):
        (self.config / "fixture-resume-parent").write_text("missing-record\n")
        result, _, _ = self.run_provider("--resume", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Invalid fixture-resume-parent", result.stderr)

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

    def test_compact_starts_a_new_chain_that_a_resumed_provider_goes_on_from(self):
        session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        compact = {"type": "user", "uuid": "compact", "origin": {"kind": "human"},
                   "message": {"role": "user", "content": [{"type": "text", "text": "/compact"}]}}
        # The first turn stays busy until it is interrupted.
        interrupt = {"type": "control_request", "request_id": "stop",
                     "request": {"subtype": "interrupt"}}
        result, output, _ = self.run_provider(frames=(prompt(1), interrupt, compact))
        self.assertEqual(result.returncode, 0, result.stderr)
        live = [frame for frame in output if frame.get("type") in ("system", "user", "result")
                and frame.get("subtype") != "init"]
        # Live, the boundary comes first, then the summary and the result.
        compaction = next(index for index, frame in enumerate(live)
                          if frame.get("subtype") == "compact_boundary")
        self.assertTrue(live[compaction + 1].get("isSynthetic"), live)
        self.assertEqual(live[compaction + 2]["type"], "result", live)
        mangled = "".join(c if c.isalnum() else "-" for c in str(self.project.resolve()))
        history = self.config / "projects" / mangled / f"{session}.jsonl"
        records = [json.loads(line) for line in history.read_text().splitlines()]
        boundary = next(index for index, record in enumerate(records)
                        if record.get("subtype") == "compact_boundary")
        # The boundary starts a new chain, linked back only logically.
        self.assertIsNone(records[boundary]["parentUuid"])
        self.assertEqual(records[boundary]["logicalParentUuid"], records[boundary - 1]["uuid"])
        summary, caveat, command, stdout = records[boundary + 1:boundary + 5]
        self.assertTrue(summary["isCompactSummary"])
        self.assertEqual(summary["parentUuid"], records[boundary]["uuid"])
        self.assertTrue(caveat["isMeta"])
        self.assertTrue(command["message"]["content"].startswith("<command-name>/compact</command-name>"))
        self.assertTrue(stdout["repeatsCompactSummary"])
        resumed, _, _ = self.run_provider("--resume", session, frames=(prompt(3),))
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        later = [json.loads(line) for line in history.read_text().splitlines()][len(records):]
        self.assertEqual(later[0]["parentUuid"], stdout["uuid"])
        # Its records keep their own numbers after the compaction's.
        ids = [record["uuid"] for record in records + later]
        self.assertEqual(len(ids), len(set(ids)), ids)

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

    def test_a_stop_for_a_task_the_frames_started_patches_it_killed_without_a_notice(self):
        # As Claude Code 2.1.296 answers a stop for an agent that already sent
        # its notice and is waiting on its own background work.
        self.frames({"type": "system", "subtype": "task_started", "uuid": "start-@TURN@",
                     "task_id": "fixture-task-toolu_reviewer_1", "tool_use_id": "toolu_reviewer_1",
                     "task_type": "local_agent", "description": "Review restart safety"},
                    {"type": "result", "uuid": "result-@TURN@", "subtype": "success",
                     "is_error": False},
                    {"_end": True})
        stop = {"type": "control_request", "request_id": "stop-1",
                "request": {"subtype": "stop_task", "task_id": "fixture-task-toolu_reviewer_1"}}
        result, output, _ = self.run_provider(frames=(prompt(1), stop))
        self.assertEqual(result.returncode, 0, result.stderr)
        answered = next(index for index, frame in enumerate(output)
                        if frame.get("type") == "control_response")
        self.assertEqual(output[answered]["response"]["request_id"], "stop-1")
        after = output[answered + 1:]
        self.assertEqual(
            [(frame.get("subtype"), frame.get("task_id"), frame.get("patch")) for frame in after],
            [("task_updated", "fixture-task-toolu_reviewer_1", {"status": "killed"})])

    def test_a_stop_for_a_working_scene_agent_still_reports_it_stopped(self):
        (self.config / "fixture-subagents").touch()
        stop = {"type": "control_request", "request_id": "stop-1",
                "request": {"subtype": "stop_task", "task_id": "fixture-task-scout-1"}}
        interrupt = {"type": "control_request", "request_id": "stop-2",
                     "request": {"subtype": "interrupt"}}
        result, output, _ = self.run_provider(frames=(prompt(1), stop, interrupt))
        self.assertEqual(result.returncode, 0, result.stderr)
        notices = [(frame["task_id"], frame["status"]) for frame in output
                   if frame.get("subtype") == "task_notification"]
        # Scout is stopped by its own Stop, Planner by the interrupt, once each.
        self.assertEqual(notices, [("fixture-task-scout-1", "stopped"),
                                   ("fixture-task-planner-1", "stopped")])
        self.assertFalse(any(frame.get("subtype") == "task_updated" for frame in output), output)

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

    def test_long_history_replays_once_as_one_chain_and_finishes_the_first_turn(self):
        session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        (self.config / "fixture-long-history").write_text("6 300\n")
        (self.config / "fixture-first-turn-seconds").write_text("0\n")
        result, output, _ = self.run_provider("--session-id", session, frames=(prompt(1), prompt(2)))
        self.assertEqual(result.returncode, 0, result.stderr)
        replayed = [frame for frame in output if frame.get("uuid", "").startswith("fixture-history-")]
        self.assertEqual([frame["type"] for frame in replayed], ["user", "assistant"] * 3)
        for frame in replayed:
            content = frame["message"]["content"]
            text = content if isinstance(content, str) else content[0]["text"]
            self.assertEqual(len(text), len(f"Step {frame['uuid'][-1]}: ") + 300)
        self.assertEqual(sum(frame.get("type") == "result" for frame in output), 2)
        history = [json.loads(line) for line in
                   next((self.config / "projects").glob(f"*/{session}.jsonl")).read_text().splitlines()]
        chain = [record["uuid"] for record in history]
        self.assertEqual([record["parentUuid"] for record in history], [None, *chain[:-1]])
        self.assertEqual(sum(uuid.startswith("fixture-history-") for uuid in chain), 6)

    def test_long_history_that_is_not_a_count_and_size_is_refused(self):
        (self.config / "fixture-long-history").write_text("many\n")
        result, _, _ = self.run_provider("--session-id", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        self.assertEqual(result.returncode, 2)
        self.assertIn("fixture-long-history must hold a count and a size", result.stderr)


if __name__ == "__main__":
    unittest.main()
