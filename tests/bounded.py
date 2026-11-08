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


def running(pid):
    try:
        with open(f"/proc/{pid}/stat") as file:
            return file.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False


def wait_for(predicate, timeout=10.0):
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
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(self.records(), [])

    def test_what_it_left_running_ends_with_it(self):
        result = subprocess.run([TOOL, "--", self.workload("exit 0\n")], env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_all_stopped()

    def test_the_time_limit_stops_everything_it_started(self):
        process = self.start("--ttl", "1", "--name", "probe", "--", self.workload("sleep 300\n"))
        _, stderr = process.communicate(timeout=20)
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
        _, stderr = process.communicate(timeout=20)
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
        _, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn("this wrapper got SIGTERM", stderr)
        self.assert_all_stopped()

    def test_a_workload_that_ignores_term_is_killed(self):
        process = self.start("--ttl", "1", "--", self.workload("trap '' TERM\nwhile :; do sleep 0.1; done\n"))
        _, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 124, stderr)
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
                                timeout=20)
        self.assertEqual(reaped.returncode, 0, reaped.stderr)
        self.assertIn("stopped probe: its wrapper had been killed, leaving 2 processes", reaped.stderr)
        self.assert_all_stopped()
        self.assertEqual(self.records(), [])

    def test_what_cleared_its_environment_but_stayed_in_the_group_goes_too(self):
        unmarked = f'env -i sh -c \'echo $$ >>"{self.pids}"; trap "" TERM; exec sleep 300\' &\n'
        for name, body, arguments in (("after it exits", unmarked + "sleep 0.3\nexit 0\n", []),
                                      ("when stopped", unmarked + "sleep 300\n", ["--ttl", "1"])):
            with self.subTest(name):
                self.pids.unlink(missing_ok=True)
                process = self.start(*arguments, "--", self.workload(body))
                process.communicate(timeout=30)
                self.assertTrue(wait_for(lambda: len(self.recorded_pids()) == 3))
                self.assert_all_stopped()

    def test_a_command_that_cannot_run_says_so_and_leaves_no_record(self):
        result = subprocess.run([TOOL, "--", str(self.directory / "missing")], env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 127)
        self.assertIn("bounded: cannot run", result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertEqual(self.records(), [])

    def test_an_owner_that_exited_unreaped_has_ended(self):
        owner = subprocess.Popen(["sleep", "1"])
        self.addCleanup(owner.wait)
        process = self.start("--ttl", "60", "--owner", str(owner.pid), "--", self.workload("sleep 300\n"))
        _, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 143, stderr)
        self.assertIn("ended", stderr)
        self.assert_all_stopped()

    def test_a_nested_workload_stays_the_outer_ones(self):
        inner = self.workload("trap '' TERM\nwhile :; do sleep 0.1; done\n")
        process = self.start("--ttl", "1", "--", str(TOOL), "--ttl", "60", "--", inner)
        _, stderr = process.communicate(timeout=30)
        self.assertEqual(process.returncode, 124, stderr)
        self.assert_all_stopped()
        self.assertTrue(wait_for(lambda: not self.records()), (self.records(), stderr))

    def test_it_refuses_what_it_cannot_bound(self):
        for arguments, message in ((["--"], "give the command after --"),
                                   (["--ttl", "0", "--", "true"], "--ttl must be a positive"),
                                   (["--ttl", "nan", "--", "true"], "--ttl must be a positive"),
                                   (["--ttl", "inf", "--", "true"], "--ttl must be a positive"),
                                   (["--owner", "999999999", "--", "true"], "is not running")):
            result = subprocess.run([TOOL, *arguments], env=self.env, capture_output=True,
                                    text=True, timeout=20)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(message, result.stderr)


if __name__ == "__main__":
    unittest.main()
