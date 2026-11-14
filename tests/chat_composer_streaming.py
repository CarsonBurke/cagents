#!/usr/bin/python3
"""Duplex stream contract for the offline Claude provider; no native app needed."""

import importlib.util
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import tempfile
import time
import unittest

# Isolated Python (-I) leaves this directory off sys.path.
_provider_spec = importlib.util.spec_from_file_location(
    "chat_composer_provider", Path(__file__).resolve().parent / "chat_composer_provider.py")
chat_composer_provider = importlib.util.module_from_spec(_provider_spec)
_provider_spec.loader.exec_module(chat_composer_provider)
SCRIPT = chat_composer_provider.SCRIPT
prompt = chat_composer_provider.prompt


def control(subtype, request_id=None, **fields):
    return {"type": "control_request", "request_id": request_id or subtype,
            "request": {"subtype": subtype, **fields}}


def delta(frame):
    return frame.get("event", {}).get("type") == "content_block_delta"


class Peer:
    def __init__(self, project, config):
        self.process = subprocess.Popen(
            [str(SCRIPT)], cwd=project, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True, env={"HOME": str(config.parent),
                                       "CLAUDE_CONFIG_DIR": str(config),
                                       "PATH": "/usr/local/bin:/usr/bin:/bin"})
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.frames = []
        self.closed = False

    def close(self):
        if self.closed:
            return
        self.closed = True
        # This peer created its own process group. Settle a sleeper as well as
        # the shell if a replay regression fails before graceful interruption.
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.process.wait(timeout=5)
        self.selector.close()
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            pipe.close()

    def send(self, frame):
        self.write((json.dumps(frame) + "\n").encode())

    def write(self, data):
        self.process.stdin.write(data)
        self.process.stdin.flush()

    def until(self, predicate, timeout=3):
        deadline = time.monotonic() + timeout
        while True:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                frame = json.loads(line)
                self.frames.append(frame)
                if predicate(frame):
                    return frame
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise AssertionError("Provider did not emit the expected frame before the deadline")
            data = os.read(self.process.stdout.fileno(), 65536)
            if not data:
                raise AssertionError("Provider closed stdout: " + self.process.stderr.read().decode())
            self.buffer += data

    def response(self, request_id):
        return self.until(lambda f: f.get("response", {}).get("request_id") == request_id)

    def interrupt(self):
        self.send(control("interrupt"))
        self.response("interrupt")
        self.until(lambda f: f.get("type") == "result")
        self.send(control("initialize", "barrier"))
        self.response("barrier")


