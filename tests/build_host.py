#!/usr/bin/env python3
"""Exercise scripts/build-host with private share locks and stand-in tools.

Every build here runs in place (CAGENTS_BUILD_SCOPE=0): no test makes a
systemd scope or joins the host's jobserver. Jobserver tests join one on a
private directory, in this process.
"""

import array
import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import shutil
import signal
import stat
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

# Bounds only a hang: every wait it guards returns as soon as what it waits for happens.
HANG_GUARD = 60

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
                 "CAGENTS_TEST_RETAINED_ACTOR_HELPER", "CAGENTS_BUILD_PROBE_DIRECTORY", "RUSTC_WRAPPER")})
with (root / "calls.jsonl").open("a") as calls:
    calls.write(json.dumps(entry) + "\n")
# As Cargo would: says something, asks the compiler its version and compiles,
# each through the compiler wrapper.
if os.environ.get("FIXTURE_SAY"):
    print(f"out {os.environ['FIXTURE_SAY']}", flush=True)
    print(f"err {os.environ['FIXTURE_SAY']}", file=sys.stderr, flush=True)
wrapper = [os.environ["RUSTC_WRAPPER"]] if os.environ.get("RUSTC_WRAPPER") else []
if os.environ.get("FIXTURE_QUERY"):
    import subprocess
    answer = subprocess.run([*wrapper, sys.executable, "-c", "print('rustc 0.0.0')", "-vV"],
                            capture_output=True, text=True)
    print(f"asked {answer.returncode} {answer.stdout.strip()}", file=sys.stderr, flush=True)
if os.environ.get("FIXTURE_COMPILE") and entry["program"] == "cargo":
    import subprocess
    if subprocess.run([*wrapper, sys.executable, "-c", "pass", "--crate-name", "fixture"]).returncode:
        raise SystemExit(101)
