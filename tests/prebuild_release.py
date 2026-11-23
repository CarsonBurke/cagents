#!/usr/bin/env python3
"""Exercise scripts/prebuild-release against a private repository whose
commits carry stand-in build scripts, a private store and sentinel state, and
stand-in memory, pressure and lock readings. Nothing builds, and the host's
user manager is never asked."""

import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
_loader = importlib.machinery.SourceFileLoader("prebuild_release", str(ROOT / "scripts/prebuild-release"))
prebuild = importlib.util.module_from_spec(importlib.util.spec_from_loader("prebuild_release", _loader))
_loader.exec_module(prebuild)

# Bounds only a hang.
HANG_GUARD = 60

GATE = """#!/bin/sh
# As the gate's exec: runs the command within the build's shares, once
# admitted after PREBUILD_GATE_WAIT seconds when that is set.
[ "$1" = exec ] || exit 99
shift
if [ -n "${PREBUILD_GATE_WAIT:-}" ]; then
    echo "cargo-gate: waiting for 2 of the 6 build memory shares on fixture; the others are in use"
    sleep "$PREBUILD_GATE_WAIT"
fi
exec "$@"
"""

# Records how it was run, then makes a bundle as build-release-linux would,
# after waiting for its release file when PREBUILD_HOLD is set.
BUILDER = r"""#!/bin/bash
set -euo pipefail
while (($#)); do
    case $1 in --ref) ref=$2; shift 2 ;; --output) out=$2; shift 2 ;; --notes) notes=$2; shift 2 ;; *) shift ;; esac
done
printf '%s\n' "$(git rev-parse HEAD) $ref ${CAGENTS_BUILD_IDLE:-normal} ${CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS:-unbounded} $CAGENTS_RELEASE_CACHE $CAGENTS_RELEASE_SMOKE_CACHE" \
    >> "$PREBUILD_FIXTURE/builds"
if [[ -n ${PREBUILD_HOLD:-} ]]; then
    trap 'touch "$PREBUILD_FIXTURE/stopped-$ref"; exit 143' TERM
    touch "$PREBUILD_FIXTURE/started-$ref"
    while [[ ! -e $PREBUILD_FIXTURE/release-$ref ]]; do sleep 0.02; done
fi
if [[ -e $PREBUILD_FIXTURE/admission ]]; then
    echo 'cargo-gate: build admission timed out'
    exit 124
fi
if [[ -e $PREBUILD_FIXTURE/smoke-hangs ]]; then
    echo 'smoke check timed out'
    exit 124
fi
if [[ -e $PREBUILD_FIXTURE/fail ]]; then
    rm -f "$PREBUILD_FIXTURE/fail-once"
    echo 'the build broke'
    exit 3
fi
[[ -s $notes ]]
mkdir -p "$out"
printf 'archive\n' > "$out/cagents-0.2.4-x86_64-unknown-linux-gnu.tar.gz"
if [[ -e $PREBUILD_FIXTURE/other-commit ]]; then ref=0000000000000000000000000000000000000000; fi
printf '{"version": "0.2.4", "source_commit": "%s", "protocol": 9}\n' "$ref" > "$out/release-metadata.json"
"""


