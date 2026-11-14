#!/usr/bin/env python3
"""Exercise scripts/build-host with private share locks and stand-in tools.

Every build here runs in place (CAGENTS_BUILD_SCOPE=0): no test makes a
systemd scope or starts the host's jobserver. The jobserver loop runs in a
thread of this process, on a private directory, with what holds its pipes
stated by the test.
"""

import array
import contextlib
import fcntl
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import termios
import threading
import time
import types
import unittest
import unittest.mock


ROOT = Path(__file__).resolve().parents[1]
# Isolated Python (-I) leaves this directory off sys.path.
_custody_spec = importlib.util.spec_from_file_location("fixture_custody", ROOT / "tests/fixture_custody.py")
fixture_custody = importlib.util.module_from_spec(_custody_spec)
_custody_spec.loader.exec_module(fixture_custody)
FixtureDirectory = fixture_custody.FixtureDirectory

_loader = importlib.machinery.SourceFileLoader("build_host", str(ROOT / "scripts/build-host"))
build_host = importlib.util.module_from_spec(importlib.util.spec_from_loader("build_host", _loader))
_loader.exec_module(build_host)

# Records each call, with which of the fixture's shares were held and the
# build's environment, then waits for its release file if one is named.
RECORDER = r'''#!/usr/bin/python3
import fcntl, json, os, sys, time
from pathlib import Path
root = Path(os.environ["FIXTURE_ROOT"])
lock = os.environ["CAGENTS_BUILD_LOCK"]
count = int(os.environ["CAGENTS_BUILD_SLOTS"])
held = []
for index, path in enumerate([lock] + [f"{lock}.{n}" for n in range(1, count)]):
    with open(path, "a") as share:
        try:
            fcntl.flock(share, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            held.append(index)
entry = dict(program=Path(sys.argv[0]).name, arguments=sys.argv[1:], held=held, pid=os.getpid(),
             parent=os.getppid(),
             environment={name: os.environ.get(name) for name in (
                 "CAGENTS_BUILD_SLOT_HELD", "CARGO_BUILD_JOBS", "MAKEFLAGS",
                 "CAGENTS_TEST_RETAINED_ACTOR_HELPER")})
with (root / "calls.jsonl").open("a") as calls:
    calls.write(json.dumps(entry) + "\n")
if entry["program"] == "memory-capped":
    command = sys.argv[1:]
    while command[:1] in (["--name"], ["--slice"]):
        command = command[2:]
    os.execvp(command[0], command)
if entry["program"] == "cargo-host-test-helper.py":
    print(os.environ.get("FIXTURE_HELPER_OUTPUT", ""))
    raise SystemExit(int(os.environ.get("FIXTURE_HELPER_EXIT", "0")))
release = os.environ.get("FIXTURE_HOLD")
if release:
    (root / f"{release}.started").touch()
    while not (root / release).exists():
        time.sleep(0.01)
raise SystemExit(int(os.environ.get("FIXTURE_EXIT", "0")))
'''


