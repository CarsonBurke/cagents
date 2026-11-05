#!/usr/bin/env python3
"""Contract for scripts/lock-holders, using private locks and owned processes."""
import contextlib
import importlib.machinery
import importlib.util
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts" / "lock-holders"


class LockHolders(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-lock-holders-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.lock = self.root / "shared.lock"
        self.lock.touch()
        self.pids = []
        self.processes = []
        self.addCleanup(self.end_processes)

    def end_processes(self):
        for pid in self.pids:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(pid, signal.SIGKILL)
        for process in self.processes:
            process.wait(timeout=5)

    def start(self, *command):
        # Its own process group, so cleanup reaches what it starts.
        process = subprocess.Popen(
            command, stdout=subprocess.DEVNULL, close_fds=False, start_new_session=True
        )
        self.processes.append(process)
        self.pids.append(process.pid)
        return process

    def report(self):
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(TOOL), str(self.lock)],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def wait_for(self, predicate, label):
        deadline = time.monotonic() + 10
        while not predicate():
            self.assertLess(time.monotonic(), deadline, f"timed out waiting for {label}")
            time.sleep(0.05)

    def holds(self):
        probe = subprocess.run(["flock", "-n", str(self.lock), "true"])
        return probe.returncode != 0

    def test_a_free_lock_says_so(self):
        self.assertEqual(self.report().strip(), f"{self.lock}: free")

    def test_a_missing_lock_says_so(self):
        self.lock.unlink()
        self.assertIn("does not exist", self.report())

    def test_a_slots_cost_record_is_not_listed_as_a_lock(self):
        Path(f"{self.lock}.1").touch()
        Path(f"{self.lock}.cost").touch()
        loader = importlib.machinery.SourceFileLoader("lock_holders_tool", str(TOOL))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        tool = importlib.util.module_from_spec(spec)
        loader.exec_module(tool)
        with unittest.mock.patch.dict(os.environ, {"CAGENTS_BUILD_LOCK": str(self.lock)}):
            locks = tool.default_locks()
        self.assertIn(str(self.lock), locks)
        self.assertIn(f"{self.lock}.1", locks)
        self.assertNotIn(f"{self.lock}.cost", locks)

    def test_an_ordinary_holder_and_its_waiter_raise_no_warning(self):
        holder = self.start("flock", str(self.lock), "sleep", "60")
        self.wait_for(self.holds, "holder")
        waiter = self.start("flock", str(self.lock), "true")
        self.wait_for(lambda: f"waiting: {waiter.pid}" in self.report(), "waiter")
        report = self.report()
        self.assertIn(f"holder {holder.pid} flock", report)
        self.assertIn("idle", report)
        self.assertNotIn("WARNING", report)
        self.assertIn("note: nothing holding this used CPU", report)

    def test_a_leftover_that_inherited_the_lock_is_named_and_left_running(self):
        pid_file = self.root / "daemon"
        self.start(
            "flock", str(self.lock), "sh", "-c",
            'setsid sleep 60 & echo $! > "$0.new"; mv "$0.new" "$0"', str(pid_file),
        ).wait(timeout=10)
        daemon = int(pid_file.read_text())
        self.pids.append(daemon)
        self.assertTrue(self.holds(), "the fixture must reproduce the inherited lock")
        report = self.report()
        self.assertIn(f"holder {daemon} sleep", report)
        self.assertIn("WARNING", report)
        os.kill(daemon, 0)
        self.assertTrue(self.holds())

    def test_a_wrapper_script_running_a_build_raises_no_warning(self):
        # Named cargo, so it counts as a build though the script closes the lock for it.
        cargo = self.root / "cargo"
        cargo.write_text("#!/bin/sh\nwhile :; do sleep 1; done\n")
        cargo.chmod(0o755)
        wrapper_with_child = self.root / "outer.sh"
        wrapper_with_child.write_text(f'#!/bin/sh\nexec 9>>"{self.lock}"\nflock 9\n"{cargo}" 9>&- &\nwait\n')
        wrapper_with_child.chmod(0o755)
        self.start(str(wrapper_with_child))
        self.wait_for(self.holds, "holder")
        self.wait_for(lambda: "outer.sh" in self.report(), "the report")
        self.assertNotIn("WARNING", self.report())

    def test_a_holder_with_a_busy_child_gets_no_idle_note(self):
        self.start("flock", "--close", str(self.lock), "sh", "-c", "while :; do :; done")
        self.wait_for(self.holds, "holder")
        self.assertNotIn("note:", self.report())

    def test_a_queued_request_is_not_taken_for_a_hidden_holder(self):
        loader = importlib.machinery.SourceFileLoader("lock_holders_tool", str(TOOL))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        tool = importlib.util.module_from_spec(spec)
        loader.exec_module(tool)
        with unittest.mock.patch.object(tool, "lock_records", return_value=[(os.getpid(), 7, True)]), \
                unittest.mock.patch.object(tool.os, "listdir", side_effect=PermissionError):
            self.assertEqual(tool.hidden_lockers(), {})

    def test_an_idle_gate_queue_gets_no_note(self):
        queue = self.root / "shared.lock.queue"
        queue.touch()
        self.lock = queue
        self.start("flock", str(queue), "sleep", "60")
        self.wait_for(self.holds, "holder")
        self.assertNotIn("note:", self.report())

    def test_each_kind_of_session_or_build_counts_without_a_warning(self):
        for name in ("kwin_wayland", "niri", "native-evidence", "rustc", "cagents-cargo-gate", "build-disk-guard"):
            with self.subTest(name=name):
                program = self.root / name
                program.write_text(f'#!/bin/sh\nexec 9>>"{self.lock}"\nflock 9\nsleep 60 9>&- &\nwait\n')
                program.chmod(0o755)
                self.start(str(program))
                self.wait_for(self.holds, "holder")
                self.assertNotIn("WARNING", self.report())
                self.end_processes()
                self.pids.clear()
                self.processes.clear()

    def test_a_lock_named_through_a_symlink_is_found(self):
        link = self.root / "link.lock"
        link.symlink_to(self.lock)
        holder = self.start("flock", str(self.lock), "sleep", "60")
        self.wait_for(self.holds, "holder")
        real, self.lock = self.lock, link
        self.assertIn(f"holder {holder.pid} flock", self.report())
        self.lock = real

    def test_a_paused_waiter_is_listed_and_a_bystander_is_not(self):
        holder = self.start("flock", str(self.lock), "sleep", "60")
        self.wait_for(self.holds, "holder")
        waiter = self.start("flock", str(self.lock), "true")
        self.wait_for(lambda: f"waiting: {waiter.pid}" in self.report(), "waiter")
        os.kill(waiter.pid, signal.SIGSTOP)
        self.wait_for(lambda: Path(f"/proc/{waiter.pid}/wchan").read_text() == "do_signal_stop", "pause")
        self.assertIn(f"waiting: {waiter.pid}", self.report())
        os.kill(waiter.pid, signal.SIGCONT)
        # Open but blocked on another lock: not waiting for this one.
        other = self.root / "other.lock"
        taken = self.start("flock", str(other), "sleep", "60")
        self.wait_for(lambda: self.is_held(other), "other holder")
        with self.lock.open("a") as opened:
            bystander = subprocess.Popen(
                ["flock", str(other), "true"], pass_fds=(opened.fileno(),), start_new_session=True,
            )
        self.processes.append(bystander)
        self.pids.append(bystander.pid)
        self.wait_for(lambda: Path(f"/proc/{bystander.pid}/wchan").read_text() == "locks_lock_inode_wait", "block")
        self.assertNotIn(str(bystander.pid), self.report())
        del holder, taken

    def is_held(self, path):
        return subprocess.run(["flock", "-n", str(path), "true"]).returncode != 0

    def test_a_locker_with_readable_descriptors_is_not_reported_for_another_file(self):
        # Same inode number, other file: what a btrfs subvolume can produce.
        loader = importlib.machinery.SourceFileLoader("lock_holders_tool", str(TOOL))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        tool = importlib.util.module_from_spec(spec)
        loader.exec_module(tool)
        with unittest.mock.patch.object(tool, "lock_records", return_value=[(os.getpid(), 7, False)]):
            self.assertEqual(tool.hidden_lockers(), {})
        with unittest.mock.patch.object(tool, "lock_records", return_value=[(os.getpid(), 7, False)]), \
                unittest.mock.patch.object(tool.os, "listdir", side_effect=PermissionError):
            self.assertEqual(tool.hidden_lockers(), {7: os.getpid()})

    def test_a_directory_that_is_not_valid_text_does_not_crash_the_report(self):
        strange = os.fsencode(self.root) + b"/\xff"
        os.mkdir(strange)
        holder = subprocess.Popen(
            ["flock", str(self.lock), "sleep", "60"], cwd=strange, start_new_session=True,
        )
        self.processes.append(holder)
        self.pids.append(holder.pid)
        self.wait_for(self.holds, "holder")
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(TOOL), str(self.lock)],
            capture_output=True, env=os.environ | {"PYTHONUTF8": "1", "PYTHONIOENCODING": "utf-8"}, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_holder_busy_only_through_its_descendants_counts_as_busy(self):
        spin = "while :; do :; done"
        self.start("flock", "--close", str(self.lock), "sh", "-c", spin)
        self.wait_for(self.holds, "holder")
        self.assertIn(" flock, busy", self.report())


if __name__ == "__main__":
    unittest.main()