class Prebuild(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="cagents-prebuild-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.repository = self.root / "repository"
        (self.repository / "scripts").mkdir(parents=True)
        (self.repository / ".gitignore").write_text(".worktrees/\n")
        (self.repository / "scripts/cargo-gate").write_text(GATE)
        (self.repository / "scripts/build-release-linux").write_text(BUILDER)
        for script in ("cargo-gate", "build-release-linux"):
            (self.repository / "scripts" / script).chmod(0o755)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "user.name", "fixture")
        self.first = self.commit("first")
        self.store = prebuild.Store(self.root / "store")
        self.sentinel = self.root / "sentinel"
        (self.sentinel / "verdicts").mkdir(parents=True)
        self.cache = self.root / "cache"
        self.proc = self.root / "proc"
        (self.proc / "pressure").mkdir(parents=True)
        self.memory(32)
        (self.proc / "pressure/memory").write_text(
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
        # The host's own lock table: a waiter below takes a real lock.
        (self.proc / "locks").symlink_to("/proc/locks")
        self.holders = []
        self.addCleanup(self.release_waiters)
        self.lock = self.root / "cargo.lock"
        Path(f"{self.lock}.queue").touch()
        bin_directory = self.root / "bin"
        bin_directory.mkdir()
        (bin_directory / "systemctl").write_text(
            f"#!/bin/sh\necho \"$*\" >> {self.root / 'systemctl'}\n")
        (bin_directory / "systemctl").chmod(0o755)
        environment = patch.dict(os.environ, {
            "PREBUILD_FIXTURE": str(self.root), "CAGENTS_PREBUILD_PROC": str(self.proc),
            "CAGENTS_BUILD_LOCK": str(self.lock), "PATH": f"{bin_directory}:{os.environ['PATH']}"})
        environment.start()
        self.addCleanup(environment.stop)
        for name, value in (("WATCH_SECONDS", 0.05), ("GRACE_SECONDS", 10), ("BACKOFF_SECONDS", 600),
                            ("SMOKE_CACHE", self.root / "smoke-cache")):
            patcher = patch.object(prebuild, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.pauses = []

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.repository), *args], check=True, text=True,
                              capture_output=True).stdout.strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def memory(self, gib):
        (self.proc / "meminfo").write_text(f"MemTotal: 48000000 kB\nMemAvailable: {int(gib * 2**20)} kB\n")

    def compiled(self, sha):
        (self.sentinel / "state.json").write_text(json.dumps({"compiled": sha, "checked": sha}))

    def verdict(self, sha, passed):
        (self.sentinel / "verdicts" / f"{sha}.json").write_text(json.dumps({"sha": sha, "passed": passed}))

    def waiter(self):
        """A build waiting for shares, holding the gate's queue lock in a
        process of its own, as the host's lock table then shows."""
        holder = subprocess.Popen(
            [sys.executable, "-c", "import fcntl, sys; f = open(sys.argv[1], 'a'); "
             "fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); sys.stdin.read()", f"{self.lock}.queue"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.holders.append(holder)
        self.assertEqual(holder.stdout.readline(), "held\n")

    def release_waiters(self):
        for holder in self.holders:
            holder.stdin.close()
            holder.wait(HANG_GUARD)
        self.holders = []

    def builds(self):
        path = self.root / "builds"
        return [line.split() for line in path.read_text().splitlines()] if path.exists() else []

    def pause(self, seconds):
        """Ends the run where it would wait: the test then looks at what it did."""
        self.pauses.append(seconds)
        raise Waited

    def run_once(self, keep=3):
        with patch.object(prebuild, "pause", self.pause):
            try:
                return prebuild.run(self.repository, self.store, self.sentinel, self.cache, keep)
            except Waited:
                return "waited"

    def start(self, keep=3):
        """Runs the service in a thread, for what changes while it builds."""
        outcome = []
        thread = threading.Thread(target=lambda: outcome.append(self.run_once(keep)), daemon=True)
        thread.start()
        return thread, outcome

    def wait_for(self, predicate, label):
        deadline = time.monotonic() + HANG_GUARD
        while not predicate():
            if time.monotonic() > deadline:
                self.fail(f"timed out waiting for {label}")
            time.sleep(0.01)

    def test_the_compiled_commit_is_bundled_in_the_background_by_its_own_scripts(self):
        self.compiled(self.first)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds(), [[self.first, self.first, "1", "10", str(self.cache),
                                          str(self.root / "smoke-cache")]])
        bundle = self.store.bundle(self.first)
        bundled = json.loads((bundle / "bundle.json").read_text())
        self.assertEqual((bundled["sha"], bundled["version"], bundled["protocol"], bundled["priority"]),
                         (self.first, "0.2.4", 9, "background"))
        self.assertTrue((bundle / "dist/cagents-0.2.4-x86_64-unknown-linux-gnu.tar.gz").is_file())
        self.assertTrue((bundle / "build.log").is_file())
        self.assertEqual(list(self.store.bundles.glob(".*")), [])
        self.assertEqual(self.store.root.stat().st_mode & 0o777, 0o700)
        # Bundled once: the next run has nothing to do.
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(len(self.builds()), 1)

    def test_a_commit_whose_tests_failed_is_not_bundled_but_one_still_testing_is(self):
        self.compiled(self.first)
        self.verdict(self.first, False)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds(), [])
        self.verdict(self.first, True)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(len(self.builds()), 1)

    def test_a_commit_of_a_released_version_waits_for_mains_bump(self):
        prebuild.UNRELEASED.clear()
        self.addCleanup(prebuild.UNRELEASED.clear)
        origin = self.root / "origin.git"
        subprocess.run(["git", "init", "-q", "--bare", str(origin)], check=True)
        self.git("remote", "add", "origin", str(origin))
        (self.repository / "Cargo.toml").write_text(
            '[package]\nname = "cagents"\nversion = "0.2.3"\n\n[dependencies]\nx = { version = "1" }\n')
        tagged = self.commit("chore(release): set version 0.2.3")
        self.git("tag", "-a", "v0.2.3", "-m", "v0.2.3")
        self.git("push", "-q", "origin", "main", "v0.2.3")
        self.compiled(tagged)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds(), [])
        # Kept in the store: later runs ask origin no more, and say why.
        self.git("remote", "set-url", "origin", str(self.root / "gone.git"))
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds(), [])
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            prebuild.status(self.store, self.sentinel, as_json=False)
        self.assertIn(f"skipped  {tagged}  0.2.3, released already", output.getvalue())
        self.assertEqual(prebuild.wait(self.store, self.sentinel, tagged, timeout=None), 1)
        self.git("remote", "set-url", "origin", str(origin))
        # Requested, it builds all the same.
        with patch.object(sys, "argv", [PROGRAM_PATH, "--repository", str(self.repository),
                                        "--store", str(self.store.root), "request", tagged]):
            prebuild.main()
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(len(self.builds()), 1)
        (self.repository / "Cargo.toml").write_text('[package]\nname = "cagents"\nversion = "0.2.4"\n')
        bump = self.commit("chore(release): start 0.2.4 after v0.2.3")
        self.compiled(bump)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual([build[0] for build in self.builds()], [tagged, bump])

    def test_a_failed_build_is_recorded_with_its_log_and_tried_once_more_an_hour_later(self):
        self.compiled(self.first)
        (self.root / "fail").touch()
        self.assertEqual(self.run_once(), 0)
        failure = json.loads((self.store.failures / f"{self.first}.json").read_text())
        self.assertIn("status 3", failure["status"])
        self.assertIn("the build broke", (self.store.failures / f"{self.first}.log").read_text())
        self.assertFalse(self.store.built(self.first))
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(len(self.builds()), 1)
        for attempts in (2, 2):
            failure["failed_at"] -= prebuild.RETRY_FAILED_SECONDS + 1
            (self.store.failures / f"{self.first}.json").write_text(json.dumps(failure))
            self.assertEqual(self.run_once(), 0)
            failure = json.loads((self.store.failures / f"{self.first}.json").read_text())
            self.assertEqual(failure["attempts"], attempts)
        # The second failure is final.
        self.assertEqual(len(self.builds()), 2)

    def test_a_bundle_naming_another_commit_is_a_failure_not_a_bundle(self):
        self.compiled(self.first)
        (self.root / "other-commit").touch()
        self.assertEqual(self.run_once(), 0)
        self.assertIn("does not name this commit", self.store.failure(self.first)["status"])
        self.assertFalse(self.store.bundle(self.first).exists())
        self.assertEqual(list(self.store.bundles.glob(".*.partial")), [])

    def test_an_incomplete_bundle_a_crash_left_is_replaced_and_stale_cache_runs_cleared(self):
        self.compiled(self.first)
        self.store.prepare()
        (self.store.bundle(self.first) / "dist").mkdir(parents=True)
        (self.cache / "run.left").mkdir(parents=True)
        (self.cache / ".lock").touch()
        self.assertEqual(self.run_once(), 0)
        self.assertTrue(self.store.built(self.first))
        self.assertFalse((self.cache / "run.left").exists())

    def test_it_does_not_start_while_memory_is_short_or_under_pressure_or_a_build_waits(self):
        self.compiled(self.first)
        for prepare in (lambda: self.memory(prebuild.START_GIB - 1),
                        lambda: (self.proc / "pressure/memory").write_text("some avg10=40.00 avg60=0 avg300=0 total=0\n"),
                        self.waiter):
            with self.subTest(prepare=prepare):
                self.setUp_readings()
                prepare()
                self.assertEqual(self.run_once(), "waited")
                self.assertEqual(self.builds(), [])

    def setUp_readings(self):
        self.memory(32)
        (self.proc / "pressure/memory").write_text("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
        self.release_waiters()

    def test_a_build_waiting_for_shares_stops_the_background_build_which_resumes_later(self):
        self.compiled(self.first)
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the build")
            # A build only passing through admission does not stop it; one
            # that keeps waiting does.
            self.waiter()
            thread.join(HANG_GUARD)
        self.assertEqual(outcome, ["waited"])
        self.assertTrue((self.root / f"stopped-{self.first}").exists())
        state = json.loads((self.store.root / "state.json").read_text())
        self.assertIn("waiting for build memory shares", state["stopped"])
        self.assertGreater(state["retry_after"], time.time() + 500)
        self.assertFalse(self.store.built(self.first))
        self.assertIsNone(self.store.failure(self.first))
        # Until the backoff passes it waits, however free the host is.
        self.setUp_readings()
        self.assertEqual(self.run_once(), "waited")
        self.assertEqual(len(self.builds()), 1)
        state["retry_after"] = time.time() - 1
        (self.store.root / "state.json").write_text(json.dumps(state))
        self.assertEqual(self.run_once(), 0)
        self.assertTrue(self.store.built(self.first))
        self.assertEqual(list(self.store.bundles.glob(".*")), [])

    def old_commit(self, message, hours):
        stamp = f"@{int(time.time() - hours * 3600)} +0000"
        with patch.dict(os.environ, GIT_COMMITTER_DATE=stamp, GIT_AUTHOR_DATE=stamp):
            return self.commit(message)

    def test_main_two_hours_without_a_bundle_starves_the_prebuild_which_builds_as_any_other(self):
        old = self.old_commit("old", 3)
        self.compiled(old)
        # No bundle, and its commit is three hours old: starved at once, it
        # builds in the build slice and queues without a deadline.
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["normal", "unbounded"])
        self.assertEqual(json.loads((self.store.bundle(old) / "bundle.json").read_text())["priority"], "starved")
        # After a quiet night the morning's commit is new: it yields as ever.
        morning = self.commit("morning")
        self.compiled(morning)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["1", "10"])
        # Commits newer than every bundle for two hours starve it again,
        # however new the tip.
        self.old_commit("unbundled", 2.5)
        later = self.commit("later")
        self.compiled(later)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["normal", "unbounded"])

    def test_half_an_hour_of_making_way_starves_it_and_waiters_no_longer_stop_it(self):
        self.compiled(self.first)
        self.store.prepare()
        (self.store.root / "state.json").write_text(json.dumps({
            "sha": self.first, "first_tried_at": time.time() - 3600, "stopped_since": time.time() - 31 * 60,
            "retry_after": time.time() - 1, "stopped": "a build is waiting for build memory shares"}))
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the build")
            self.waiter()
            time.sleep(prebuild.WATCH_SECONDS * (prebuild.SUSTAINED + 3))
            self.assertFalse((self.root / f"stopped-{self.first}").exists())
            # Less than 6 GiB, which stops a background build, does not stop
            # it; memory running out does.
            self.memory(4)
            time.sleep(prebuild.WATCH_SECONDS * (prebuild.SUSTAINED + 3))
            self.assertFalse((self.root / f"stopped-{self.first}").exists())
            self.memory(1)
            thread.join(HANG_GUARD)
        self.assertEqual(outcome, ["waited"])
        state = json.loads((self.store.root / "state.json").read_text())
        self.assertIn("GiB of memory", state["stopped"])
        self.assertEqual(state["starved"], "waited")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            prebuild.status(self.store, self.sentinel, as_json=False)
        self.assertIn(f"starved  {self.first}  (waited)", output.getvalue())
        # Starved, it takes no backoff: it queues again at once.
        self.assertEqual(state["retry_after"], 0)
        self.assertNotIn("stopped  ", output.getvalue())
        self.release_waiters()
        self.setUp_readings()
        state["retry_after"] = time.time() - 1
        (self.store.root / "state.json").write_text(json.dumps(state))
        with patch.dict(os.environ, PREBUILD_GATE_WAIT="0.3"):
            self.assertEqual(self.run_once(), 0)
        bundled = json.loads((self.store.bundle(self.first) / "bundle.json").read_text())
        self.assertEqual(bundled["priority"], "starved")
        # Its wait counts from the first try, an hour ago, to the admission.
        self.assertGreaterEqual(bundled["waited_seconds"], 3600)
        self.assertLess(bundled["seconds"], 60)
        # A bundle ends the starvation.
        self.assertEqual(json.loads((self.store.root / "state.json").read_text()), {})

    def test_a_prebuild_never_started_counts_its_wait_and_starved_starts_on_less_memory(self):
        self.compiled(self.first)
        self.memory(10)
        self.assertEqual(self.run_once(), "waited")
        state = json.loads((self.store.root / "state.json").read_text())
        self.assertLessEqual(time.time() - state["stopped_since"], 5)
        state["stopped_since"] -= 31 * 60
        (self.store.root / "state.json").write_text(json.dumps(state))
        # Starved, it starts above the floor rather than the 16 GiB a
        # background one waits for.
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["normal", "unbounded"])

    def test_the_admission_splits_the_wait_from_the_build(self):
        self.compiled(self.first)
        gate_wait = 1.6
        with patch.dict(os.environ, PREBUILD_GATE_WAIT=str(gate_wait)):
            self.assertEqual(self.run_once(), 0)
        bundled = json.loads((self.store.bundle(self.first) / "bundle.json").read_text())
        # The gate's queue is the wait; the quick build after it is the build.
        self.assertGreaterEqual(bundled["waited_seconds"], gate_wait - 0.1)
        self.assertLess(bundled["seconds"], gate_wait)
        self.assertIn("prebuild-release: admitted ", (self.store.bundle(self.first) / "build.log").read_text())

    def test_queued_at_the_gate_memory_does_not_stop_it_and_once_admitted_does(self):
        self.compiled(self.first)
        with patch.dict(os.environ, PREBUILD_HOLD="1", PREBUILD_GATE_WAIT="0.6"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.store.root / "building.json").exists(), "the queued build")
            self.memory(4)
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the admission")
            thread.join(HANG_GUARD)
        self.assertTrue((self.root / f"stopped-{self.first}").exists())
        self.assertEqual(outcome, ["waited"])
        self.assertIn("6 GiB", json.loads((self.store.root / "state.json").read_text())["stopped"])

    def test_crossing_into_starvation_mid_build_skips_the_backoff(self):
        self.compiled(self.first)
        self.store.prepare()
        (self.store.root / "state.json").write_text(json.dumps({
            "sha": self.first, "first_tried_at": time.time(), "stopped_since": time.time()}))
        with patch.object(prebuild, "STARVED_SECONDS", 1), patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the build")
            time.sleep(1)
            self.waiter()
            # Stopped for the waiter, it is starved by then and queues again
            # at once rather than back off, the waiter no longer stopping it.
            self.wait_for(lambda: len(self.builds()) == 2, "the starved build")
            state = json.loads((self.store.root / "state.json").read_text())
            (self.root / f"release-{self.first}").touch()
            thread.join(HANG_GUARD)
        self.assertEqual([build[2:4] for build in self.builds()], [["1", "10"], ["normal", "unbounded"]])
        self.assertEqual(state["retry_after"], 0)
        self.assertIn("waiting for build memory shares", state["stopped"])
        self.assertEqual(outcome, [0])
        self.assertEqual(self.pauses, [])

    def test_only_a_bundle_of_the_targets_history_holds_off_starvation(self):
        old = self.old_commit("old", 3)
        side = self.commit("side")
        self.git("reset", "-q", "--hard", old)
        self.compiled(old)
        self.store.prepare()
        (self.store.bundle(side)).mkdir()
        (self.store.bundle(side) / "bundle.json").write_text(json.dumps(
            {"sha": side, "version": "0.2.4", "protocol": 9, "built_at": time.time(), "bytes": 1}))
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["normal", "unbounded"])

    def test_a_target_built_by_any_route_ends_its_record(self):
        self.compiled(self.first)
        self.store.prepare()
        long_ago = time.time() - 31 * 60
        record = {"sha": self.first, "first_tried_at": long_ago, "stopped_since": long_ago,
                  "starved": "waited", "retry_after": time.time() + 300, "stopped": "a build is waiting"}
        (self.store.root / "state.json").write_text(json.dumps(record))
        # A request builds the target: nothing is left to build, and the
        # record ends rather than starve whatever comes next.
        with patch.object(sys, "argv", [PROGRAM_PATH, "--repository", str(self.repository),
                                        "--store", str(self.store.root), "request", self.first]):
            prebuild.main()
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(json.loads((self.store.root / "state.json").read_text()), {})
        # Had main moved on before the record ended, the next target still
        # starts afresh: a background build, with no starved status.
        (self.store.root / "state.json").write_text(json.dumps(record))
        second = self.commit("second")
        self.compiled(second)
        self.memory(10)
        self.assertEqual(self.run_once(), "waited")
        state = json.loads((self.store.root / "state.json").read_text())
        self.assertEqual(state["sha"], second)
        self.assertNotIn("starved", state)
        self.assertNotIn("retry_after", state)
        self.assertLessEqual(time.time() - state["stopped_since"], 5)
        self.memory(32)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds()[-1][2:4], ["1", "10"])

    def test_a_newer_target_keeps_the_backoff_and_a_request_keeps_the_record(self):
        self.compiled(self.first)
        self.store.prepare()
        since = time.time() - 600
        (self.store.root / "state.json").write_text(json.dumps({
            "sha": self.first, "first_tried_at": since, "stopped_since": since,
            "retry_after": time.time() + 300, "stopped": "a build is waiting for build memory shares"}))
        second = self.commit("second")
        self.compiled(second)
        self.assertEqual(self.run_once(), "waited")
        self.assertEqual(self.builds(), [])
        state = json.loads((self.store.root / "state.json").read_text())
        self.assertEqual((state["sha"], state["stopped_since"]), (second, since))
        # A request builds without touching it.
        with patch.object(sys, "argv", [PROGRAM_PATH, "--repository", str(self.repository),
                                        "--store", str(self.store.root), "request", self.first]):
            prebuild.main()
        self.assertEqual(self.run_once(), "waited")
        self.assertTrue(self.store.built(self.first))
        self.assertEqual(json.loads((self.store.root / "state.json").read_text())["stopped_since"], since)

    def test_memory_running_short_stops_the_background_build(self):
        self.compiled(self.first)
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the build")
            self.memory(prebuild.FLOOR_GIB - 1)
            thread.join(HANG_GUARD)
        self.assertEqual(outcome, ["waited"])
        self.assertTrue((self.root / f"stopped-{self.first}").exists())

    def test_failing_tests_stop_the_build_and_count_as_its_failure(self):
        self.compiled(self.first)
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the build")
            self.verdict(self.first, False)
            thread.join(HANG_GUARD)
        self.assertEqual(outcome, [0])
        self.assertEqual(self.store.failure(self.first)["status"], "tests failed")
        self.assertFalse(self.store.built(self.first))

    def test_a_request_preempts_the_background_build_and_builds_at_normal_priority(self):
        self.compiled(self.first)
        second = self.commit("second")
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the background build")
            (self.root / f"release-{second}").touch()
            with patch.object(sys, "argv", [PROGRAM_PATH, "--repository", str(self.repository),
                                            "--store", str(self.store.root), "request", second]):
                self.assertEqual(prebuild.main(), 0)
            # Even short of memory, which the background build yields to.
            self.memory(prebuild.FLOOR_GIB - 1)
            self.wait_for(lambda: self.store.built(second), "the requested bundle")
            self.assertEqual(json.loads((self.store.bundle(second) / "bundle.json").read_text())["priority"],
                             "normal")
            thread.join(HANG_GUARD)
        self.assertEqual(self.builds()[1][:5], [second, second, "normal", "unbounded", str(self.cache)])
        self.assertIn(f"start --no-block {prebuild.SERVICE}", (self.root / "systemctl").read_text())
        self.assertEqual(list(self.store.requests.iterdir()), [])

    def test_the_newest_bundles_and_pinned_ones_are_kept(self):
        commits = [self.first] + [self.commit(f"commit {index}") for index in range(4)]
        for sha in commits:
            self.compiled(sha)
            self.assertEqual(self.run_once(keep=10), 0)
        pinned = self.store.bundle(commits[0]) / "pinned"
        pinned.touch()
        self.compiled(commits[-1])
        self.assertEqual(self.run_once(keep=2), 0)
        self.assertEqual(sorted(path.name for path in self.store.kept()),
                         sorted([commits[0], commits[3], commits[4]]))
        # A pin lapses after a day.
        old = time.time() - prebuild.PIN_SECONDS - 1
        os.utime(pinned, (old, old))
        self.assertEqual(self.run_once(keep=2), 0)
        self.assertEqual(sorted(path.name for path in self.store.kept()), sorted(commits[3:]))

    def test_requests_build_in_order_and_one_already_bundled_is_not_requested(self):
        second = self.commit("second")
        third = self.commit("third")
        arguments = [PROGRAM_PATH, "--repository", str(self.repository), "--store", str(self.store.root)]
        for sha in (third, second):
            with patch.object(sys, "argv", [*arguments, "request", sha]):
                prebuild.main()
            time.sleep(0.01)
        self.assertEqual(self.run_once(), 0)
        self.assertEqual([build[1] for build in self.builds()], [third, second])
        self.assertEqual(list(self.store.requests.iterdir()), [])
        # Once evicted, a commit requested while it was bundled is not rebuilt.
        with patch.object(sys, "argv", [*arguments, "request", second]):
            prebuild.main()
        self.assertEqual(list(self.store.requests.iterdir()), [])
        self.assertEqual(self.run_once(keep=0), 0)
        self.assertEqual(len(self.builds()), 2)

    def test_a_lock_on_another_file_with_the_same_inode_number_is_no_waiter(self):
        found = Path(f"{self.lock}.queue").stat()
        (self.proc / "locks").unlink()
        # The test's own process holds no lock on the queue file.
        (self.proc / "locks").write_text(f"1: FLOCK  ADVISORY  WRITE {os.getpid()} 00:20:{found.st_ino} 0 EOF\n")
        self.assertFalse(prebuild.share_waiter())
        self.waiter()
        (self.proc / "locks").unlink()
        (self.proc / "locks").symlink_to("/proc/locks")
        self.assertTrue(prebuild.share_waiter())

    def test_wait_gives_up_at_its_timeout_and_starts_the_service_meanwhile(self):
        self.compiled(self.first)
        arguments = [PROGRAM_PATH, "--repository", str(self.repository), "--store", str(self.store.root),
                     "--sentinel-state", str(self.sentinel)]
        with patch.object(prebuild, "pause", lambda seconds: None), \
                patch.object(sys, "argv", [*arguments, "wait", self.first, "--timeout", "0.2"]):
            self.assertEqual(prebuild.main(), 124)
        self.assertIn(f"start --no-block {prebuild.SERVICE}", (self.root / "systemctl").read_text())

    def test_only_the_gates_admission_timeout_is_a_stop_and_a_smoke_timeout_a_failure(self):
        self.compiled(self.first)
        (self.root / "admission").touch()
        self.assertEqual(self.run_once(), "waited")
        self.assertIsNone(self.store.failure(self.first))
        self.assertIn("admission", json.loads((self.store.root / "state.json").read_text())["stopped"])
        (self.root / "admission").unlink()
        (self.store.root / "state.json").unlink()
        (self.root / "smoke-hangs").touch()
        self.assertEqual(self.run_once(), 0)
        self.assertIn("status 124", self.store.failure(self.first)["status"])

    def test_requesting_the_commit_building_in_the_background_restarts_it_at_normal_priority(self):
        self.compiled(self.first)
        arguments = [PROGRAM_PATH, "--repository", str(self.repository), "--store", str(self.store.root)]
        with patch.dict(os.environ, PREBUILD_HOLD="1"):
            thread, outcome = self.start()
            self.wait_for(lambda: (self.root / f"started-{self.first}").exists(), "the background build")
            with patch.object(sys, "argv", [*arguments, "request", self.first]):
                prebuild.main()
            self.wait_for(lambda: (self.root / f"stopped-{self.first}").exists(), "the background build to stop")
            (self.root / f"release-{self.first}").touch()
            thread.join(HANG_GUARD)
        self.assertEqual(outcome, [0])
        self.assertEqual([build[2] for build in self.builds()], ["1", "normal"])
        self.assertEqual(json.loads((self.store.bundle(self.first) / "bundle.json").read_text())["priority"],
                         "normal")

    def test_wait_for_a_commit_nothing_will_build_says_so(self):
        arguments = [PROGRAM_PATH, "--repository", str(self.repository), "--store", str(self.store.root),
                     "--sentinel-state", str(self.sentinel)]
        with patch.object(prebuild, "pause", lambda seconds: None), \
                patch.object(sys, "argv", [*arguments, "wait", self.first]):
            self.assertEqual(prebuild.main(), 1)

    def test_another_run_holding_the_store_leaves_it_alone(self):
        self.compiled(self.first)
        self.store.prepare()
        with (self.store.root / "lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertEqual(self.run_once(), 0)
        self.assertEqual(self.builds(), [])

    def test_wait_reports_a_bundle_or_a_failure(self):
        self.compiled(self.first)
        (self.root / "fail").touch()
        self.run_once()
        arguments = [PROGRAM_PATH, "--repository", str(self.repository), "--store", str(self.store.root)]
        with patch.object(sys, "argv", [*arguments, "wait", self.first]):
            self.assertEqual(prebuild.main(), 1)
        # A request clears the failure, to build it again.
        (self.root / "fail").unlink()
        with patch.object(sys, "argv", [*arguments, "request", self.first]):
            prebuild.main()
        self.assertEqual(self.run_once(), 0)
        with patch.object(sys, "argv", [*arguments, "wait", self.first]):
            self.assertEqual(prebuild.main(), 0)


class Waited(Exception):
    pass


PROGRAM_PATH = str(ROOT / "scripts/prebuild-release")


if __name__ == "__main__":
    unittest.main()