if entry["program"] == "memory-capped":
    command = sys.argv[1:]
    while command[:1] in (["--name"], ["--slice"], ["--stop-leftovers"]):
        command = command[1 if command[0] == "--stop-leftovers" else 2:]
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

    def wait_for(self, predicate, label, timeout=HANG_GUARD):
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
                              cwd=self.root, capture_output=True, text=True, timeout=HANG_GUARD)

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
        stdout, stderr = process.communicate(timeout=HANG_GUARD)
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
        for options in ((), ("--wait-seconds", str(10 * HANG_GUARD))):
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
        for options in ((), ("--wait-seconds", str(10 * HANG_GUARD))):
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

    def test_a_build_another_build_host_started_stays_in_its_process_group(self):
        # Its own group would stop a command reading the terminal; the outer
        # build's group or scope already ends it.
        leads = [sys.executable, "-c", "import os; print(os.getpgrp() == os.getpid())"]
        for command, expected in ((leads, "True"),
                                  ([str(self.build_host), "build", "--shares", "0", "--", *leads], "False")):
            with self.subTest(expected=expected):
                result = self.run_host("build", "--", *command)
                self.assertEqual((result.returncode, result.stdout.strip()), (0, expected), result.stderr)

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
        # Two extra shares follow the host's own.
        self.assertIn(int(result.stdout), range(4, max(2, memory * 3 // 4 // 2**32) + 3))


class ExtraShares(BuildHostFixture):
    """Past the host's shares, a build takes an extra one only while memory
    allows, in the same line as any other."""

    slots = 2
    GiB = 2**30

    def setUp(self):
        super().setUp()
        self.proc = self.root / "proc"
        (self.proc / "pressure").mkdir(parents=True)
        self.env |= {"CAGENTS_BUILD_EXTRA_SHARES": "2", "CAGENTS_BUILD_PROC": str(self.proc)}
        self.memory(available=64 * self.GiB)
        # The host's own shares are another build's.
        for index in range(self.slots):
            self.hold(self.share(index))

    def memory(self, available=None, pressure=0.0):
        """The host's MemAvailable and memory pressure, as the kernel shows them."""
        if available is not None:
            self.write(self.proc / "meminfo", f"MemTotal:       {64 * 2**20} kB\n"
                       f"MemFree:          123456 kB\nMemAvailable:   {available // 1024} kB\n")
        self.write(self.proc / "pressure/memory",
                   f"some avg10={pressure:.2f} avg60=0.05 avg300=0.04 total=4877431889\n"
                   f"full avg10={pressure / 2:.2f} avg60=0.03 avg300=0.02 total=4427153170\n")

    def write(self, path, text):
        # Whole, as /proc shows it: a reader never sees half of it.
        path.with_name(path.name + ".new").write_text(text)
        path.with_name(path.name + ".new").replace(path)

    def assert_waits(self, name, label):
        time.sleep(0.6)
        self.assertFalse(self.started(name), label)

    def test_an_extra_share_is_taken_when_memory_holds_the_build_and_the_floor(self):
        self.memory(available=(2 * 4 + 8) * self.GiB)
        build = self.held_build("extra", 2)
        self.wait_for(lambda: self.started("extra"), "the build on extra shares")
        self.assertTrue(self.is_locked(self.share(2)) and self.is_locked(self.share(3)))
        status, _stdout, stderr = self.finish(build, "extra")
        self.assertEqual(status, 0, stderr)
        self.assertNotIn("waiting", stderr)
        self.assertFalse(self.is_locked(self.share(2)) or self.is_locked(self.share(3)))

    def test_a_build_waits_while_memory_available_is_short_and_starts_once_it_is_not(self):
        self.memory(available=(4 + 8) * self.GiB - 1)
        build = self.held_build("short", 1)
        self.wait_for(lambda: self.is_locked(f"{self.lock}.queue"), "the build to queue")
        self.assert_waits("short", "an extra share was taken without the memory for it")
        self.assertFalse(self.is_locked(self.share(2)))
        self.memory(available=(4 + 8) * self.GiB)
        self.wait_for(lambda: self.started("short"), "the build once memory allowed")
        status, _stdout, stderr = self.finish(build, "short")
        self.assertEqual(status, 0, stderr)
        self.assertIn("waiting for 1 of the 2 build memory shares", stderr)
        self.assertIn("an extra share is free, but not the memory for it", stderr)
        self.assertIn("admitted on 1 extra build memory share, as memory allows", stderr)

    def test_extra_shares_others_hold_count_against_memory_available(self):
        # Another build took an extra share and may not have used its memory yet.
        self.hold(self.share(2))
        self.memory(available=(4 + 8) * self.GiB)
        build = self.held_build("second", 1)
        self.assert_waits("second", "memory counted once for two builds")
        self.memory(available=(2 * 4 + 8) * self.GiB)
        self.wait_for(lambda: self.started("second"), "the build once memory held both")
        self.assertTrue(self.is_locked(self.share(3)))
        self.assertEqual(self.finish(build, "second")[0], 0)

    def test_a_build_waits_under_memory_pressure_and_starts_once_it_eases(self):
        self.memory(pressure=5.0)
        build = self.held_build("pressed", 1)
        self.assert_waits("pressed", "an extra share was taken under memory pressure")
        self.memory(pressure=1.0)
        self.wait_for(lambda: self.started("pressed"), "the build once pressure eased")
        self.assertEqual(self.finish(build, "pressed")[0], 0)

    def test_without_memory_readings_no_extra_share_is_taken(self):
        for missing in ("meminfo", "pressure/memory"):
            with self.subTest(missing=missing):
                self.memory(available=64 * self.GiB)
                (self.proc / missing).unlink()
                build = self.held_build(f"blind-{missing[:3]}", 1, "--wait-seconds", "1")
                status, _stdout, stderr = self.finish(build)
                self.assertEqual(status, 124, stderr)
                self.assertFalse(self.started(f"blind-{missing[:3]}"))

    def test_a_build_short_of_memory_keeps_its_place_at_the_head_of_the_line(self):
        self.memory(available=(2 * 4 + 8) * self.GiB - 1)
        head = self.held_build("head", 2)
        self.wait_for(lambda: self.is_locked(f"{self.lock}.queue"), "the head")
        behind = self.held_build("behind", 1)
        self.wait_for(lambda: "lock" in Path(f"/proc/{behind.pid}/wchan").read_text(),
                      "the smaller build to queue behind it")
        # Memory enough for the smaller build alone goes to neither.
        self.memory(available=(4 + 8) * self.GiB)
        self.assert_waits("behind", "a smaller build overtook the head on extra shares")
        self.assertFalse(self.started("head"))
        self.memory(available=(2 * 4 + 8) * self.GiB)
        self.wait_for(lambda: self.started("head"), "the head once memory allowed")
        # The head holds both extra shares: the one behind waits for a share.
        self.assert_waits("behind", "more shares were taken than the host and its extras have")
        self.assertEqual(self.finish(head, "head")[0], 0)
        self.wait_for(lambda: self.started("behind"), "the smaller build after the head")
        self.assertEqual(self.finish(behind, "behind")[0], 0)
        self.assertEqual([call["arguments"] for call in self.calls()], [["head"], ["behind"]])

    def test_no_more_than_the_extra_shares_are_taken_however_much_memory_is_free(self):
        first, second = self.held_build("first", 1), self.held_build("second", 1)
        self.wait_for(lambda: self.started("first") and self.started("second"), "two extra shares")
        third = self.held_build("third", 1)
        self.assert_waits("third", "a third extra share was taken")
        self.assertEqual(self.finish(first, "first")[0], 0)
        self.wait_for(lambda: self.started("third"), "the third build once an extra share freed")
        self.assertEqual([self.finish(build, name)[0] for build, name in
                          ((second, "second"), (third, "third"))], [0, 0])

    def test_extra_shares_are_taken_only_to_complete_a_build(self):
        # One extra share is free and the other is another build's: taking
        # the free one would leave this build holding it on memory it may
        # not have when a second share frees.
        other = self.hold(self.share(3))
        build = self.held_build("pair", 2)
        self.wait_for(lambda: self.is_locked(f"{self.lock}.queue"), "the build to queue")
        self.assert_waits("pair", "a build started on one share of two")
        self.assertFalse(self.is_locked(self.share(2)), "a partial extra share was held")
        other.close()
        self.wait_for(lambda: self.started("pair"), "the build once both extra shares freed")
        self.assertEqual(self.finish(build, "pair")[0], 0)

    def test_a_build_that_need_not_wait_takes_an_extra_share_only_if_memory_allows(self):
        env = {"CAGENTS_BUILD_LOCK": str(self.lock), "CAGENTS_BUILD_SLOTS": str(self.slots),
               "CAGENTS_BUILD_EXTRA_SHARES": "2", "CAGENTS_BUILD_SCOPE": "0",
               "CAGENTS_BUILD_PROC": str(self.proc)}
        said = io.StringIO()
        with unittest.mock.patch.dict(os.environ, env), contextlib.redirect_stderr(said):
            self.memory(available=(4 + 8) * self.GiB - 1)
            with self.assertRaises(build_host.Busy):
                with build_host.shares(1, None, queue_up=False):
                    self.fail("admitted without the memory for an extra share")
            self.assertFalse(self.is_locked(self.share(2)))
            self.memory(available=(4 + 8) * self.GiB)
            with build_host.shares(1, None, queue_up=False):
                self.assertTrue(self.is_locked(self.share(2)))
            self.assertFalse(self.is_locked(self.share(2)))
        self.assertIn("admitted on 1 extra build memory share", said.getvalue())

    def test_the_build_slices_room_under_its_cap_bounds_extra_shares(self):
        stat = self.root / "memory.stat"
        budget = 32 * self.GiB
        with unittest.mock.patch.object(build_host, "budget", return_value=budget):
            for anonymous, allowed in ((budget - 8 * self.GiB, True), (budget - 8 * self.GiB + 1, False)):
                with self.subTest(anonymous=anonymous):
                    self.write(stat, f"anon {anonymous}\nfile 123456789\nfile_dirty 0\n")
                    with unittest.mock.patch.dict(os.environ, {"CAGENTS_BUILD_PROC": str(self.proc)}):
                        self.assertEqual(build_host.memory_allows(8 * self.GiB, str(stat)), allowed)
            with unittest.mock.patch.dict(os.environ, {"CAGENTS_BUILD_PROC": str(self.proc)}):
                # A slice no build has started holds nothing; one where no
                # user manager is, or an unreadable one, bars them.
                (self.root / "manager/background.slice").mkdir(parents=True)
                self.assertTrue(build_host.memory_allows(
                    8 * self.GiB, str(self.root / "manager/background.slice/builds.slice/memory.stat")))
                self.assertFalse(build_host.memory_allows(
                    8 * self.GiB, str(self.root / "elsewhere/background.slice/builds.slice/memory.stat")))
                self.write(stat, "file 1\n")
                self.assertFalse(build_host.memory_allows(8 * self.GiB, str(stat)))

    def test_the_host_has_two_extra_shares_unless_its_share_count_is_configured(self):
        for env, extra in (({}, 2), ({"CAGENTS_BUILD_SLOTS": "1"}, 0),
                           ({"CAGENTS_BUILD_SLOTS": "1", "CAGENTS_BUILD_EXTRA_SHARES": "3"}, 3)):
            with self.subTest(env=env), unittest.mock.patch.dict(os.environ, env, clear=True):
                self.assertEqual(build_host.extra_share_count(), extra)

    def test_a_configured_share_count_has_no_extra_shares_unless_they_are_set(self):
        env = {key: value for key, value in self.env.items() if key != "CAGENTS_BUILD_EXTRA_SHARES"}
        result = self.run_host("build", "--wait-seconds", "1", "--", str(self.cargo), "mutex", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual(self.calls(), [])
        result = self.run_host("build", "--", "true", env=self.env | {"CAGENTS_BUILD_EXTRA_SHARES": "x"})
        self.assertEqual(result.returncode, 2)
        self.assertIn("CAGENTS_BUILD_EXTRA_SHARES must be a whole number", result.stderr)


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
                    ("memory-capped", ["--slice", "app-cagents-test.slice", "--stop-leftovers", str(self.cargo),
                                       subcommand, "--lib", "--", "filter", "--no-run"], []),
                    ("cargo", [subcommand, "--lib", "--", "filter", "--no-run"], []),
                ])

    def test_cargos_short_names_split_as_the_long_ones(self):
        self.assertEqual(self.phases("t", "--lib"), [
            ("cargo", ["test", "--no-run", "--lib"], [0, 1]),
            ("memory-capped", ["--slice", "app-cagents-test.slice", "--stop-leftovers", str(self.cargo), "test", "--lib"], []),
            ("cargo", ["test", "--lib"], []),
        ])
        (self.root / "calls.jsonl").unlink()
        self.assertEqual(self.phases("r", "--bin", "app"), [
            ("cargo", ["build", "--bin", "app"], [0, 1]),
            ("cargo", ["run", "--bin", "app"], []),
        ])

    def test_a_program_builds_then_runs_without_shares_or_the_limit(self):
        self.assertEqual(self.phases("run", "--bin", "app", "--", "argument with spaces"), [
            ("cargo", ["build", "--bin", "app"], [0, 1]),
            ("cargo", ["run", "--bin", "app", "--", "argument with spaces"], []),
        ])

    def test_a_toolchain_argument_stays_before_the_subcommand(self):
        self.assertEqual([arguments for _, arguments, _ in self.phases("+nightly", "test", "--lib")],
                         [["+nightly", "test", "--no-run", "--lib"],
                          ["--slice", "app-cagents-test.slice", "--stop-leftovers", str(self.cargo), "+nightly", "test", "--lib"],
                          ["+nightly", "test", "--lib"]])

    def test_doctests_build_and_run_in_one_within_the_shares_under_the_limit(self):
        # Cargo cannot build doctests without running them.
        for arguments in (("test", "--doc"), ("+nightly", "test", "--doc", "--", "filter")):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                self.assertEqual(self.phases(*arguments), [
                    ("memory-capped", ["--slice", "app-cagents-test.slice", "--stop-leftovers", str(self.cargo), *arguments], [0, 1]),
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
                          ["--slice", "app-cagents-test.slice", "--stop-leftovers", str(self.cargo), "test", "--config", runner, "--lib"],
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


class Probing(BuildHostFixture):
    """A build whose shares are taken first asks Cargo whether it compiles
    anything, and waits for them only if it does."""

    slots = 2

    def setUp(self):
        super().setUp()
        shutil.copy2(ROOT / "scripts/cargo-gate-rustc-guard", self.root / "scripts")
        # Every share is taken by other builds.
        self.others = [self.hold(self.share(index)) for index in range(self.slots)]

    def queued(self, build):
        """Waits until `build` is in the queue for shares and has stayed
        there, past the moment a probe would have run."""
        queue = f"{self.lock}.queue"
        self.wait_for(lambda: self.is_locked(queue), "the build to queue")
        time.sleep(0.5)
        self.assertTrue(self.is_locked(queue) and build.poll() is None, "the build left the queue")

    def free_shares(self):
        for holder in self.others:
            holder.close()

    def test_a_build_with_nothing_to_compile_takes_no_shares(self):
        for arguments in (("build", "--lib"), ("check",), ("c",), ("clippy",), ("test", "--no-run"),
                          ("+nightly", "build")):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                result = self.run_host("cargo", "--", str(self.cargo), *arguments,
                                       env=self.env | {"FIXTURE_SAY": "fresh"})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn("waiting", result.stderr)
                [call] = self.calls()
                self.assertEqual(call["arguments"], list(arguments))
                self.assertEqual(call["environment"]["CARGO_BUILD_JOBS"], "1")
                self.assertEqual(Path(call["environment"]["RUSTC_WRAPPER"]).name, "cargo-gate-rustc-guard")
                self.assertIsNotNone(call["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"])
                # Its output, kept while it ran, is the build's.
                self.assertEqual(result.stdout, "out fresh\n")
                self.assertEqual(result.stderr, "err fresh\n")
                self.assertFalse(Path(call["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"]).exists())

    def test_tests_and_programs_run_after_a_probe_found_them_built(self):
        result = self.run_host("cargo", "--", str(self.cargo), "test", "--lib")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([(call["program"], call["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"] is not None)
                          for call in self.calls()],
                         [("cargo", True), ("memory-capped", False), ("cargo", False)])

    def test_a_build_that_compiles_waits_for_its_shares_and_builds_again(self):
        build = self.start("cargo", "--", str(self.cargo), "build",
                           env=self.env | {"FIXTURE_COMPILE": "1", "FIXTURE_SAY": "built"})
        self.wait_for(lambda: len(self.calls()) == 1, "the probe")
        self.queued(build)
        [probe] = self.calls()
        self.assertIsNotNone(probe["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"])
        self.free_shares()
        status, stdout, stderr = self.finish(build)
        self.assertEqual(status, 0, stderr)
        [_, admitted] = self.calls()
        self.assertEqual(admitted["held"], [0])
        self.assertIsNone(admitted["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"])
        self.assertIsNone(admitted["environment"]["RUSTC_WRAPPER"])
        # Only the admitted build's output shows.
        self.assertEqual(stdout, "out built\n")
        self.assertIn("waiting for 1 of the 2 build memory shares", stderr)
        self.assertEqual(stderr.count("err built"), 1)

    def test_queries_reach_the_compiler_through_the_callers_wrapper(self):
        caller = self.root / "bin/caller-wrapper"
        caller.write_text("#!/bin/sh\nprintf 'caller '\nexec \"$@\"\n")
        caller.chmod(0o755)
        for env, expected in (({}, "asked 0 rustc 0.0.0"),
                              ({"RUSTC_WRAPPER": str(caller)}, "asked 0 caller rustc 0.0.0")):
            with self.subTest(env=env):
                result = self.run_host("cargo", "--", str(self.cargo), "check",
                                       env=self.env | env | {"FIXTURE_QUERY": "1"})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, expected + "\n")

    def test_a_failed_probe_is_the_builds_failure(self):
        result = self.run_host("cargo", "--", str(self.cargo), "test", "--lib",
                               env=self.env | {"FIXTURE_EXIT": "101", "FIXTURE_SAY": "broken"})
        self.assertEqual(result.returncode, 101)
        self.assertEqual(result.stderr, "err broken\n")
        self.assertEqual(len(self.calls()), 1)

    def test_what_runs_or_documents_is_not_probed(self):
        for arguments in (("test", "--doc"), ("doc",), ("test", "--doc", "--no-run"), ("metadata",)):
            with self.subTest(arguments):
                (self.root / "calls.jsonl").unlink(missing_ok=True)
                build = self.start("cargo", "--", str(self.cargo), *arguments)
                self.queued(build)
                self.assertEqual(self.calls(), [])
                build.kill()
                build.wait()

    def test_with_every_probe_slot_taken_a_build_waits_for_its_shares(self):
        for index in range(4):
            self.hold(f"{self.lock}.probe.{index}")
        build = self.start("cargo", "--", str(self.cargo), "check")
        self.queued(build)
        self.assertEqual(self.calls(), [])
        self.free_shares()
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, 0, stderr)
        self.assertEqual(self.calls()[0]["held"], [0])

    def test_probing_can_be_turned_off(self):
        build = self.start("cargo", "--", str(self.cargo), "check", env=self.env | {"CAGENTS_BUILD_PROBE": "0"})
        self.queued(build)
        self.assertEqual(self.calls(), [])
        self.free_shares()
        self.assertEqual(self.finish(build)[0], 0)

    def test_free_shares_are_taken_without_a_probe(self):
        self.free_shares()
        result = self.run_host("cargo", "--", str(self.cargo), "check")
        self.assertEqual(result.returncode, 0, result.stderr)
        [call] = self.calls()
        self.assertEqual(call["held"], [0])
        self.assertIsNone(call["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"])

    def test_a_probe_outgrowing_its_slot_ends_with_what_it_started(self):
        script = ("import os, subprocess, sys, time\n"
                  "child = subprocess.Popen([sys.executable, '-c', "
                  "'x = b\"x\" * (64 << 20); import time; time.sleep(60)'])\n"
                  "open(os.environ['FIXTURE_GRANDCHILD'], 'w').write(str(child.pid))\n"
                  "time.sleep(60)\n")
        recorded = self.root / "grandchild"
        # In place, as every build here runs, never in a scope.
        with tempfile.TemporaryDirectory(dir=self.root) as directory, \
                unittest.mock.patch.object(build_host, "PROBE_MEMORY", 32 << 20), \
                unittest.mock.patch.object(build_host, "prepare_slice", lambda: False):
            probe = build_host.Probe(directory, None)
            started = time.monotonic()
            with self.assertRaises(build_host.Stale):
                probe.run([sys.executable, "-c", script], dict(self.env) | {"FIXTURE_GRANDCHILD": str(recorded)})
            self.assertLess(time.monotonic() - started, HANG_GUARD)
        # What it started ended with it.
        grandchild = int(recorded.read_text())
        def ended():
            with contextlib.suppress(FileNotFoundError, ProcessLookupError):
                return Path(f"/proc/{grandchild}/stat").read_text().rsplit(") ", 1)[1][0] == "Z"
            return True
        self.wait_for(ended, "the probe's child to end")

    def test_a_cargo_the_probe_cannot_start_is_reported_by_the_build(self):
        build = self.start("cargo", "--", str(self.root / "missing-cargo"), "check")
        self.queued(build)
        self.free_shares()
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, 127, stderr)
        self.assertNotIn("Traceback", stderr)

    def test_a_probe_ends_at_the_admission_deadline(self):
        build = self.start("cargo", "--wait-seconds", "1", "--", str(self.cargo), "check",
                           env=self.env | {"FIXTURE_HOLD": "never"})
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, 124, stderr)
        self.assertIn("build admission timed out", stderr)
        [probe] = self.calls()
        self.assertIsNotNone(probe["environment"]["CAGENTS_BUILD_PROBE_DIRECTORY"])

    def test_a_signal_ends_a_probe_with_what_it_started(self):
        build = self.start("cargo", "--", str(self.cargo), "check", env=self.env | {"FIXTURE_HOLD": "never"})
        self.wait_for(lambda: self.started("never"), "the probe")
        [probe] = self.calls()
        build.send_signal(signal.SIGTERM)
        status, _stdout, _stderr = self.finish(build)
        self.assertEqual(status, -signal.SIGTERM)
        self.wait_for(lambda: not Path(f"/proc/{probe['pid']}").exists(), "the probe to end")
        # Its slot is free again.
        self.assertFalse(self.is_locked(f"{self.lock}.probe.0"))

    def test_the_guard_ends_a_probe_at_its_first_compile_only(self):
        guard = self.root / "scripts/cargo-gate-rustc-guard"
        with tempfile.TemporaryDirectory(dir=self.root) as directory:
            env = self.env | {"CAGENTS_BUILD_PROBE_DIRECTORY": directory}
            # Each in a process group of its own, as a probe is, which a
            # compile ends.
            asked = subprocess.run([str(guard), "/bin/echo", "--print=cfg"], env=env,
                                   capture_output=True, text=True, process_group=0)
            self.assertEqual((asked.returncode, asked.stdout), (0, "--print=cfg\n"))
            self.assertFalse((Path(directory) / "compiles").exists())
            compiled = subprocess.run(["/bin/sh", "-c", '"$@"; echo "survived $?"', "compiler",
                                       str(guard), "/bin/echo", "src/lib.rs"], env=env,
                                      capture_output=True, text=True, process_group=0)
            self.assertEqual((compiled.returncode, compiled.stdout), (-signal.SIGKILL, ""))
            self.assertTrue((Path(directory) / "compiles").exists())


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
        build = self.start("build", "--", "/bin/sh", "-c", 'sleep infinity & echo $! >"$1"; wait', "script",
                           str(self.root / "background"))
        self.wait_for((self.root / "background").exists, "the background job")
        self.wait_for(lambda: (self.root / "background").read_text().strip(), "its process ID")
        background = os.pidfd_open(int((self.root / "background").read_text()))
        self.addCleanup(os.close, background)
        build.send_signal(signal.SIGTERM)
        status, _stdout, stderr = self.finish(build)
        self.assertEqual(status, -signal.SIGTERM, stderr)
        self.assertTrue(select.select([background], [], [], HANG_GUARD)[0],
                        "the background job outlived its build")

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
        # Outlasting the hang guard, so that returning within it shows the
        # command was killed rather than left to finish.
        command = ["/bin/sh", "-c", f'trap "" INT; touch "$1"; exec sleep {2 * HANG_GUARD}', "ignoring",
                   str(ready)]
        readings = []

        def fast():
            readings.append(True)
            return time.monotonic() * 50

        clock = types.SimpleNamespace(monotonic=fast, monotonic_ns=time.monotonic_ns, sleep=time.sleep)
        early = []
        previous = signal.signal(signal.SIGTERM, lambda *_: early.append(True))
        self.addCleanup(signal.signal, signal.SIGTERM, previous)

        def interrupt():
            deadline = time.monotonic() + HANG_GUARD
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
        self.assertLess(time.monotonic() - started, HANG_GUARD)
        # The grace was measured on the module's clock: once when the
        # interrupt came, and again when it ran out.
        self.assertGreaterEqual(len(readings), 2)
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
    """Builds' places in a jobserver on a private directory, each stocked by
    a thread of this process, with this test taking and giving back tokens
    as the builds' compilers would."""

    TOKENS = 4

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-jobserver-")
        self.addCleanup(self.scope.close)
        self.directory = Path(self.scope.name)
        for name, value in (("TOKEN_TICK", 0.002), ("SWEEP_SECONDS", 0.02)):
            patcher = unittest.mock.patch.object(build_host, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def join(self):
        jobserver = build_host.Jobserver(str(self.directory), self.TOKENS)
        self.addCleanup(lambda: jobserver.pool is not None and jobserver.close())
        reader, writer = map(int, jobserver.auth.split(","))
        # This test is the only reader, so the build's end need not block.
        os.set_blocking(reader, False)
        jobserver.client = types.SimpleNamespace(reader=reader, writer=writer, held=0)
        return jobserver

    def take(self, jobserver, count):
        """Takes `count` tokens as the build's compilers would, waiting for each."""
        deadline = time.monotonic() + HANG_GUARD
        while count:
            try:
                got = len(os.read(jobserver.client.reader, count))
            except BlockingIOError:
                self.assertLess(time.monotonic(), deadline, f"{count} tokens never came")
                time.sleep(0.001)
                continue
            jobserver.client.held += got
            count -= got

    def give_back(self, jobserver, count):
        os.write(jobserver.client.writer, b"+" * count)
        jobserver.client.held -= count

    def settle(self, *jobservers):
        """Stops the stocking threads, so counts hold still."""
        for jobserver in jobservers:
            jobserver.stopping.set()
            jobserver.stocker.join()

    def wait_for(self, predicate, label):
        deadline = time.monotonic() + HANG_GUARD
        while not predicate():
            self.assertLess(time.monotonic(), deadline, f"timed out waiting for {label}")
            time.sleep(0.002)

    def counts(self, *jobservers):
        """The tokens in the pool, and with each build, in its pipes or held."""
        return (waiting(jobservers[0].pool),
                [waiting(j.give) + waiting(j.back) + j.client.held for j in jobservers])

    def test_the_first_build_fills_the_pool_and_gets_pipes_of_its_own(self):
        jobserver = self.join()
        self.wait_for(lambda: jobserver.taken == 1 and waiting(jobserver.give) == 1, "a token in its pipe")
        self.settle(jobserver)
        self.assertEqual(self.counts(jobserver), (self.TOKENS - 1, [1]))
        environment = build_host.build_environment({"MFLAGS": "-s"}, jobserver.auth)
        self.assertEqual(environment["MAKEFLAGS"], f"-j --jobserver-auth={jobserver.auth}")
        self.assertNotIn("MFLAGS", environment)
        reader, writer = map(int, jobserver.auth.split(","))
        self.assertTrue(os.get_inheritable(reader) and os.get_inheritable(writer))
        # Clear of what a script redirects, as `exec 9>lock` would.
        self.assertGreaterEqual(min(reader, writer), build_host.HIGH_DESCRIPTOR)
        # Nothing of the build's own is left to clean up.
        self.assertEqual(sorted(path.name for path in self.directory.iterdir()), ["builds", "join", "pool", "users"])

    def test_a_build_gets_more_as_it_takes_them_and_what_it_gives_back_goes_to_the_pool(self):
        jobserver = self.join()
        self.take(jobserver, self.TOKENS)
        # The stocker counts a token after writing it, so the build can hold
        # the last ones before it has counted them.
        self.wait_for(lambda: jobserver.taken == self.TOKENS, "every token counted as taken")
        self.give_back(jobserver, 3)
        self.wait_for(lambda: jobserver.taken < self.TOKENS, "the returned tokens to go back")
        self.settle(jobserver)
        pool, [build] = self.counts(jobserver)
        self.assertEqual((pool + build, jobserver.taken), (self.TOKENS, build))

    def test_a_build_that_ends_holding_tokens_loses_none(self):
        other = self.join()
        jobserver = self.join()
        # Its compilers are killed with two, while the other build keeps the pool.
        self.take(jobserver, 2)
        jobserver.close()
        self.settle(other)
        self.assertEqual(sum(self.counts(other)[1]) + waiting(other.pool), self.TOKENS)

    def at_build_pace(self):
        """Ticks as long as the real ones, with jobs ending more often than a
        compiler's do."""
        self.TOKENS = 8
        for name, value in (("TOKEN_TICK", 0.02), ("SWEEP_SECONDS", 0.1)):
            patcher = unittest.mock.patch.object(build_host, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def compiling(self, jobservers, seconds=2):
        """How many tokens each build held on average over the last two
        thirds of `seconds`, with its compilers ending a job now and then and
        starting as many as they can."""
        stop = threading.Event()
        samples = [[] for _ in jobservers]

        def compile(jobserver, held):
            while not stop.is_set():
                if jobserver.client.held:
                    self.give_back(jobserver, 1)
                with contextlib.suppress(BlockingIOError):
                    jobserver.client.held += len(os.read(jobserver.client.reader, self.TOKENS))
                held.append(jobserver.client.held)
                time.sleep(0.01)

        workers = [threading.Thread(target=compile, args=pair) for pair in zip(jobservers, samples)]
        for worker in workers:
            worker.start()
        time.sleep(seconds)
        stop.set()
        for worker in workers:
            worker.join()
        return [sum(held[len(held) // 3:]) / max(1, len(held) - len(held) // 3) for held in samples]

    def test_builds_that_want_more_share_the_tokens_equally(self):
        self.at_build_pace()
        first = self.join()
        self.take(first, self.TOKENS)
        second = self.join()
        held = self.compiling([first, second])
        # Equal shares, judged against each other: how many both hold at once
        # depends on how fast this host runs their compilers.
        self.assertGreaterEqual(min(held), 1, held)
        self.assertGreaterEqual(min(held), 0.75 * max(held), held)

    def test_a_build_alone_in_wanting_more_gets_what_the_others_leave(self):
        self.at_build_pace()
        idle = self.join()
        busy = self.join()
        # The idle build keeps one in its pipe.
        [held] = self.compiling([busy])
        self.assertGreaterEqual(held, self.TOKENS - 2, held)

    def test_what_a_killed_build_host_took_is_given_back_by_another(self):
        other = self.join()
        killed = self.join()
        self.take(killed, 3)
        self.wait_for(lambda: os.pread(killed.record, 20, 0).strip() == str(killed.taken).encode(),
                      "its record")
        # SIGKILL: its thread and descriptors go, and nothing more.
        killed.stopping.set()
        killed.stocker.join()
        for descriptor in (*killed.own, killed.record, killed.pool, killed.users):
            os.close(descriptor)
        killed.pool = None
        self.wait_for(lambda: not os.listdir(self.directory / "builds") or
                      os.listdir(self.directory / "builds") == [Path(other.record_path).name],
                      "its record to be settled")
        self.settle(other)
        self.assertEqual(sum(self.counts(other)[1]) + waiting(other.pool), self.TOKENS)

    def test_a_build_finding_the_pool_unused_fills_it_and_forgets_old_records(self):
        first = self.join()
        self.take(first, 2)
        first.close()
        (self.directory / "builds" / "1-2").write_text("3")
        second = self.join()
        self.assertEqual(os.listdir(self.directory / "builds"), [Path(second.record_path).name])
        self.settle(second)
        self.assertEqual(waiting(second.pool) + waiting(second.give), self.TOKENS)

    def test_a_pool_that_is_not_a_pipe_is_not_joined(self):
        (self.directory / "pool").write_text("")
        with self.assertRaises(OSError):
            build_host.Jobserver(str(self.directory), self.TOKENS)
        self.assertEqual(sorted(path.name for path in self.directory.iterdir()),
                         ["builds", "join", "pool", "users"])

    def test_a_build_rejoins_when_something_between_closed_its_jobservers_descriptors(self):
        reader, writer = os.pipe()
        self.addCleanup(os.close, writer)
        named = {"MAKEFLAGS": f"-j --jobserver-auth={reader},{writer}"}
        for environment, lost in (({}, False), ({"MAKEFLAGS": "-s"}, False), (named, False)):
            with unittest.mock.patch.dict(os.environ, environment, clear=True):
                self.assertEqual(build_host.lost_jobserver(), lost, environment)
        os.close(reader)
        with unittest.mock.patch.dict(os.environ, named, clear=True):
            self.assertTrue(build_host.lost_jobserver())
        with unittest.mock.patch.dict(os.environ, named | {"CAGENTS_BUILD_SCOPE": "0"}, clear=True):
            self.assertFalse(build_host.lost_jobserver())
        # A descriptor reused for something other than a pipe is lost as well.
        with open(os.devnull) as file, unittest.mock.patch.dict(
                os.environ, {"MAKEFLAGS": f"-j --jobserver-auth={file.fileno()},{writer}"}, clear=True):
            self.assertTrue(build_host.lost_jobserver())

    def test_cargo_jobs_follow_the_jobserver_unless_the_caller_set_them(self):
        self.assertEqual(build_host.build_environment({}, "3,4")["CARGO_BUILD_JOBS"], str(os.cpu_count()))
        self.assertEqual(build_host.build_environment({"CARGO_BUILD_JOBS": "5"}, "3,4")["CARGO_BUILD_JOBS"],
                         "5")


if __name__ == "__main__":
    unittest.main()