class BuildHostFixture(unittest.TestCase):
    """A private copy of build-host, its shares and a recording Cargo."""

    slots = 2

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-build-host-")
        self.addCleanup(self.scope.close)
        self.root = Path(self.scope.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.build_host = scripts / "build-host"
        shutil.copy2(ROOT / "scripts/build-host", self.build_host)
        self.cargo = self.root / "bin/cargo"
        self.cargo.parent.mkdir()
        self.write_tool(self.cargo)
        # build-host runs the memory limit with its own Python, as it would
        # the real one.
        self.write_tool(scripts / "memory-capped")
        self.lock = self.root / "slot.lock"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("CAGENTS_", "CARGO_"))
                    and key not in ("MAKEFLAGS", "MFLAGS")} | {
            "CAGENTS_BUILD_LOCK": str(self.lock), "CAGENTS_BUILD_SLOTS": str(self.slots),
            "CAGENTS_BUILD_SCOPE": "0", "FIXTURE_ROOT": str(self.root)}
        self.holders = []
        self.addCleanup(self.release_holders)

    def write_tool(self, path):
        path.write_text(RECORDER.replace("/usr/bin/python3", sys.executable, 1))
        path.chmod(0o755)

    def release_holders(self):
        for holder in self.holders:
            holder.close()

    def share(self, index):
        return self.lock if index == 0 else Path(f"{self.lock}.{index}")

    def hold(self, path):
        """Takes the lock on `path` in this process, as another build would."""
        holder = Path(path).open("a")
        fcntl.flock(holder, fcntl.LOCK_EX)
        self.holders.append(holder)
        return holder

    def is_locked(self, path):
        with Path(path).open("a") as probe:
            try:
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
        return False

    def wait_for(self, predicate, label, timeout=5):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                self.fail(f"timed out waiting for {label}")
            time.sleep(0.01)

    def start(self, kind, *arguments, env=None):
        return self.scope.popen([str(self.build_host), kind, *arguments], env=env or self.env,
                                cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def run_host(self, kind, *arguments, env=None):
        return self.scope.run([str(self.build_host), kind, *arguments], env=env or self.env,
                              cwd=self.root, capture_output=True, text=True, timeout=20)

    def held_build(self, name, shares, *options):
        """A `build` holding `shares` until the file `name` exists."""
        return self.start("build", "--shares", str(shares), *options, "--",
                          str(self.cargo), name, env=self.env | {"FIXTURE_HOLD": name})

    def started(self, name):
        return (self.root / f"{name}.started").exists()

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def finish(self, process, name=None):
        if name:
            (self.root / name).touch()
        stdout, stderr = process.communicate(timeout=10)
        return process.returncode, stdout, stderr


class Admission(BuildHostFixture):
    """A build waits only for the shares it needs, in arrival order."""

    slots = 3

    def test_a_build_whose_shares_are_free_starts_without_waiting(self):
        build = self.held_build("first", 2)
        self.wait_for(lambda: self.started("first"), "the build")
        self.assertEqual(self.calls()[0]["held"], [0, 1])
        status, _stdout, stderr = self.finish(build, "first")
        self.assertEqual(status, 0, stderr)
        self.assertNotIn("waiting", stderr)
        self.assertFalse(any(self.is_locked(self.share(index)) for index in range(self.slots)))

    def test_a_build_needing_two_waits_while_one_is_free_and_starts_when_freed(self):
        first, second = self.hold(self.share(0)), self.hold(self.share(1))
        build = self.held_build("build", 2)
        self.wait_for(lambda: self.is_locked(self.share(2)), "the build to take the free share")
        time.sleep(0.4)
        self.assertFalse(self.started("build"), "two shares were needed and one was free")
        first.close()
        self.wait_for(lambda: self.started("build"), "the build after a share freed")
        self.assertEqual(sorted(self.calls()[0]["held"]), [0, 1, 2])
        status, _stdout, stderr = self.finish(build, "build")
        self.assertEqual(status, 0, stderr)
        self.assertIn("waiting for 2 of the 3 build memory shares", stderr)
        # What it held goes back; what another holds stays held.
        self.assertTrue(self.is_locked(self.share(1)))
        self.assertFalse(self.is_locked(self.share(0)) or self.is_locked(self.share(2)))
        second.close()

    def test_the_head_of_the_queue_is_not_overtaken_by_a_smaller_build(self):
        for options in ((), ("--wait-seconds", "30")):
            with self.subTest(waiting_with_a_deadline=bool(options)):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                first, second = self.hold(self.share(0)), self.hold(self.share(1))
                head = self.held_build("head", 3)
                self.wait_for(lambda: self.is_locked(self.share(2)), "the head's partial shares")
                behind = self.held_build("behind", 1, *options)
                self.wait_for(lambda: self.opens(behind.pid, f"{self.lock}.queue"),
                              "the smaller build to queue")
                # A freed share goes to the head, never to the smaller build.
                first.close()
                self.wait_for(lambda: self.is_locked(self.share(0)), "the head to take a freed share")
                time.sleep(0.4)
                self.assertFalse(self.started("behind"), "a smaller build overtook the queue's head")
                second.close()
                self.wait_for(lambda: self.started("head"), "the head")
                time.sleep(0.3)
                self.assertFalse(self.started("behind"), "the head holds every share")
                self.assertEqual(self.finish(head, "head")[0], 0)
                self.wait_for(lambda: self.started("behind"), "the smaller build after the head")
                self.assertEqual(self.finish(behind, "behind")[0], 0)
                self.assertEqual([call["arguments"] for call in self.calls()], [["head"], ["behind"]])
                for name in ("head", "behind"):
                    for path in (self.root / name, self.root / f"{name}.started"):
                        path.unlink()

    def test_waiters_start_in_the_order_they_arrived(self):
        env = self.env | {"CAGENTS_BUILD_SLOTS": "1"}
        for options in ((), ("--wait-seconds", "30")):
            with self.subTest(waiting_with_a_deadline=bool(options)):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                share = self.hold(self.lock)
                builds = []
                for name in ("first", "second", "third"):
                    builds.append(self.start("build", *options, "--", str(self.cargo), name,
                                             env=env | {"FIXTURE_HOLD": name}))
                    # Each is in line before the next arrives: the first at
                    # the head, holding the queue, the others in the kernel's
                    # queue of its waiters.
                    if name == "first":
                        self.wait_for(lambda: self.is_locked(f"{self.lock}.queue"), "the head")
                    else:
                        self.wait_for(lambda: self.blocked_on_a_lock(builds[-1].pid), f"the {name} to queue")
                share.close()
                # One share: each runs alone, until the test lets it finish.
                for name, build in zip(("first", "second", "third"), builds):
                    self.wait_for(lambda: self.started(name), name)
                    self.assertEqual([call["arguments"] for call in self.calls()][-1], [name])
                    self.assertEqual(self.finish(build, name)[0], 0)
                for name in ("first", "second", "third"):
                    for path in (self.root / name, self.root / f"{name}.started"):
                        path.unlink()

    def blocked_on_a_lock(self, pid):
        with contextlib.suppress(FileNotFoundError, ProcessLookupError):
            return (self.opens(pid, f"{self.lock}.queue")
                    and "lock" in Path(f"/proc/{pid}/wchan").read_text())
        return False

    def opens(self, pid, path):
        target = os.path.realpath(path)
        with contextlib.suppress(FileNotFoundError, ProcessLookupError):
            for descriptor in Path(f"/proc/{pid}/fd").iterdir():
                with contextlib.suppress(FileNotFoundError):
                    if os.readlink(descriptor) == target:
                        return True
        return False

    def test_a_deadline_expires_with_124_and_gives_back_only_what_it_took(self):
        for options in (("--wait-seconds", "1"),
                        ("--deadline", str(time.monotonic_ns() + 600_000_000))):
            with self.subTest(option=options[0]):
                other = self.hold(self.share(1))
                build = self.held_build("never", 3, *options)
                status, _stdout, stderr = self.finish(build)
                self.assertEqual(status, 124, stderr)
                self.assertIn("cargo-gate: build admission timed out", stderr)
                self.assertFalse(self.started("never"))
                self.assertTrue(self.is_locked(self.share(1)), "another build's share was released")
                for path in (self.share(0), self.share(2), f"{self.lock}.queue"):
                    self.assertFalse(self.is_locked(path), f"{path} stayed held")
                other.close()

    def test_a_deadline_expires_behind_the_queues_holder_without_taking_it(self):
        queue = self.hold(f"{self.lock}.queue")
        build = self.held_build("never", 1, "--deadline", str(time.monotonic_ns() + 500_000_000))
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, 124, stderr)
        self.assertEqual(stderr.count("build admission timed out"), 1, stderr)
        self.assertFalse(self.started("never"))
        self.assertFalse(self.is_locked(self.lock))
        queue.close()

    def test_a_build_with_no_shares_does_not_wait(self):
        for index in range(self.slots):
            self.hold(self.share(index))
        self.hold(f"{self.lock}.queue")
        result = self.run_host("build", "--shares", "0", "--", str(self.cargo), "nested")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)
        self.assertEqual([call["arguments"] for call in self.calls()], [["nested"]])

    def test_more_shares_than_the_host_has_take_them_all(self):
        result = self.run_host("build", "--shares", "9", "--", str(self.cargo), "all")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[0]["held"], [0, 1, 2])

    def test_invalid_options_and_share_counts_are_refused(self):
        for arguments, env, message in (
                (("build", "--shares", "x", "--", "true"), None, "--shares needs a whole number"),
                (("build", "--deadline", "-1", "--", "true"), None, "--deadline needs a whole number"),
                (("build", "--wait-seconds", "1.5", "--", "true"), None, "--wait-seconds needs a whole number"),
                (("build", "--unknown", "1", "--", "true"), None, "usage: build-host"),
                (("build", "--shares", "1", "--"), None, "usage: build-host"),
                (("other", "--", "true"), None, "usage: build-host"),
                (("build", "--", "true"), self.env | {"CAGENTS_BUILD_SLOTS": "0"},
                 "CAGENTS_BUILD_SLOTS must be a positive integer")):
            with self.subTest(arguments=arguments, env=bool(env)):
                result = self.run_host(*arguments, env=env)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(message, result.stderr)
        self.assertEqual(self.calls(), [])


