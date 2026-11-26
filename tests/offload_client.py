#!/usr/bin/env python3
"""Contract tests for the offload clients' shared local slots."""

import fcntl
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
from offload_client import local_phase, run_local


class LocalAdmission(unittest.TestCase):
    def setUp(self):
        root = self.enterContext(tempfile.TemporaryDirectory())
        self.lock = Path(root) / "client.lock"
        self.environment = os.environ | {"CAGENTS_BUILD_LOCAL_SYNC_LOCK": str(self.lock)}
        self.environment.pop("CAGENTS_BUILD_LOCAL_SYNC_SLOTS", None)

    def launch(self, arguments):
        process = self.enterContext(subprocess.Popen(
            [sys.executable, "-I", *arguments], env=self.environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        ))
        self.addCleanup(process.kill)
        return process

    def read(self, pipe):
        self.assertTrue(select.select([pipe], [], [], 10)[0], "waiting for the client stalled")
        return pipe.readline()

    def holder(self):
        return self.launch([str(SCRIPTS / "offload_client.py"), "local", sys.executable, "-u", "-c",
                            'import os; print("held"); os.read(0, 1)'])

    def test_default_two_slots_and_either_slot_wakes_the_waiter(self):
        first = self.holder()
        self.assertEqual(self.read(first.stdout), b"held\n")
        second = self.holder()
        self.assertEqual(self.read(second.stdout), b"held\n")
        waiter = self.holder()
        self.assertEqual(self.read(waiter.stderr), b"waiting for a local sync slot\n")
        self.assertFalse(select.select([waiter.stdout], [], [], 0)[0])

        # Abruptly release slot 1 while slot 0 is still held.
        second.kill()
        second.wait(timeout=10)
        self.assertEqual(self.read(waiter.stdout), b"held\n")
        self.assertIsNone(first.poll())

    def test_configured_single_slot_wait_respects_deadline(self):
        self.environment["CAGENTS_BUILD_LOCAL_SYNC_SLOTS"] = "1"
        with local_phase(self.environment):
            with self.assertRaisesRegex(TimeoutError, "waiting for a local sync slot"):
                with local_phase(self.environment, deadline=time.monotonic() + 0.1):
                    self.fail("the sole slot was already held")
        with local_phase(self.environment):
            self.assertFalse(Path(f"{self.lock}.1").exists())

    def test_cancellation_wakes_waiter_and_returns_the_slot(self):
        self.environment["CAGENTS_BUILD_LOCAL_SYNC_SLOTS"] = "1"
        with local_phase(self.environment):
            waiting = self.launch(["-c", """
import sys
sys.path.insert(0, sys.argv[1])
from offload_client import LocalSyncCancelled, run_local

try:
    run_local(lambda _descriptor: sys.exit(3), cancel_fd=0)
except LocalSyncCancelled:
    sys.exit(0)
sys.exit(4)
""", str(SCRIPTS)])
            self.assertEqual(self.read(waiting.stderr), b"waiting for a local sync slot\n")
            waiting.stdin.write(b"cancel")
            waiting.stdin.flush()
            self.assertEqual(waiting.wait(timeout=10), 0)
        with open(f"{self.lock}.0", "rb") as descriptor:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_local_worker_lowers_priority_without_changing_caller(self):
        before = os.getpriority(os.PRIO_PROCESS, 0)
        inside = run_local(lambda _descriptor: os.getpriority(os.PRIO_PROCESS, 0),
                           environment=self.environment)
        self.assertEqual(inside, min(before + 10, 19))
        self.assertEqual(os.getpriority(os.PRIO_PROCESS, 0), before)

    def test_priority_only_command_can_run_while_sync_slots_are_busy(self):
        self.environment["CAGENTS_BUILD_LOCAL_SYNC_SLOTS"] = "1"
        before = os.getpriority(os.PRIO_PROCESS, 0)
        with local_phase(self.environment):
            process = self.launch([str(SCRIPTS / "offload_client.py"), "low", sys.executable, "-c",
                                   "import os; print(os.getpriority(os.PRIO_PROCESS, 0))"])
            self.assertEqual(int(self.read(process.stdout)), min(before + 10, 19))
            self.assertEqual(process.wait(timeout=10), 0)


if __name__ == "__main__":
    unittest.main()
