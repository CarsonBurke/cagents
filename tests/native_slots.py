#!/usr/bin/python3
"""Contract tests for scripts/native_slots.py: arrival order, release and cancellation."""
import fcntl
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
SCRIPT = REPOSITORY / "scripts" / "native_slots.py"

# Takes a slot on descriptors this shell owns, as native-evidence does, then
# reports which one and keeps it until its stdin closes.
TAKER = r"""
import os, subprocess, sys
lock, wait = sys.argv[1], sys.argv[2]
count = int(subprocess.run([sys.executable, "-I", sys.argv[3], "count"], capture_output=True,
                           text=True, check=True).stdout)
queue = os.open(lock + ".queue", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
slots = [os.open(lock if index == 0 else f"{lock}.{index}", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
         for index in range(count)]
arguments = ["--queue-fd", str(queue)]
for descriptor in slots:
    arguments += ["--slot-fd", str(descriptor)]
if wait != "forever":
    arguments += ["--wait", wait]
taken = subprocess.run([sys.executable, "-I", sys.argv[3], "acquire", *arguments, "--parent-pid", str(os.getpid())],
                       pass_fds=(queue, *slots), capture_output=True, text=True)
os.close(queue)
if taken.returncode:
    print(f"status {taken.returncode}", flush=True)
    sys.exit(0)
index = int(taken.stdout)
for position, descriptor in enumerate(slots):
    if position != index:
        os.close(descriptor)
print(f"slot {index}", flush=True)
sys.stdin.read()
"""


def held(path: Path) -> bool:
    with path.open("a") as probe:
        try:
            fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(probe, fcntl.LOCK_UN)
    return False