class Streaming(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="chat-stream-contract-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.config = self.root / "home/.claude"
        self.config.mkdir(parents=True)
        self.project = self.root / "project"
        self.project.mkdir()
        self.reply = self.config / "fixture-reply.md"
        self.reply.write_text("x" * 1048576)
        (self.config / "fixture-stream-reply").write_text("1\n")

    def start(self):
        peer = Peer(self.project, self.config)
        self.addCleanup(peer.close)
        peer.send(prompt(1))
        peer.until(delta)
        return peer

    def history(self):
        return [json.loads(line) for path in (self.config / "projects").glob("*/*.jsonl")
                for line in path.read_text().splitlines()]

    def start_replay(self, *frames):
        marker = {"type": "assistant", "uuid": "replay-start-@TURN@",
                  "message": {"id": "replay-start-@TURN@", "role": "assistant",
                              "content": [{"type": "text", "text": "Replay started."}]}}
        (self.config / "fixture-frames.jsonl").write_text(
            "".join(json.dumps(frame) + "\n" for frame in (marker, *frames)))
        peer = Peer(self.project, self.config)
        self.addCleanup(peer.close)
        peer.send(prompt(1))
        peer.until(lambda f: f.get("uuid") == "replay-start-1")
        return peer

    def test_long_replay_pause_services_controls_and_interrupts_without_late_frames(self):
        (self.config / "fixture-usage.json").write_text('{"remaining":17}')
        late = {"type": "assistant", "uuid": "replay-never-@TURN@",
                "message": {"id": "replay-never-@TURN@", "role": "assistant",
                            "content": [{"type": "text", "text": "Must not escape interruption."}]}}
        peer = self.start_replay({"_sleep": 600}, late, {"_end": True})
        started = time.monotonic()
        peer.send(control("get_usage"))
        self.assertEqual(peer.response("get_usage")["response"]["response"], {"remaining": 17})
        peer.send(control("stop_task", task_id="fixture-background-command"))
        peer.response("stop_task")
        peer.until(lambda f: f.get("task_id") == "fixture-background-command" and
                   f.get("patch", {}).get("status") == "killed")
        peer.send(prompt(99))
        peer.until(lambda f: f.get("command_uuid") == "prompt-99" and f.get("state") == "queued")
        peer.interrupt()
        self.assertLess(time.monotonic() - started, 3, "the 600-second replay remains duplex")
        self.assertEqual([f["uuid"] for f in self.history()], ["prompt-1", "replay-start-1"])
        self.assertEqual(sum(f.get("type") == "result" for f in peer.frames), 1)
        self.assertFalse(any(f.get("uuid", "").startswith("replay-never-") for f in peer.frames))

        # Resume input in this same shell, without a delayed replay writer
        # persisting or publishing any part of the interrupted turn.
        (self.config / "fixture-frames.jsonl").unlink()
        self.reply.write_text("The next turn completed.")
        peer.send(prompt(2))
        peer.until(lambda f: f.get("type") == "result")
        history = self.history()
        self.assertEqual([f["uuid"] for f in history],
                         ["prompt-1", "replay-start-1", "prompt-2", "fixture-message-2"])
        self.assertEqual(history[2]["parentUuid"], "replay-start-1")
        peer.process.stdin.close()
        self.assertEqual(peer.process.wait(timeout=3), 0)
        self.assertEqual(peer.process.stderr.read(), b"")

    def test_partial_interrupt_survives_a_replay_pause_deadline(self):
        continued = {"type": "assistant", "uuid": "replay-next-@TURN@",
                     "message": {"id": "replay-next-@TURN@", "role": "assistant",
                                 "content": [{"type": "text", "text": "Next replay frame."}]}}
        peer = self.start_replay({"_sleep": 0.12}, continued, {"_sleep": 600})
        request = (json.dumps(control("interrupt")) + "\n").encode()
        peer.write(request[:25])
        peer.until(lambda f: f.get("uuid") == "replay-next-1")
        peer.write(request[25:])
        peer.response("interrupt")
        peer.until(lambda f: f.get("type") == "result")
        peer.send(control("initialize", "barrier"))
        peer.response("barrier")
        self.assertEqual(sum(f.get("type") == "result" for f in peer.frames), 1)
        self.assertFalse(any(f.get("uuid") == "fixture-message-1" for f in self.history()))

    def test_eof_during_long_replay_pause_exits_without_a_sleeper(self):
        peer = self.start_replay({"_sleep": 600})
        peer.process.stdin.close()
        self.assertEqual(peer.process.wait(timeout=3), 0)
        self.assertEqual(peer.process.stderr.read(), b"")
        self.assertEqual([f["uuid"] for f in self.history()], ["prompt-1", "replay-start-1"])

    def test_scripted_end_releases_busy_state_for_the_next_turn(self):
        result = {"type": "result", "uuid": "replay-result-@TURN@",
                  "subtype": "success", "is_error": False}
        peer = self.start_replay({"_sleep": 0.01}, result, {"_end": True})
        peer.until(lambda f: f.get("uuid") == "replay-result-1")
        peer.send(prompt(2))
        peer.until(lambda f: f.get("uuid") == "replay-result-2")
        self.assertEqual([f["uuid"] for f in self.history()],
                         ["prompt-1", "replay-start-1", "prompt-2", "replay-start-2"])

    def test_interrupt_settles_before_the_remaining_long_reply_and_can_start_another_turn(self):
        peer = self.start()
        started = time.monotonic()
        peer.interrupt()
        self.assertLess(time.monotonic() - started, 3)  # Full stream takes 64 seconds.
        result_at = next(i for i, f in enumerate(peer.frames) if f.get("type") == "result")
        self.assertFalse(any(delta(f) for f in peer.frames[result_at:]))
        self.assertEqual(sum(f.get("type") == "result" for f in peer.frames), 1)
        self.assertFalse(any(f.get("type") == "assistant" for f in peer.frames))
        self.assertEqual([f["uuid"] for f in self.history()], ["prompt-1"])
        # A new reply in the same process must use the state and parent chain
        # owned by the input loop, without persisting the unsent first answer.
        self.reply.write_text("The next turn completed.")
        peer.send(prompt(2))
        peer.until(lambda f: f.get("type") == "result")
        history = self.history()
        self.assertEqual([f["uuid"] for f in history],
                         ["prompt-1", "prompt-2", "fixture-message-2"])
        self.assertEqual(history[1]["parentUuid"], "prompt-1")
        self.assertEqual(history[2]["parentUuid"], "prompt-2")

    def test_zero_and_submicrosecond_delays_still_service_ready_controls(self):
        for value in ("0", "0.0000001"):
            with self.subTest(value=value):
                (self.config / "fixture-stream-reply").write_text(value + "\n")
                peer = self.start()
                peer.send(prompt(99))
                peer.until(lambda f: f.get("command_uuid") == "prompt-99" and f.get("state") == "queued")
                peer.interrupt()
                self.assertLess(sum(delta(f) for f in peer.frames), 64)
                self.assertFalse(any(f.get("type") == "assistant" for f in peer.frames))
                self.assertEqual(sum(f.get("type") == "result" for f in peer.frames), 1)
                peer.close()

    def test_every_control_and_queued_user_are_serviced_during_the_stream(self):
        (self.config / "fixture-fast-mode").touch()
        (self.config / "fixture-usage.json").write_text('{"remaining":17}')
        peer = self.start()
        requests = [control("initialize"), control("get_usage"),
                    control("set_model", model="sonnet"),
                    control("apply_flag_settings", settings={"fastMode": True}),
                    control("cancel_async_message", message_uuid="queued-other"),
                    control("stop_task", task_id="fixture-task-other")]
        for request in requests:
            peer.send(request)
        peer.send(prompt(99))
        peer.until(lambda f: f.get("command_uuid") == "prompt-99" and f.get("state") == "queued")
        responses = {f["response"]["request_id"]: f["response"] for f in peer.frames
                     if f.get("type") == "control_response"}
        self.assertEqual(set(responses), {r["request_id"] for r in requests})
        self.assertEqual(responses["get_usage"]["response"], {"remaining": 17})
        self.assertEqual(responses["apply_flag_settings"]["response"]["fast_mode_state"], "on")
        self.assertTrue(any(f.get("command_uuid") == "queued-other" and f.get("state") == "cancelled"
                            for f in peer.frames))
        self.assertTrue(any(f.get("patch", {}).get("status") == "killed" for f in peer.frames))
        peer.interrupt()
        self.assertEqual([f["uuid"] for f in self.history()], ["prompt-1"])
        self.reply.write_text("Done.")
        peer.send(prompt(2))
        init = peer.until(lambda f: f.get("subtype") == "init")
        self.assertEqual(init["model"], "sonnet")
        peer.until(lambda f: f.get("type") == "result")

    def test_known_background_agents_stop_once_during_streaming(self):
        (self.config / "fixture-subagents").touch()
        peer = self.start()
        peer.send(control("stop_task", task_id="fixture-task-scout-1"))
        peer.response("stop_task")
        peer.until(lambda f: f.get("subtype") == "task_notification" and
                   f.get("task_id") == "fixture-task-scout-1")
        peer.interrupt()
        stopped = [f["task_id"] for f in peer.frames if f.get("subtype") == "task_notification"]
        self.assertEqual(stopped, ["fixture-task-scout-1", "fixture-task-planner-1"])
        self.assertFalse(any(f.get("uuid") == "fixture-message-1" for f in self.history()))
        peer.process.stdin.close()
        self.assertEqual(peer.process.wait(timeout=5), 0)
        self.assertEqual(peer.process.stderr.read(), b"")

    def test_partial_control_survives_a_chunk_deadline_without_blocking_deltas(self):
        (self.config / "fixture-stream-reply").write_text("0.12\n")
        peer = self.start()
        request = (json.dumps(control("interrupt")) + "\n").encode()
        peer.write(request[:25])
        peer.until(delta)  # A partial line must neither reach jq nor block the next chunk.
        peer.write(request[25:])
        peer.response("interrupt")
        peer.until(lambda f: f.get("type") == "result")
        peer.send(control("initialize", "barrier"))
        peer.response("barrier")
        results = [f for f in peer.frames if f.get("type") == "result"]
        self.assertEqual(len(results), 1)
        self.assertEqual([f["uuid"] for f in self.history()], ["prompt-1"])

    def test_controls_do_not_restart_the_chunk_delay(self):
        (self.config / "fixture-stream-reply").write_text("0.3\n")
        peer = self.start()
        started = time.monotonic()
        for index in range(8):
            peer.send(control("initialize", str(index)))
            peer.response(str(index))
            time.sleep(0.05)
        # At least one chunk was emitted while the controls kept arriving.
        self.assertGreaterEqual(sum(delta(f) for f in peer.frames), 2)
        self.assertLess(time.monotonic() - started, 2)
        peer.interrupt()

    def test_eof_finishes_the_finite_stream_and_discards_an_unterminated_input_tail(self):
        text = "Small finite reply.\n" * 1800
        self.reply.write_text(text)
        (self.config / "fixture-stream-reply").write_text("0.01\n")
        result = subprocess.run(
            [str(SCRIPT)], cwd=self.project, capture_output=True, text=True,
            input=json.dumps(prompt(1)) + '\n{"type":"control_request"', timeout=5,
            env={"HOME": str(self.config.parent), "CLAUDE_CONFIG_DIR": str(self.config),
                 "PATH": "/usr/local/bin:/usr/bin:/bin"})
        self.assertEqual(result.returncode, 0, result.stderr)
        frames = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual("".join(f["event"]["delta"]["text"] for f in frames if delta(f)), text)
        self.assertEqual([f for f in frames if f.get("type") == "assistant"][0]
                         ["message"]["content"][0]["text"], text)
        self.assertEqual(self.history()[-1]["parentUuid"], "prompt-1")

    def test_invalid_chunk_delays_fail_with_a_fixture_error(self):
        for value in ("soon", "-1", ".", "1.2.3"):
            with self.subTest(value=value):
                (self.config / "fixture-stream-reply").write_text(value + "\n")
                result = subprocess.run([str(SCRIPT)], cwd=self.project, input="", text=True,
                                        capture_output=True, timeout=5,
                                        env={"HOME": str(self.config.parent),
                                             "CLAUDE_CONFIG_DIR": str(self.config),
                                             "PATH": "/usr/local/bin:/usr/bin:/bin"})
                self.assertEqual(result.returncode, 2)
                self.assertIn("fixture-stream-reply must hold seconds", result.stderr)

    def test_the_second_line_sets_the_characters_per_delta(self):
        # Slicing is by character, so multibyte text must survive the split.
        text = "Token-sized deltas approximate a provider's stream: é😀中. " * 20
        self.reply.write_text(text)
        (self.config / "fixture-stream-reply").write_text("0\n24\n")
        result = subprocess.run(
            [str(SCRIPT)], cwd=self.project, capture_output=True, text=True,
            input=json.dumps(prompt(1)) + "\n", timeout=10,
            env={"HOME": str(self.config.parent), "CLAUDE_CONFIG_DIR": str(self.config),
                 "PATH": "/usr/local/bin:/usr/bin:/bin"})
        self.assertEqual(result.returncode, 0, result.stderr)
        chunks = [f["event"]["delta"]["text"]
                  for f in map(json.loads, result.stdout.splitlines()) if delta(f)]
        self.assertEqual("".join(chunks), text)
        self.assertEqual({len(chunk) for chunk in chunks[:-1]}, {24})
        self.assertEqual(len(chunks), -(-len(text) // 24))

    def test_a_delay_alone_keeps_the_default_delta_size(self):
        text = "x" * 16385
        self.reply.write_text(text)
        (self.config / "fixture-stream-reply").write_text("0\n")
        result = subprocess.run(
            [str(SCRIPT)], cwd=self.project, capture_output=True, text=True,
            input=json.dumps(prompt(1)) + "\n", timeout=10,
            env={"HOME": str(self.config.parent), "CLAUDE_CONFIG_DIR": str(self.config),
                 "PATH": "/usr/local/bin:/usr/bin:/bin"})
        self.assertEqual(result.returncode, 0, result.stderr)
        chunks = [f["event"]["delta"]["text"]
                  for f in map(json.loads, result.stdout.splitlines()) if delta(f)]
        self.assertEqual([len(chunk) for chunk in chunks], [16384, 1])

    def test_invalid_delta_sizes_fail_with_a_fixture_error(self):
        for value in ("0", "-4", "many", "1.5"):
            with self.subTest(value=value):
                (self.config / "fixture-stream-reply").write_text("0.1\n" + value + "\n")
                result = subprocess.run([str(SCRIPT)], cwd=self.project, input="", text=True,
                                        capture_output=True, timeout=5,
                                        env={"HOME": str(self.config.parent),
                                             "CLAUDE_CONFIG_DIR": str(self.config),
                                             "PATH": "/usr/local/bin:/usr/bin:/bin"})
                self.assertEqual(result.returncode, 2)
                self.assertIn("second line must be a positive number", result.stderr)


if __name__ == "__main__":
    unittest.main()
