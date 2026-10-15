#!/usr/bin/env python3
"""Offline fixture contracts; these never establish native parser acceptance."""
import json
from pathlib import Path
import runpy
import socket
import tempfile
import threading
import time
import unittest

PEER = runpy.run_path(str(Path(__file__).resolve().parent / "fixtures/terminal-escape-codex.py"))


class TerminalEscapeFixture(unittest.TestCase):
    def test_aggregate_gate_cannot_mask_streaming_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            messages = []
            thread = threading.Thread(target=PEER["run_stream"], args=(messages.append, root, "private-thread", 2049))
            thread.start()
            try:
                deadline = time.monotonic() + 3
                while not (root / "stream-paused.json").exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.001)
                receipt = json.loads((root / "stream-paused.json").read_text())
                self.assertFalse(receipt["completed_aggregate_sent"])
                self.assertEqual(receipt["expected_stream_output"], PEER["EXPECTED"])
                self.assertFalse(any(message["method"] == "item/completed" for message in messages))
                deltas = [message["params"]["delta"] for message in messages if message["method"].endswith("outputDelta")]
                self.assertEqual(deltas[:2], ["\x1b", "]"])
                self.assertIn(["\x1b", "\\"], [deltas[index:index + 2] for index in range(len(deltas) - 1)])
                self.assertEqual(sum(record["payload_bytes"] for record in receipt["families"]), 7 * 2049)
                self.assertTrue(all(record["payload_delta_count"] == 3 for record in receipt["families"]))
                self.assertTrue(thread.is_alive(), "completion must wait for independent UI proof")
            finally:
                (root / "complete-stream").touch()
                thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            completed = next(message for message in messages if message["method"] == "item/completed")
            self.assertEqual(completed["params"]["item"]["aggregatedOutput"], "COMPLETED AGGREGATE REPLACED STREAM\n")
            self.assertNotIn("stream sentinel", completed["params"]["item"]["aggregatedOutput"])

    def test_websocket_heartbeat_does_not_end_a_paused_stream(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(PEER["frame"](b"heartbeat", opcode=9) + PEER["frame"]({"id": 4, "method": "models/list"}))
            pongs = []
            message = PEER["receive"](reader, pongs.append)
            self.assertEqual(pongs, [b"heartbeat"])
            self.assertEqual(message["id"], 4)
        finally:
            reader.close()
            writer.close()


if __name__ == "__main__":
    unittest.main()
