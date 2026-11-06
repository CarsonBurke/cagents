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
import textwrap
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
        self.assertNotIn("no holder is a recognized", report)
        self.assertIn("note: nothing holding this used CPU during the one-second sample", report)
        self.assertIn("Run this again", report)
        self.assertNotIn("leftover", self.notes(report))

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
        notes = self.notes(report)
        self.assertIn("no holder is a recognized build or session", notes)
        self.assertIn("idle during the sample", notes)
        self.assertIn("could have inherited the lock", notes)
        # It can't tell this from a script between its own steps, so never concludes.
        self.assertIn("this can't tell which", notes)
        self.assertIn("Everyone queued waits until every holder releases it", notes)
        self.assertIn("commands and directories above", notes)
        self.assertIn("ask the owner", notes)
        self.assertNotIn("took this lock", notes)
        self.assertNotRegex(notes, "(?i)kill|probably|leftover|WARNING")
        os.kill(daemon, 0)
        self.assertTrue(self.holds())

    def test_the_help_does_not_present_process_age_as_hold_time(self):
        output = subprocess.run([sys.executable, "-I", "-B", str(TOOL), "--help"],
                                capture_output=True, text=True, check=True).stdout
        self.assertIn("Process age isn't hold time", output)

    @staticmethod
    def notes(report):
        """Only what the tool says: a holder's directory could contain any word."""
        return "\n".join(line for line in report.splitlines() if line.startswith("  note:"))

    def python_holder(self, body):
        """An unrecognized holder that takes the lock itself, then runs BODY."""
        return self.start(
            sys.executable, "-c",
            "import fcntl, os, sys, time\n"
            "lock = open(sys.argv[1], 'a')\n"
            "fcntl.flock(lock, fcntl.LOCK_EX)\n" + textwrap.dedent(body),
            str(self.lock),
        )

    def test_a_holder_that_took_the_lock_itself_is_not_suspected_of_inheriting_it(self):
        holder = self.python_holder("time.sleep(60)")
        self.wait_for(self.holds, "holder")
        report = self.report()
        self.assertRegex(
            report,
            rf"holder {holder.pid} .*process age \d+:\d\d:\d\d, so it took this lock at most "
            "that long ago",
        )
        notes = self.notes(report)
        self.assertIn(f"idle during the sample. {holder.pid} took this lock; any process that inherited",
                      notes)
        self.assertIn("inherited it keeps it after the taker exits.", notes)
        self.assertNotIn("can't tell which", notes)
        self.assertNotRegex(notes, "(?i)kill|leftover")

    def test_several_takers_of_a_shared_lock_are_all_named(self):
        takers = []
        for _ in range(2):
            taker = self.start(
                sys.executable, "-c",
                "import fcntl, os, sys, time\n"
                "lock = open(sys.argv[1], 'a')\n"
                "fcntl.flock(lock, fcntl.LOCK_SH)\n"
                "open(sys.argv[1] + '.' + str(os.getpid()), 'w').close()\n"
                "time.sleep(60)\n",
                str(self.lock),
            )
            takers.append(taker)
            self.wait_for(Path(f"{self.lock}.{taker.pid}").exists, "a shared holder")
        notes = self.notes(self.report())
        first, second = sorted(taker.pid for taker in takers)
        self.assertIn(f"{first} and {second} took this lock; any process that inherited it", notes)
        self.assertNotIn("can't tell which", notes)

    def test_a_taker_does_not_speak_for_a_holder_that_inherited_from_it(self):
        pid_file = self.root / "child"
        taker = self.python_holder(f"""
            child = os.fork()
            if child == 0:
                open({str(pid_file)!r}, "w").write(str(os.getpid()))
                time.sleep(60)
                os._exit(0)
            time.sleep(60)
        """)
        self.wait_for(lambda: pid_file.exists() and pid_file.read_text(), "child")
        child = int(pid_file.read_text())
        self.pids.append(child)
        report = self.report()
        self.assertRegex(report, rf"holder {taker.pid} .*at most that long ago")
        self.assertNotRegex(report, rf"holder {child} .*at most that long ago")
        notes = self.notes(report)
        self.assertIn(f"{taker.pid} took this lock; any process that inherited it", notes)
        # It may be a script between its steps, which holds a lock the same way.
        self.assertIn("working through its own steps", notes)
        self.assertIn("can't tell which", notes)

    def test_a_busy_unrecognized_holder_is_said_to_be_busy(self):
        self.python_holder("while True: pass")
        self.wait_for(self.holds, "holder")
        report = self.report()
        self.assertIn("no holder is a recognized build or session, and they were busy during", report)

    def test_a_running_locker_that_handed_the_lock_on_is_not_said_to_hold_it(self):
        # The parent took the lock, gave its descriptor to a child and closed its own.
        pid_file = self.root / "child"
        self.python_holder(f"""
            child = os.fork()
            if child == 0:
                open({str(pid_file)!r}, "w").write(str(os.getpid()))
                time.sleep(60)
                os._exit(0)
            lock.close()
            time.sleep(60)
        """)
        self.wait_for(lambda: pid_file.exists() and pid_file.read_text(), "child")
        child = int(pid_file.read_text())
        self.pids.append(child)
        self.wait_for(lambda: self.report().count("  holder ") == 1, "the parent letting go")
        report = self.report()
        self.assertIn(f"holder {child} ", report)
        self.assertNotIn("took this lock", self.notes(report))
        self.assertIn("could have inherited the lock from another process", report)

    def test_a_shell_holding_the_lock_of_a_flock_that_exited_is_only_undetermined(self):
        # The usual `exec 9>LOCK; flock 9`: the process that took the lock has gone.
        shell = self.start("bash", "-c", 'exec 9>"$0"; flock 9; exec -a holder-shell sleep 60',
                           str(self.lock))
        self.wait_for(self.holds, "holder")
        report = self.report()
        self.assertIn(f"holder {shell.pid} ", report)
        notes = self.notes(report)
        self.assertNotIn("took this lock", notes)
        self.assertIn("this can't tell which", notes)
        self.assertNotRegex(notes, "(?i)kill|leftover")

    def test_a_holders_figure_is_labeled_as_its_process_age(self):
        self.start("flock", str(self.lock), "sleep", "60")
        self.wait_for(self.holds, "holder")
        report = self.report()
        self.assertRegex(report, r"process age \d+:\d\d:\d\d")
        self.assertNotIn(", up ", report)

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
        self.assertNotIn("no holder is a recognized", self.report())

    def test_a_holder_with_a_busy_child_gets_no_idle_note(self):
        self.start("flock", "--close", str(self.lock), "sh", "-c", "while :; do :; done")
        self.wait_for(self.holds, "holder")
        self.assertNotIn("note:", self.report())

    def test_a_holder_waiting_for_another_lock_says_which(self):
        other = self.root / "other.lock"
        other.touch()
        self.start("flock", str(other), "sleep", "60")
        original = self.lock
        self.lock = other
        self.wait_for(self.holds, "other holder")
        self.lock = original
        self.start("flock", "--close", str(self.lock), "flock", str(other), "true")
        self.wait_for(self.holds, "holder")
        environment = os.environ | {"CAGENTS_BUILD_LOCK": str(other)}
        def report():
            return subprocess.run(
                [sys.executable, "-I", "-B", str(TOOL), str(self.lock)],
                capture_output=True, text=True, timeout=30, env=environment,
            ).stdout
        self.wait_for(lambda: "waiting for" in report(), "the note")
        text = report()
        self.assertIn(f"waiting for {other},", text)
        self.assertNotIn("nothing holding this used CPU", text)

    def test_a_gates_queue_head_waiting_for_a_slot_gets_no_note(self):
        other = self.root / "slot.lock"
        other.touch()
        self.start("flock", str(other), "sleep", "60")
        original = self.lock
        self.lock = other
        self.wait_for(self.holds, "slot holder")
        self.lock = self.root / "gate.lock.queue"
        self.lock.touch()
        queue = self.lock
        self.start("flock", "--close", str(queue), "flock", str(other), "true")
        self.wait_for(self.holds, "queue head")
        self.lock = original
        report = subprocess.run(
            [sys.executable, "-I", "-B", str(TOOL), str(queue)], capture_output=True, text=True,
            timeout=30, env=os.environ | {"CAGENTS_BUILD_LOCK": str(other)},
        ).stdout
        self.assertNotIn(f"waiting for {other}", report)

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
        for name in ("kwin_wayland", "niri", "native-evidence", "rustc", "cagents-cargo-gate", "build-disk-guard",
                     "prune-build-dirs"):
            with self.subTest(name=name):
                program = self.root / name
                program.write_text(f'#!/bin/sh\nexec 9>>"{self.lock}"\nflock 9\nsleep 60 9>&- &\nwait\n')
                program.chmod(0o755)
                self.start(str(program))
                self.wait_for(self.holds, "holder")
                self.assertNotIn("no holder is a recognized", self.report())
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
