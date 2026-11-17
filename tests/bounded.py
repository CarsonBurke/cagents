#!/usr/bin/env python3
"""Contract for scripts/bounded: a workload ends with its owner, its time
limit or its wrapper, and takes everything it started with it."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts/bounded"
# Bounds only a hang: every wait it guards returns as soon as what it waits for happens.
HANG_GUARD = 60


def running(pid):
    try:
        with open(f"/proc/{pid}/stat") as file:
            return file.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False


def wait_for(predicate, timeout=HANG_GUARD):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


class Bounded(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp(prefix="bounded-test-"))
        self.addCleanup(shutil.rmtree, self.directory, True)
        self.pids = self.directory / "pids"
        self.env = os.environ | {"XDG_STATE_HOME": str(self.directory / "state"),
                                 "CAGENTS_BOUNDED_POLL_SECONDS": "0.1",
                                 "CAGENTS_BOUNDED_GRACE_SECONDS": "1"}
        self.env.pop("CAGENTS_BOUNDED", None)
        self.addCleanup(self.kill_leftovers)

    def kill_leftovers(self):
        for pid in self.recorded_pids():
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass

    def recorded_pids(self):
        try:
            return [int(line) for line in self.pids.read_text().split()]
        except OSError:
            return []

    def workload(self, body):
        """A shell workload that records its own pid and its children's in
        `pids`, one of them in a session of its own, outside the group."""
        script = self.directory / "workload"
        script.write_text("#!/bin/sh\n"
                          f'echo $$ >>"{self.pids}"\n'
                          f'setsid sh -c \'echo $$ >>"{self.pids}"; exec sleep 300\' &\n'
                          + body)
        script.chmod(0o755)
        return str(script)

    def start(self, *arguments):
        process = subprocess.Popen([TOOL, *arguments], env=self.env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

        def end():
            if process.poll() is None:
                process.kill()
            process.communicate()
        self.addCleanup(end)
        return process

    def records(self):
        found = sorted((self.directory / "state/cagents-bounded").glob("*.json"))
        return [json.loads(path.read_text()) for path in found]

    def assert_all_stopped(self):
        self.assertTrue(self.recorded_pids(), "the workload never started")
        self.assertTrue(wait_for(lambda: not any(map(running, self.recorded_pids()))),
                        f"still running: {[p for p in self.recorded_pids() if running(p)]}")

    def test_its_exit_status_is_the_commands(self):
        result = subprocess.run([TOOL, "--", "sh", "-c", "exit 7"], env=self.env,
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(self.records(), [])

    def test_what_it_left_running_ends_with_it(self):
        result = subprocess.run([TOOL, "--", self.workload("exit 0\n")], env=self.env,
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_all_stopped()

    def test_the_time_limit_stops_everything_it_started(self):
        process = self.start("--ttl", "1", "--name", "probe", "--", self.workload("sleep 300\n"))
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 124, stderr)
        self.assertRegex(stderr, r"stopped probe after \d+s: its --ttl of 1 seconds passed")
        self.assert_all_stopped()
        self.assertEqual(self.records(), [])

    def test_it_ends_with_its_owner(self):
        owner = subprocess.Popen(["sleep", "300"])
        self.addCleanup(owner.kill)
        process = self.start("--owner", str(owner.pid), "--", self.workload("sleep 300\n"))
        self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 2))
        owner.kill()
        owner.wait()
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn(f"its owner, sleep 300 (pid {owner.pid}), ended", stderr)
        self.assert_all_stopped()

    def test_an_ssh_connection_above_it_is_its_owner(self):
        # sshd's per-connection process exits when the client goes; what it
        # started does not, which is how a probe loop outlived its session.
        connection = self.directory / "sshd-session"
        shutil.copy("/bin/sh", connection)
        shell = subprocess.Popen(
            [connection, "-c", f'"{TOOL}" --ttl 60 -- "{self.workload("sleep 300")}" & wait'],
            env=self.env, stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (shell.poll() is None and shell.kill(), shell.communicate()))
        self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 2 and self.records()))
        self.assertEqual(self.records()[0]["owner"], shell.pid)
        self.assertIn("the SSH connection", self.records()[0]["owner_name"])
        shell.kill()
        shell.wait()
        self.assert_all_stopped()
        self.assertTrue(wait_for(lambda: not self.records()))

    def test_stopping_the_wrapper_stops_the_workload(self):
        process = self.start("--", self.workload("sleep 300\n"))
        self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 2))
        process.terminate()
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn("this wrapper got SIGTERM", stderr)
        self.assert_all_stopped()

    def ignoring_term(self):
        """A workload body that ignores TERM, saying so once it does: a stop
        sent before then would end it without the kill under test."""
        return f"trap '' TERM\n: >\"{self.directory}/ignoring\"\nwhile :; do sleep 0.1; done\n"

    def test_a_workload_that_ignores_term_is_killed(self):
        process = self.start("--", self.workload(self.ignoring_term()))
        self.assertTrue(wait_for((self.directory / "ignoring").exists))
        process.terminate()
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn("it ignored TERM, so it was killed", stderr)
        self.assert_all_stopped()

    def test_a_killed_wrapper_is_listed_and_reaped(self):
        process = self.start("--name", "probe", "--", self.workload("sleep 300\n"))
        self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 2))
        listing = subprocess.run([TOOL, "--list"], env=self.env, capture_output=True, text=True)
        self.assertIn(f"probe, wrapper pid {process.pid}", listing.stdout)
        self.assertIn(f"stop:    kill {process.pid}", listing.stdout)
        process.kill()
        process.wait()
        leader, outside = self.recorded_pids()
        # The group's leader goes with its parent; what left the group can't.
        self.assertTrue(wait_for(lambda: not running(leader)))
        self.assertTrue(running(outside))
        listing = subprocess.run([TOOL, "--list"], env=self.env, capture_output=True, text=True)
        # The leader's foreground sleep is in its group, but nothing signals
        # a group whose wrapper was killed outright.
        self.assertRegex(listing.stdout,
                         r"its wrapper was killed; scripts/bounded --reap stops the 2 processes")
        reaped = subprocess.run([TOOL, "--reap"], env=self.env, capture_output=True, text=True,
                                timeout=HANG_GUARD)
        self.assertEqual(reaped.returncode, 0, reaped.stderr)
        self.assertIn("stopped probe: its wrapper had been killed, leaving 2 processes", reaped.stderr)
        self.assert_all_stopped()
        self.assertEqual(self.records(), [])

    def test_what_cleared_its_environment_but_stayed_in_the_group_goes_too(self):
        unmarked = f'env -i sh -c \'echo $$ >>"{self.pids}"; trap "" TERM; exec sleep 300\' &\n'
        # Until all three have recorded themselves, within the hang guard.
        recorded = (f'i=0; while [ "$(wc -l <"{self.pids}")" -lt 3 ] && [ $i -lt {HANG_GUARD * 10} ]; '
                    'do sleep 0.1; i=$((i + 1)); done\n')
        for name, body, stop in (("after it exits", unmarked + recorded + "exit 0\n", False),
                                 ("when stopped", unmarked + "sleep 300\n", True)):
            with self.subTest(name):
                self.pids.unlink(missing_ok=True)
                process = self.start("--", self.workload(body))
                self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 3))
                if stop:
                    process.terminate()
                process.communicate(timeout=HANG_GUARD)
                self.assert_all_stopped()

    def test_a_command_that_cannot_run_says_so_and_leaves_no_record(self):
        result = subprocess.run([TOOL, "--", str(self.directory / "missing")], env=self.env,
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 127)
        self.assertIn("bounded: cannot run", result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertEqual(self.records(), [])

    def test_an_owner_that_exited_unreaped_has_ended(self):
        owner = subprocess.Popen(["sleep", "infinity"])
        self.addCleanup(owner.wait)
        self.addCleanup(owner.kill)  # Runs first, so a failure cannot leave the wait hanging.
        process = self.start("--owner", str(owner.pid), "--", self.workload("sleep 300\n"))
        # Its record is written once it has found the owner running.
        self.assertTrue(wait_for(self.records))
        owner.kill()  # Not waited for: it stays a zombie.
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn("ended", stderr)
        self.assert_all_stopped()

    def test_a_nested_workload_stays_the_outer_ones(self):
        inner = self.workload(self.ignoring_term())
        process = self.start("--", str(TOOL), "--", inner)
        self.assertTrue(wait_for((self.directory / "ignoring").exists))
        process.terminate()
        _, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 143, stderr)
        self.assert_all_stopped()
        self.assertTrue(wait_for(lambda: not self.records()), (self.records(), stderr))

    def test_it_refuses_what_it_cannot_bound(self):
        for arguments, message in ((["--"], "give the command after --"),
                                   (["--ttl", "0", "--", "true"], "--ttl must be a positive"),
                                   (["--ttl", "nan", "--", "true"], "--ttl must be a positive"),
                                   (["--ttl", "inf", "--", "true"], "--ttl must be a positive"),
                                   (["--owner", "999999999", "--", "true"], "is not running")):
            result = subprocess.run([TOOL, *arguments], env=self.env, capture_output=True,
                                    text=True, timeout=HANG_GUARD)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(message, result.stderr)


if __name__ == "__main__":
    unittest.main()
