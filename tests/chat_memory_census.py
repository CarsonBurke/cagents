#!/usr/bin/env python3
"""Contracts for the native memory measurement report, without launching the UI."""
import json
from pathlib import Path
import runpy
import tempfile
import unittest


CENSUS = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/summarize-chat-memory"))


class OwnerCensus(unittest.TestCase):
    def parse(self, text):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ui.log"
            path.write_text(text)
            return CENSUS["trace_owners"](path, 42)

    def test_partial_event_keeps_the_last_complete_owner(self):
        owners, _, coverage = self.parse(
            'cagents::memory: process_id=42 owner="chat_runtime" chat_id=chat '
            'resident_bytes=8192 cache_entries=4 exposed_presentation_bytes=32768\n'
            'cagents::memory: process_id=42 owner="chat_runtime" chat_id=chat resident_bytes=0')
        self.assertEqual(CENSUS["owner_totals"](owners)["chat_runtime"], {
            "owners": 1, "resident_bytes": 8192, "cache_entries": 4,
            "exposed_presentation_bytes": 32768})
        self.assertEqual(coverage["incomplete_owner_events"], 1)

    def test_missing_invalid_or_negative_counter_cannot_fabricate_zero(self):
        prefix = 'cagents::memory: process_id=42 owner="ipc" codec=7 '
        owners, _, coverage = self.parse(prefix + 'read_capacity=1024 write_capacity=2048\n'
            + prefix + 'read_capacity=0\n'
            + prefix + 'read_capacity=0 write_capacity=truncated\n'
            + prefix + 'read_capacity=-1 write_capacity=0\n')
        self.assertEqual(CENSUS["owner_totals"](owners)["ipc"], {
            "owners": 1, "read_capacity": 1024, "write_capacity": 2048})
        self.assertEqual(coverage["incomplete_owner_events"], 3)

    def test_complete_release_removes_owner(self):
        prefix = 'cagents::memory: process_id=42 owner="chat_client" chat_id=chat '
        owners, _, coverage = self.parse(prefix + 'resident_bytes=100\n'
            + prefix + 'released=true resident_bytes=0\n')
        self.assertEqual(CENSUS["owner_totals"](owners)["chat_client"], {
            "owners": 0, "resident_bytes": 0})
        self.assertEqual(coverage["complete_owner_events"], 2)

    def test_stack_only_or_wrong_pid_does_not_qualify_owner_coverage(self):
        for text in (
            'cagents::memory: process_id=42 owner="thread_stack" role="chat_reader" '
            'thread_id=43 stack_base=4096 reserved_stack_bytes=8192 guard_bytes=4096\n',
            'cagents::memory: process_id=43 owner="chat_client" chat_id=chat resident_bytes=100\n',
            'cagents::memory: process_id=42 owner="chat_client" chat_id=chat\n',
        ):
            with self.subTest(log=text), self.assertRaisesRegex(SystemExit, "no complete owner"):
                self.parse(text)

    def test_offline_pending_file_and_release_are_complete_separate_owners(self):
        prefix = 'cagents::memory: process_id=42 owner="chat_offline" offline_id=8 '
        owners, _, coverage = self.parse(prefix + 'pending_bytes=4096 in_flight_bytes=0 io_buffer_capacity=0 disk_bytes=0\n'
            + prefix + 'pending_bytes=0 in_flight_bytes=0 io_buffer_capacity=0 disk_bytes=8192\n'
            + prefix + 'pending_bytes=0 in_flight_bytes=0 disk_bytes=0\n')
        self.assertEqual(CENSUS["owner_totals"](owners)["chat_offline"], {
            "owners": 1, "pending_bytes": 0, "in_flight_bytes": 0,
            "io_buffer_capacity": 0, "disk_bytes": 8192})
        self.assertEqual(coverage["incomplete_owner_events"], 1)
        owners, _, _ = self.parse(prefix + 'pending_bytes=1 in_flight_bytes=2 io_buffer_capacity=3 disk_bytes=4\n'
            + prefix + 'released=true pending_bytes=0 in_flight_bytes=0 io_buffer_capacity=0 disk_bytes=0\n')
        self.assertEqual(CENSUS["owner_totals"](owners)["chat_offline"]["owners"], 0)

    def test_mapping_intersections_do_not_prorate_partial_vmas(self):
        result = CENSUS["stack_totals"]([(4096, 8192), (8192, 4096)], [
            {"start": 4096, "end": 8192, "Rss": 8, "Pss": 7},
            {"start": 8192, "end": 16384, "Rss": 99, "Pss": 88},
        ])
        self.assertEqual(result["reserved_bytes"], 8192)
        self.assertEqual(result["rss_kib"], 8)
        self.assertEqual(result["partial_overlap_vmas_unattributed"], 1)
        self.assertNotIn("start", json.dumps(result))


if __name__ == "__main__":
    unittest.main()