class NativeSlots(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="native-slots-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.lock = Path(self.directory.name) / "native.lock"
        self.takers: list[subprocess.Popen] = []
        self.addCleanup(self.end_takers)

    def end_takers(self) -> None:
        for taker in self.takers:
            if taker.poll() is None:
                taker.kill()
            taker.wait()
            for stream in (taker.stdin, taker.stdout):
                stream.close()

    def take(self, slots: int, wait: str = "forever") -> subprocess.Popen:
        taker = subprocess.Popen(
            [sys.executable, "-I", "-c", TAKER, str(self.lock), wait, str(SCRIPT)],
            env={**os.environ, "CAGENTS_EVIDENCE_NATIVE_SLOTS": str(slots)},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.takers.append(taker)
        return taker

    def outcome(self, taker: subprocess.Popen) -> str:
        return taker.stdout.readline().strip()

    def release(self, taker: subprocess.Popen) -> None:
        taker.stdin.close()
        taker.wait(timeout=10)

    def slot(self, index: int) -> Path:
        return self.lock if index == 0 else Path(f"{self.lock}.{index}")

    def queued(self, count: int) -> None:
        """Waits until `count` requests wait for this test's lock files."""
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            inodes = set()
            for path in (self.lock, Path(f"{self.lock}.queue"), Path(f"{self.lock}.1"), Path(f"{self.lock}.2")):
                try:
                    inodes.add(str(path.stat().st_ino))
                except FileNotFoundError:
                    pass
            waiting = [line for line in Path("/proc/locks").read_text().splitlines()
                       if "->" in line and "FLOCK" in line and line.split()[-3].rsplit(":", 1)[-1] in inodes]
            if len(waiting) >= count:
                return
            time.sleep(0.02)
        self.fail(f"{count} waiters never queued")

    def pending(self, taker: subprocess.Popen) -> bool:
        """Whether the taker is still waiting, without blocking on it."""
        return not select.select([taker.stdout], [], [], 0.5)[0]

    def test_sessions_fill_separate_slots_until_none_is_free(self) -> None:
        first, second = self.take(2), self.take(2)
        self.assertEqual({self.outcome(first), self.outcome(second)}, {"slot 0", "slot 1"})
        self.assertTrue(held(self.slot(0)) and held(self.slot(1)))
        self.assertFalse(held(Path(f"{self.lock}.queue")))
        busy = self.take(2, wait="0")
        self.assertEqual(self.outcome(busy), "status 1")
        self.release(first)
        self.assertEqual(self.outcome(self.take(2, wait="0"))[:5], "slot ")

    def test_waiters_take_freed_slots_in_the_order_they_arrived(self) -> None:
        holders = [self.take(2), self.take(2)]
        self.assertEqual(sorted(self.outcome(holder) for holder in holders), ["slot 0", "slot 1"])
        # The first waits at the head, for slot 0 and polling slot 1; the
        # second waits behind it in the queue.
        first = self.take(2)
        self.queued(1)
        second = self.take(2)
        self.queued(2)
        # Whichever slot frees, only the head may take it.
        self.release(holders[1])
        self.assertTrue(self.outcome(first).startswith("slot "))
        self.assertTrue(self.pending(second))
        self.release(holders[0])
        self.assertTrue(self.outcome(second).startswith("slot "))

    def test_an_older_tool_waiting_on_slot_zero_shares_its_line(self) -> None:
        holder = self.take(1)
        self.assertEqual(self.outcome(holder), "slot 0")
        older = subprocess.Popen(["flock", str(self.lock), "-c", "echo older; read line"],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.takers.append(older)
        self.queued(1)
        newer = self.take(1)
        self.queued(2)
        self.release(holder)
        # Both wait in the kernel's line for slot 0 and take it in turn;
        # neither retries past the other.
        if self.pending(older):
            self.assertEqual(self.outcome(newer), "slot 0")
            self.assertTrue(self.pending(older))
            self.release(newer)
            self.assertEqual(older.stdout.readline().strip(), "older")
        else:
            self.assertEqual(older.stdout.readline().strip(), "older")
            self.assertTrue(self.pending(newer))
            older.stdin.close()
            older.wait(timeout=10)
            self.assertEqual(self.outcome(newer), "slot 0")

    def test_a_wait_that_runs_out_leaves_no_slot_or_queue_held(self) -> None:
        holder = self.take(2)
        other = self.take(2)
        self.outcome(holder)
        self.outcome(other)
        late = self.take(2, wait="1")
        self.assertEqual(self.outcome(late), "status 1")
        self.release(holder)
        self.release(other)
        for path in (self.slot(0), self.slot(1), Path(f"{self.lock}.queue")):
            self.assertFalse(held(path), path)

    def test_a_cancelled_waiter_cannot_take_a_slot_later(self) -> None:
        holder = self.take(1)
        self.assertEqual(self.outcome(holder), "slot 0")
        queue = os.open(f"{self.lock}.queue", os.O_WRONLY | os.O_APPEND)
        slot = os.open(self.lock, os.O_WRONLY | os.O_APPEND)
        try:
            waiter = subprocess.Popen(
                [sys.executable, "-I", str(SCRIPT), "acquire", "--queue-fd", str(queue), "--slot-fd", str(slot),
                 "--parent-pid", str(os.getpid())],
                env={**os.environ, "CAGENTS_EVIDENCE_NATIVE_SLOTS": "1"},
                pass_fds=(queue, slot), stdout=subprocess.PIPE, text=True)
            self.queued(1)
            waiter.send_signal(signal.SIGTERM)
            self.assertEqual(waiter.wait(timeout=10), 128 + signal.SIGTERM)
            waiter.stdout.close()
            self.release(holder)
            time.sleep(0.5)
            # Neither this process's open files nor anything else took it.
            self.assertFalse(held(self.lock))
            self.assertFalse(held(Path(f"{self.lock}.queue")))
        finally:
            os.close(queue)
            os.close(slot)

    def test_a_waiter_whose_caller_is_gone_gives_up(self) -> None:
        queue = os.open(f"{self.lock}.queue", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        slot = os.open(self.lock, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        try:
            result = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), "acquire", "--queue-fd", str(queue), "--slot-fd", str(slot),
                 "--parent-pid", str(os.getpid() + 10_000_000)],
                pass_fds=(queue, slot), capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertFalse(held(self.lock))
        finally:
            os.close(queue)
            os.close(slot)

    def test_the_slot_count_is_bounded(self) -> None:
        for value, status in (("", 0), ("1", 0), ("8", 0), ("0", 2), ("9", 2), ("03", 2), ("x", 2)):
            result = subprocess.run([sys.executable, "-I", str(SCRIPT), "count"], capture_output=True, text=True,
                                    env={**os.environ, "CAGENTS_EVIDENCE_NATIVE_SLOTS": value})
            self.assertEqual(result.returncode, status, value)
        paths = subprocess.run([sys.executable, "-I", str(SCRIPT), "paths", "/var/tmp/x.lock"], capture_output=True,
                               text=True, check=True, env={**os.environ, "CAGENTS_EVIDENCE_NATIVE_SLOTS": "3"})
        self.assertEqual(paths.stdout.split(), ["/var/tmp/x.lock", "/var/tmp/x.lock.1", "/var/tmp/x.lock.2"])


if __name__ == "__main__":
    unittest.main()
