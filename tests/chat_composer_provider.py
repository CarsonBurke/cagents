#!/usr/bin/python3
"""Contract for the synthetic Claude provider's start-up and exit switches,
run directly: no app, daemon or real provider."""

import json
import os
import selectors
import signal
import stat
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


class LiveProvider:
    """A single private process session with continuously drained JSON output."""
    def __init__(self, case):
        self.case = case
        copied = case.root / "bin/claude"
        copied.parent.mkdir(exist_ok=True)
        shutil.copy2(SCRIPT, copied)
        self.process = subprocess.Popen(
            [str(copied)], cwd=case.project, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
            env={"HOME": str(case.root / "home"), "CLAUDE_CONFIG_DIR": str(case.config),
                 "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"})
        case.addCleanup(self.cleanup)
        self.selector = selectors.DefaultSelector()
        self.buffers = {"stdout": b"", "stderr": b""}
        self.output = []
        self.stderr = b""
        for label, pipe in (("stdout", self.process.stdout), ("stderr", self.process.stderr)):
            os.set_blocking(pipe.fileno(), False)
            self.selector.register(pipe, selectors.EVENT_READ, label)

    def send(self, *frames, trailing=b""):
        self.process.stdin.write(b"".join(json.dumps(frame).encode() + b"\n" for frame in frames)
                                 + trailing)
        self.process.stdin.flush()

    def read_until(self, condition, timeout=2):
        deadline = time.monotonic() + timeout
        while not condition():
            remaining = deadline - time.monotonic()
            self.case.assertGreater(remaining, 0, (self.output[-3:], self.stderr))
            self.case.assertTrue(self.selector.get_map(), (self.process.poll(), self.stderr))
            for key, _ in self.selector.select(min(remaining, .05)):
                data = os.read(key.fileobj.fileno(), 65536)
                if not data:
                    self.selector.unregister(key.fileobj)
                    continue
                if key.data == "stderr":
                    self.stderr += data
                    continue
                self.buffers["stdout"] += data
                while b"\n" in self.buffers["stdout"]:
                    line, self.buffers["stdout"] = self.buffers["stdout"].split(b"\n", 1)
                    self.output.append(json.loads(line))
        return self.output

    def deltas(self):
        return [frame["event"]["delta"]["text"] for frame in self.output
                if frame.get("event", {}).get("delta", {}).get("type") == "text_delta"]

    def response(self, request_id):
        return any(frame.get("response", {}).get("request_id") == request_id
                   for frame in self.output)

    def first_delta(self):
        self.send(prompt(1))
        self.read_until(lambda: bool(self.deltas()))

    def finish(self):
        self.process.stdin.close()
        self.read_until(lambda: not self.selector.get_map(), timeout=5)
        self.case.assertEqual(self.process.wait(timeout=2), 0, self.stderr)
        self.case.assertEqual(self.buffers["stdout"], b"")
        self.case.assertEqual(self.stderr, b"")

    def cleanup(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=2)
        self.selector.close()
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            pipe.close()


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

    def stream(self, text, delay="0.5"):
        (self.config / "fixture-stream-reply").write_text(delay + "\n")
        (self.config / "fixture-reply.md").write_text(text)
        return LiveProvider(self)

    def history(self):
        histories = list((self.config / "projects").glob("*/*.jsonl"))
        self.assertEqual(len(histories), 1)
        return [json.loads(line) for line in histories[0].read_text().splitlines()]

    def test_stream_interrupt_settles_only_emitted_prefix_and_resumes_same_history(self):
        text = "Offline background update for responsiveness comparison.\n" * 40000
        live = self.stream(text)
        live.first_delta()
        began = time.monotonic()
        live.send({"type": "control_request", "request_id": "interrupt-stream",
                   "request": {"subtype": "interrupt"}})
        live.read_until(lambda: live.response("interrupt-stream"))
        self.assertLess(time.monotonic() - began, 2)
        stream_root = next(self.config.glob(".fixture-stream.*"))
        for path in stream_root.iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        live.finish()
        ack = next(index for index, frame in enumerate(live.output)
                   if frame.get("response", {}).get("request_id") == "interrupt-stream")
        self.assertFalse(any(frame.get("event", {}).get("delta", {}).get("type") == "text_delta"
                             for frame in live.output[ack + 1:]))
        prefix = "".join(live.deltas())
        self.assertTrue(prefix)
        self.assertLess(len(prefix), len(text))
        self.assertTrue(text.startswith(prefix))
        self.assertEqual(self.texts(live.output), [prefix])
        self.assertEqual(len([frame for frame in live.output if frame.get("type") == "result"]), 1)
        before = self.history()
        self.assertEqual(self.texts(before), [prefix])
        self.assertEqual(before[-1]["parentUuid"], "prompt-1")
        self.assertFalse(list(self.config.glob(".fixture-stream.*")))
        (self.config / "fixture-reply.md").write_text("Followup preserved.")
        (self.config / "fixture-stream-reply").unlink()
        result, output, _ = self.run_provider("--resume", before[-1]["sessionId"], frames=(prompt(2),))
        self.assertEqual(result.returncode, 0, result.stderr)
        after = self.history()
        self.assertEqual(after[:len(before)], before)
        self.assertEqual(after[len(before)]["parentUuid"], before[-1]["uuid"])
        self.assertEqual(self.texts(output), ["Followup preserved."])
        ids = [frame["uuid"] for frame in after]
        self.assertEqual(len(ids), len(set(ids)))

    def test_interrupt_before_the_first_delta_persists_no_unseen_answer(self):
        live = self.stream("An answer that has not been emitted.\n" * 40000)
        live.send(prompt(1), {"type": "control_request", "request_id": "early-interrupt",
                              "request": {"subtype": "interrupt"}})
        live.read_until(lambda: live.response("early-interrupt"))
        live.finish()
        self.assertEqual(live.deltas(), [])
        self.assertEqual(self.texts(live.output), [])
        self.assertEqual(self.texts(self.history()), [])

    def test_uninterrupted_stream_and_eof_preserve_exact_unicode_and_cadence(self):
        text = "A漢🙂\n" * 10000
        live = self.stream(text, "0.04")
        began = time.monotonic()
        live.send(prompt(1))
        live.finish()
        self.assertGreaterEqual(time.monotonic() - began, .04 * 3)
        self.assertEqual("".join(live.deltas()), text)
        self.assertEqual([len(part) for part in live.deltas()], [16384, 16384, 7232])
        self.assertEqual(self.texts(live.output), [text])
        self.assertEqual(self.texts(self.history()), [text])
        self.assertFalse(list(self.config.glob(".fixture-stream.*")))

    def test_stream_yields_control_and_preserves_complete_and_partial_read_ahead(self):
        live = self.stream("Streaming reply.\n" * 3000, "0.04")
        live.first_delta()
        second = prompt(2)
        raw_second = json.dumps(second).encode()
        # Complete controls and a partial next frame arrive in one write. The
        # helper must neither buffer past it invisibly nor parse that prefix.
        live.send({"type": "control_request", "request_id": "observe-stream",
                   "request": {"subtype": "initialize"}},
                  {"type": "control_request", "request_id": "stop-stream",
                   "request": {"subtype": "interrupt"}}, trailing=raw_second[:19])
        live.read_until(lambda: live.response("stop-stream"))
        live.process.stdin.write(raw_second[19:] + b"\n")
        live.process.stdin.flush()
        live.read_until(lambda: len([frame for frame in live.output if frame.get("type") == "result"]) == 2)
        live.finish()
        self.assertEqual([frame["response"]["request_id"] for frame in live.output
                          if frame.get("type") == "control_response"],
                         ["observe-stream", "stop-stream"])
        records = self.history()
        users = [frame["uuid"] for frame in records if frame.get("type") == "user"]
        self.assertEqual(users, ["prompt-1", "prompt-2"])
        self.assertEqual(records[2]["parentUuid"], records[1]["uuid"])
        self.assertEqual(self.texts(live.output)[-1], "Streaming reply.\n" * 3000)

    def test_term_reaps_stream_helper_and_removes_private_state(self):
        live = self.stream("Streaming reply.\n" * 40000)
        live.first_delta()
        children = Path(f"/proc/{live.process.pid}/task/{live.process.pid}/children")
        owned = [int(pid) for pid in children.read_text().split()]
        self.assertTrue(owned, "the fixture must have an actual awaited stream helper")
        live.process.send_signal(signal.SIGTERM)
        self.assertEqual(live.process.wait(timeout=2), 143)
        self.assertFalse(list(self.config.glob(".fixture-stream.*")))
        for pid in owned:
            self.assertFalse(Path(f"/proc/{pid}").exists(), f"owned helper {pid} outlived fixture")

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
