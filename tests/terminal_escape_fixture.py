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
            worker_errors = []

            def run_stream():
                try:
                    PEER["run_stream"](messages.append, root, "private-thread", 2049)
                except BaseException as error:
                    worker_errors.append(error)

            thread = threading.Thread(target=run_stream)
            thread.start()
            try:
                deadline = time.monotonic() + 3
                while not (root / "stream-paused.json").exists():
                    if worker_errors:
                        raise worker_errors[0]
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.001)
                receipt = json.loads((root / "stream-paused.json").read_text())
                self.assertFalse(receipt["completed_aggregate_sent"])
                self.assertEqual(receipt["expected_stream_output"], PEER["EXPECTED"])
                self.assertFalse(any(message["method"] == "item/completed" for message in messages))
                deltas = [message["params"]["delta"] for message in messages if message["method"].endswith("outputDelta")]
                # Check the actual stream independently of the peer's receipt
                # and EXPECTED constant, before the aggregate gate is opened.
                self.assertEqual(
                    [delta for delta in deltas if delta.endswith(" stream sentinel\n")],
                    [
                        "osc stream sentinel\n",
                        "dcs stream sentinel\n",
                        "apc stream sentinel\n",
                        "sos stream sentinel\n",
                        "pm stream sentinel\n",
                        "csi stream sentinel\n",
                        "esc stream sentinel\n",
                    ],
                )
                self.assertEqual(deltas[:2], ["\x1b", "]"])
                self.assertIn(["\x1b", "\\"], [deltas[index:index + 2] for index in range(len(deltas) - 1)])
                self.assertEqual(sum(record["payload_bytes"] for record in receipt["families"]), 7 * 2049)
                self.assertTrue(all(record["payload_delta_count"] == 3 for record in receipt["families"]))
                self.assertTrue(thread.is_alive(), "completion must wait for independent UI proof")
            finally:
                (root / "complete-stream").touch()
                thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            # Thread exceptions are otherwise only printed, even if the peer
            # already emitted item/completed and its aggregate looks correct.
            if worker_errors:
                raise worker_errors[0]
            completed = next(message for message in messages if message["method"] == "item/completed")
            self.assertEqual(completed["params"]["item"]["aggregatedOutput"], "COMPLETED AGGREGATE REPLACED STREAM\n")
            self.assertNotIn("stream sentinel", completed["params"]["item"]["aggregatedOutput"])

    def test_websocket_heartbeat_does_not_end_a_paused_stream(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(PEER["frame"](b"heartbeat", opcode=9) + PEER["frame"]({"id": 4, "method": "models/list"}))
            writer.shutdown(socket.SHUT_WR)
            pongs = []
            message = PEER["receive"](reader, pongs.append)
            self.assertEqual(pongs, [b"heartbeat"])
            self.assertEqual(message["id"], 4)
        finally:
            reader.close()
            writer.close()


if __name__ == "__main__":
    unittest.main()