class TheBuildsEnvironment(BuildHostFixture):
    """What a build runs with, outside the jobserver."""

    def test_a_build_marks_its_commands_as_inside_its_shares(self):
        result = self.run_host("build", "--", str(self.cargo), "marked")
        self.assertEqual(result.returncode, 0, result.stderr)
        call = self.calls()[0]
        marker = call["environment"]["CAGENTS_BUILD_SLOT_HELD"]
        # The command is build-host's own child, and the marker names it, as
        # the gate's inside_gate check reads it.
        self.assertEqual(int(marker), call["parent"])
        self.assertNotEqual(call["parent"], os.getpid())

    def test_a_build_in_place_gets_cargo_jobs_unless_the_caller_set_them(self):
        cpus = os.cpu_count() or 1
        for env, expected in ((self.env, str(max(2, min(8, cpus // 2)))),
                              (self.env | {"CARGO_BUILD_JOBS": "3"}, "3")):
            with self.subTest(expected=expected):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                result = self.run_host("build", "--", str(self.cargo), "jobs", env=env)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = self.calls()[0]["environment"]
                self.assertEqual(environment["CARGO_BUILD_JOBS"], expected)
                self.assertIsNone(environment["MAKEFLAGS"])

    def test_the_commands_status_is_the_builds(self):
        result = self.run_host("build", "--", str(self.cargo), env=self.env | {"FIXTURE_EXIT": "17"})
        self.assertEqual(result.returncode, 17)
        result = self.run_host("build", "--", "/bin/sh", "-c", "kill -USR1 $$")
        self.assertEqual(result.returncode, 128 + signal.SIGUSR1)
        result = self.run_host("build", "--", str(self.root / "missing"))
        self.assertEqual(result.returncode, 127, result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertFalse(self.is_locked(self.lock))

    def test_a_directory_to_remove_goes_when_the_build_ends(self):
        directory = self.root / "shims"
        for env, status in ((self.env, 0), (self.env | {"FIXTURE_EXIT": "5"}, 5)):
            with self.subTest(status=status):
                directory.mkdir()
                (directory / "cargo").write_text("")
                result = self.run_host("build", "--remove", str(directory), "--", str(self.cargo), env=env)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertFalse(directory.exists())

    def test_the_share_count_is_the_setting_or_the_hosts(self):
        result = self.run_host("count")
        self.assertEqual((result.returncode, result.stdout), (0, f"{self.slots}\n"), result.stderr)
        env = {key: value for key, value in self.env.items() if key != "CAGENTS_BUILD_SLOTS"}
        result = self.run_host("count", env=env)
        memory = os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
        # /proc/meminfo's total is a little below the physical pages'.
        self.assertIn(int(result.stdout), range(2, max(2, memory * 3 // 4 // 2**32) + 1))


class CargoPhases(BuildHostFixture):
    """`test`, `bench` and `run` build with the shares and run without them."""

    def phases(self, *arguments, options=(), env=None):
        result = self.run_host("cargo", "--shares", "2", *options, "--", str(self.cargo), *arguments,
                               env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return [(call["program"], call["arguments"], call["held"]) for call in self.calls()]

    def test_tests_and_benchmarks_build_then_run_under_the_memory_limit_without_shares(self):
        for subcommand in ("test", "bench"):
            with self.subTest(subcommand):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual(self.phases(subcommand, "--lib", "--", "filter", "--no-run"), [
                    ("cargo", [subcommand, "--no-run", "--lib"], [0, 1]),
                    ("memory-capped", ["--slice", "app-cagents-test.slice", str(self.cargo),
                                       subcommand, "--lib", "--", "filter", "--no-run"], []),
                    ("cargo", [subcommand, "--lib", "--", "filter", "--no-run"], []),
                ])

    def test_a_program_builds_then_runs_without_shares_or_the_limit(self):
        self.assertEqual(self.phases("run", "--bin", "app", "--", "argument with spaces"), [
            ("cargo", ["build", "--bin", "app"], [0, 1]),
            ("cargo", ["run", "--bin", "app", "--", "argument with spaces"], []),
        ])

    def test_a_toolchain_argument_stays_before_the_subcommand(self):
        self.assertEqual([arguments for _, arguments, _ in self.phases("+nightly", "test", "--lib")],
                         [["+nightly", "test", "--no-run", "--lib"],
                          ["--slice", "app-cagents-test.slice", str(self.cargo), "+nightly", "test", "--lib"],
                          ["+nightly", "test", "--lib"]])

    def test_doctests_build_and_run_in_one_within_the_shares_under_the_limit(self):
        # Cargo cannot build doctests without running them.
        for arguments in (("test", "--doc"), ("+nightly", "test", "--doc", "--", "filter")):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual(self.phases(*arguments), [
                    ("memory-capped", ["--slice", "app-cagents-test.slice", str(self.cargo), *arguments], [0, 1]),
                    ("cargo", list(arguments), [0, 1]),
                ])

    def test_a_doc_or_no_run_after_the_separator_belongs_to_the_tests(self):
        for argument in ("--doc", "--no-run"):
            with self.subTest(argument):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual([program for program, _, _ in self.phases("test", "--", argument)],
                                 ["cargo", "memory-capped", "cargo"])

    def test_what_only_builds_runs_in_one_without_the_limit(self):
        for arguments in (("test", "--no-run", "--lib"), ("bench", "--no-run"),
                          ("run", "--no-run"), ("build", "--lib"), ("check",)):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual(self.phases(*arguments), [("cargo", list(arguments), [0, 1])])

    def test_a_failed_build_runs_nothing(self):
        result = self.run_host("cargo", "--", str(self.cargo), "test", "--lib",
                               env=self.env | {"FIXTURE_EXIT": "101"})
        self.assertEqual(result.returncode, 101)
        self.assertEqual([call["arguments"] for call in self.calls()], [["test", "--no-run", "--lib"]])

    def test_the_runner_goes_after_the_subcommand_of_what_runs_programs(self):
        runner = "target.'cfg(all())'.runner=[\"env\"]"
        self.assertEqual([arguments for _, arguments, _ in self.phases("test", "--lib", options=("--runner", runner))],
                         [["test", "--no-run", "--config", runner, "--lib"],
                          ["--slice", "app-cagents-test.slice", str(self.cargo), "test", "--config", runner, "--lib"],
                          ["test", "--config", runner, "--lib"]])
        for arguments, expected in ((("+nightly", "run"), ["+nightly", "build", "--config", runner]),
                                    (("build",), ["build"])):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual(self.phases(*arguments, options=("--runner", runner))[0][1], expected)

    def test_the_host_test_helper_runs_first_within_the_shares(self):
        helper = self.root / "scripts/cargo-host-test-helper.py"
        self.write_tool(helper)
        calls = self.phases("test", "--lib", env=self.env | {"FIXTURE_HELPER_OUTPUT": "retained-helper"})
        self.assertEqual(calls[0], ("cargo-host-test-helper.py", [str(self.cargo), "test", "--lib"], [0, 1]))
        self.assertEqual([program for program, _, _ in calls[1:]], ["cargo", "memory-capped", "cargo"])
        self.assertEqual(self.calls()[-1]["environment"]["CAGENTS_TEST_RETAINED_ACTOR_HELPER"],
                         "retained-helper")
        # A helper that fails stops the test before it builds.
        (self.root / "calls.jsonl").unlink()
        result = self.run_host("cargo", "--", str(self.cargo), "test",
                               env=self.env | {"FIXTURE_HELPER_EXIT": "3"})
        self.assertEqual(result.returncode, 3)
        self.assertEqual([call["program"] for call in self.calls()], ["cargo-host-test-helper.py"])
        # Nor does it run for a test that is only built, or again for one
        # whose caller already has it.
        for arguments, env in ((("test", "--no-run"), self.env),
                               (("test",), self.env | {"CAGENTS_TEST_RETAINED_ACTOR_HELPER": "given"})):
            with self.subTest(arguments=arguments):
                (self.root / "calls.jsonl").unlink()
                self.assertNotIn("cargo-host-test-helper.py",
                                 [program for program, _, _ in self.phases(*arguments, env=env)])

    def test_the_real_memory_limit_takes_the_test_slice(self):
        capped = self.root / "scripts/memory-capped"
        capped.unlink()
        shutil.copy2(ROOT / "scripts/memory-capped", capped)
        # A zero limit runs the command as it is, without a scope.
        calls = self.phases("test", "--lib", env=self.env | {"CAGENTS_MEMORY_MAX": "0"})
        self.assertEqual(calls[-1], ("cargo", ["test", "--lib"], []))


class Interruption(BuildHostFixture):
    """A signal to build-host reaches its command as Ctrl-C would."""

    COMMAND = r'''
import signal, sys, time
from pathlib import Path
root = Path(sys.argv[1])
for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
    signal.signal(number, lambda number, _frame: (
        (root / "received").write_text(signal.Signals(number).name), sys.exit(130)))
(root / "ready").touch()
while True:
    time.sleep(0.05)
'''

    def test_a_signal_reaches_the_command_as_an_interrupt_first(self):
        for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            with self.subTest(signal=number):
                for name in ("ready", "received"):
                    (self.root / name).unlink(missing_ok=True)
                build = self.start("build", "--shares", "2", "--", sys.executable, "-I", "-c",
                                   self.COMMAND, str(self.root))
                self.wait_for((self.root / "ready").exists, "the command")
                build.send_signal(number)
                status, _stdout, stderr = self.finish(build)
                self.assertEqual((self.root / "received").read_text(), "SIGINT")
                # It ends as the signal it was sent would end it.
                self.assertEqual(status, -number, stderr)
                self.assertFalse(self.is_locked(self.lock) or self.is_locked(self.share(1)))

    def test_the_interrupt_reaches_the_commands_whole_group(self):
        # A script's own commands get it too, as they would from a terminal.
        build = self.start("build", "--", "/bin/sh", "-c", '"$@" & wait', "script",
                           sys.executable, "-I", "-c", self.COMMAND, str(self.root))
        self.wait_for((self.root / "ready").exists, "the script's command")
        build.send_signal(signal.SIGTERM)
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, -signal.SIGTERM, stderr)
        self.assertEqual((self.root / "received").read_text(), "SIGINT")

    def test_what_ignores_the_interrupt_ends_with_the_command(self):
        # A shell's background job ignores Ctrl-C; once the shell has gone,
        # nothing would be left to end it.
        build = self.start("build", "--", "/bin/sh", "-c", 'sleep 30 & echo $! >"$1"; wait', "script",
                           str(self.root / "background"))
        self.wait_for((self.root / "background").exists, "the background job")
        self.wait_for(lambda: (self.root / "background").read_text().strip(), "its process ID")
        background = os.pidfd_open(int((self.root / "background").read_text()))
        self.addCleanup(os.close, background)
        build.send_signal(signal.SIGTERM)
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, -signal.SIGTERM, stderr)
        self.assertTrue(select.select([background], [], [], 2)[0], "the background job outlived its build")

    def test_a_signal_while_waiting_ends_the_wait_and_releases_only_its_own(self):
        other = self.hold(self.lock)
        directory = self.root / "shims"
        directory.mkdir()
        build = self.start("build", "--shares", "2", "--remove", str(directory), "--",
                           str(self.cargo), "never")
        self.wait_for(lambda: self.is_locked(self.share(1)), "the build's partial share")
        build.send_signal(signal.SIGTERM)
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, -signal.SIGTERM, stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse(directory.exists())
        self.assertTrue(self.is_locked(self.lock), "another build's share was released")
        self.assertFalse(self.is_locked(self.share(1)) or self.is_locked(f"{self.lock}.queue"))
        other.close()

    def test_a_command_ignoring_the_interrupt_is_killed_after_its_grace(self):
        # In this process, with build-host's clock fifty times as fast, so
        # its five-second grace passes in a tenth of one.
        ready = self.root / "ready"
        command = ["/bin/sh", "-c", 'trap "" INT; touch "$1"; exec sleep 30', "ignoring", str(ready)]
        clock = types.SimpleNamespace(monotonic=lambda: time.monotonic() * 50,
                                      monotonic_ns=time.monotonic_ns, sleep=time.sleep)
        early = []
        previous = signal.signal(signal.SIGTERM, lambda *_: early.append(True))
        self.addCleanup(signal.signal, signal.SIGTERM, previous)

        def interrupt():
            deadline = time.monotonic() + 5
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            time.sleep(0.1)
            os.kill(os.getpid(), signal.SIGTERM)

        sender = threading.Thread(target=interrupt)
        sender.start()
        started = time.monotonic()
        with unittest.mock.patch.object(build_host, "time", clock):
            with self.assertRaises(build_host.Interrupted) as raised:
                build_host.run(command, self.env, scoped=False)
        sender.join()
        self.assertEqual(early, [])
        self.assertEqual(raised.exception.number, signal.SIGTERM)
        self.assertLess(time.monotonic() - started, 3)
        self.assertIs(signal.getsignal(signal.SIGTERM).__class__, types.FunctionType)


class Slice(unittest.TestCase):
    """Builds go in systemd's low-weight background.slice, under one memory cap."""

    def prepare(self, shown, changed=0, scope=None):
        calls = []

        def systemctl(*arguments):
            calls.append(arguments)
            if arguments[0] == "show":
                return subprocess.CompletedProcess(arguments, shown[0], f"MemoryHigh={shown[1]}\n", "")
            return subprocess.CompletedProcess(arguments, changed, "", "")

        with unittest.mock.patch.object(build_host, "systemctl", systemctl), \
                unittest.mock.patch.object(build_host, "budget", lambda: 3 * 2**30 + 1), \
                unittest.mock.patch.dict(os.environ):
            os.environ.pop("CAGENTS_BUILD_SCOPE", None)
            if scope is not None:
                os.environ["CAGENTS_BUILD_SCOPE"] = scope
            return build_host.prepare_slice(), calls

    def test_the_build_slice_is_in_systemds_background_slice(self):
        # The parent is the dash-separated prefix; app.slice holds sessions and tests.
        self.assertEqual(build_host.BUILD_SLICE.split("-")[0], "background")
        self.assertEqual(build_host.TEST_SLICE.split("-")[0], "app")

    def test_the_cap_is_set_only_when_it_differs(self):
        self.assertEqual(self.prepare((0, "infinity")), (True, [
            ("show", "background-cagents.slice", "--property=MemoryHigh"),
            ("set-property", "--runtime", "background-cagents.slice", f"MemoryHigh={3 * 2**30}")]))
        self.assertEqual(self.prepare((0, str(3 * 2**30))),
                         (True, [("show", "background-cagents.slice", "--property=MemoryHigh")]))

    def test_without_a_user_manager_or_with_scopes_off_builds_run_in_place(self):
        self.assertEqual(self.prepare((1, ""))[0], False)
        self.assertEqual(self.prepare((0, "infinity"), changed=1)[0], False)
        self.assertEqual(self.prepare((0, "infinity"), scope="0"),
                         (False, []))


def waiting(descriptor):
    count = array.array("i", [0])
    fcntl.ioctl(descriptor, termios.FIONREAD, count)
    return count[0]


class Jobserver(unittest.TestCase):
    """The jobserver loop, in a thread, on a private directory."""

    TOKENS = 4

    def setUp(self):
        self.directory = Path(tempfile.mkdtemp(prefix="cagents-jobserver-"))
        self.addCleanup(shutil.rmtree, self.directory, ignore_errors=True)
        # The users locks this test holds for generations, as builds would.
        self.users = {}
        self.addCleanup(self.leave_all)
        clock = types.SimpleNamespace(monotonic=time.monotonic, monotonic_ns=time.monotonic_ns,
                                      sleep=lambda seconds: time.sleep(min(seconds, 0.05)))
        for name, value in (("TICK", 0.02), ("ROTATE", 60.0), ("IDLE_EXIT", 60.0), ("time", clock)):
            patcher = unittest.mock.patch.object(build_host, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.descriptors = {}
        self.addCleanup(lambda: [os.close(fd) for fd in self.descriptors.values()])
        self.result = []
        self.thread = None

    def serve(self, **settings):
        for name, value in settings.items():
            setattr(build_host, name, value)
        self.thread = threading.Thread(
            target=lambda: self.result.append(build_host.serve(str(self.directory), self.TOKENS)),
            daemon=True)
        self.thread.start()
        self.addCleanup(self.stop)
        self.wait_for((self.directory / "pid").exists, "the jobserver")
        self.assertEqual(int((self.directory / "pid").read_text()), os.getpid())
        return self.current()

    def use(self, path):
        """Holds the generation's users lock, as a build that joined it does."""
        descriptor = os.open(path + ".users", os.O_RDONLY)
        fcntl.flock(descriptor, fcntl.LOCK_SH)
        self.users[path] = descriptor

    def leave(self, path):
        os.close(self.users.pop(path))

    def leave_all(self):
        for path in list(self.users):
            self.leave(path)

    def stop(self):
        self.leave_all()
        build_host.IDLE_EXIT = 0
        self.thread.join(timeout=5)
        self.assertFalse(self.thread.is_alive(), "the jobserver did not exit")

    def current(self):
        return os.path.realpath(self.directory / "current")

    def open(self, path):
        """This test's own descriptor on a generation, as a build's."""
        if path not in self.descriptors:
            self.descriptors[path] = os.open(path, os.O_RDWR | os.O_NONBLOCK)
        return self.descriptors[path]

    def take(self, path, count):
        taken = len(os.read(self.open(path), count))
        self.assertEqual(taken, count)
        return taken

    def give_back(self, path, count):
        os.write(self.open(path), b"+" * count)

    def in_pipes(self):
        return sum(waiting(fd) for fd in self.descriptors.values())

    def wait_for(self, predicate, label, timeout=5, held=None):
        """Waits for `predicate`, checking meanwhile that the pipes and
        `held`, the tokens builds still have, never exceed the count."""
        deadline = time.monotonic() + timeout
        while True:
            if held is not None:
                self.assertLessEqual(self.in_pipes() + held(), self.TOKENS, label)
            if predicate():
                return
            if time.monotonic() >= deadline:
                self.fail(f"timed out waiting for {label}")
            time.sleep(0.005)

    def test_tokens_no_build_has_open_are_made_up(self):
        path = self.serve()
        descriptor = self.open(path)
        self.assertEqual(waiting(descriptor), self.TOKENS)
        # A build killed with three tokens; nothing has the pipe open now.
        self.take(path, 3)
        self.wait_for(lambda: waiting(descriptor) == self.TOKENS, "the lost tokens made up")
        # Tokens it gives back late are more than there should be.
        self.give_back(path, 3)
        self.wait_for(lambda: waiting(descriptor) == self.TOKENS, "the surplus taken back")
        self.assertEqual(self.current(), path)

    def test_tokens_out_with_a_build_are_not_made_up_before_it_moves_on(self):
        path = self.serve()
        self.use(path)
        self.take(path, 2)
        time.sleep(0.3)
        self.assertEqual(waiting(self.open(path)), 2)
        self.assertEqual(self.current(), path, "rotated before its time")
        self.give_back(path, 2)
        time.sleep(0.1)
        self.assertEqual(waiting(self.open(path)), self.TOKENS)

    def test_tokens_out_too_long_move_builds_to_a_new_generation(self):
        old = self.serve(ROTATE=0.1)
        self.use(old)
        held = [3]
        self.take(old, 3)
        self.wait_for(lambda: self.current() != old, "a new generation", held=lambda: held[0])
        new = self.current()
        # What was waiting in the old pipe moved to the new one.
        self.wait_for(lambda: waiting(self.open(new)) == 1, "the waiting token to move",
                      held=lambda: held[0])
        self.assertEqual(waiting(self.open(old)), 0)
        # A token the old generation's build gives back moves across too.
        self.give_back(old, 1)
        held[0] = 2
        self.wait_for(lambda: waiting(self.open(new)) == 2 and waiting(self.open(old)) == 0,
                      "the returned token to move", held=lambda: held[0])
        # While a build still has the old one open, its tokens may come back.
        self.use(new)
        time.sleep(0.2)
        self.assertEqual(waiting(self.open(new)), 2, "tokens made up while their build still ran")
        self.assertEqual(self.current(), new, "a second generation while one is retiring")
        self.assertTrue(os.path.exists(old))
        # Its build is gone, with the two it held: they are made up once.
        self.leave(old)
        held[0] = 0
        self.wait_for(lambda: not os.path.exists(old), "the old generation to go", held=lambda: held[0])
        self.wait_for(lambda: waiting(self.open(new)) == self.TOKENS, "the lost tokens made up",
                      held=lambda: held[0])
        time.sleep(0.2)
        self.assertEqual(self.in_pipes(), self.TOKENS)
        self.assertEqual(self.current(), new)

    def test_an_idle_jobserver_exits_and_removes_what_names_it(self):
        path = self.serve(IDLE_EXIT=0.2)
        self.use(path)
        time.sleep(0.4)
        self.assertTrue(self.thread.is_alive(), "it exited while a build had its pipe")
        self.leave_all()
        self.thread.join(timeout=5)
        self.assertEqual(self.result, [0])
        for name in ("pid", "current", os.path.basename(path), os.path.basename(path) + ".users"):
            self.assertFalse((self.directory / name).exists(), name)

    def test_a_build_that_joins_uses_the_generation_until_it_ends(self):
        path = self.serve()
        with unittest.mock.patch.object(build_host, "jobserver_directory", lambda: str(self.directory)), \
                unittest.mock.patch.object(build_host, "jobserver_alive", lambda _directory: True):
            joined = build_host.join_jobserver()
        self.assertIsNotNone(joined)
        descriptors, joined_path = joined
        self.assertEqual(joined_path, path)
        # A build holding tokens, which the jobserver must not make up.
        self.take(path, 3)
        time.sleep(0.3)
        self.assertEqual(waiting(self.open(path)), 1)
        # Its processes end, and the tokens with them.
        for descriptor in descriptors:
            os.close(descriptor)
        self.wait_for(lambda: waiting(self.open(path)) == self.TOKENS, "the lost tokens made up")


class JoiningTheJobserver(unittest.TestCase):
    """A build joins a running jobserver's current generation."""

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-jobserver-join-")
        self.addCleanup(self.scope.close)
        self.directory = Path(self.scope.name) / "jobserver"
        self.directory.mkdir()
        patcher = unittest.mock.patch.object(build_host, "jobserver_directory", lambda: str(self.directory))
        patcher.start()
        self.addCleanup(patcher.stop)
        # Starting one would need the user manager.
        patcher = unittest.mock.patch.object(build_host.subprocess, "run",
                                             side_effect=AssertionError("started a jobserver"))
        patcher.start()
        self.addCleanup(patcher.stop)
        # A process whose arguments name the directory, as the jobserver's do.
        self.server = self.scope.popen([sys.executable, "-I", "-c", "import time; time.sleep(60)",
                                        str(self.directory)])
        (self.directory / "pid").write_text(f"{self.server.pid}\n")
        self.wait_for_arguments()

    def wait_for_arguments(self):
        deadline = time.monotonic() + 5
        while str(self.directory).encode() not in Path(f"/proc/{self.server.pid}/cmdline").read_bytes():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.01)

    def test_a_build_joins_the_current_generation(self):
        fifo = self.directory / "fifo-3"
        os.mkfifo(fifo, 0o600)
        (self.directory / "fifo-3.users").touch()
        (self.directory / "current").symlink_to("fifo-3")
        joined = build_host.join_jobserver()
        self.assertIsNotNone(joined)
        descriptors, path = joined
        self.addCleanup(lambda: [os.close(descriptor) for descriptor in descriptors])
        self.assertEqual(path, str(fifo))
        self.assertTrue(all(os.get_inheritable(descriptor) for descriptor in descriptors))
        # It holds the generation's users lock shared.
        with open(self.directory / "fifo-3.users") as users:
            with self.assertRaises(BlockingIOError):
                fcntl.flock(users, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(users, fcntl.LOCK_SH | fcntl.LOCK_NB)
        environment = build_host.build_environment({"MFLAGS": "-s", "CARGO_BUILD_JOBS": "5"}, path)
        self.assertEqual(environment["MAKEFLAGS"], f"-j --jobserver-auth=fifo:{fifo}")
        self.assertEqual(environment["CARGO_MAKEFLAGS"], environment["MAKEFLAGS"])
        self.assertNotIn("MFLAGS", environment)
        self.assertEqual(environment["CARGO_BUILD_JOBS"], "5")
        self.assertEqual(environment["CAGENTS_BUILD_SLOT_HELD"], str(os.getpid()))
        self.assertEqual(build_host.build_environment({}, path)["CARGO_BUILD_JOBS"], str(os.cpu_count()))

    def test_a_current_that_is_not_a_pipe_is_not_joined(self):
        (self.directory / "fifo-1").write_text("")
        (self.directory / "fifo-1.users").touch()
        (self.directory / "current").symlink_to("fifo-1")
        self.assertIsNone(build_host.join_jobserver())


if __name__ == "__main__":
    unittest.main()
