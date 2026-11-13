#!/usr/bin/env python3
"""Exercise scripts/build-disk-guard with private locks and a stand-in pruner."""

import fcntl
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
GUARD = ROOT / "scripts/build-disk-guard"


class BuildDiskGuard(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-disk-guard-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.pruned = self.root / "pruned"
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CAGENTS_BUILD_")
        }
        self.env["CAGENTS_BUILD_LOCK"] = str(self.root / "cargo.lock")
        self.env["CAGENTS_BUILD_INSTALLED_GATE"] = ""
        self.env["CAGENTS_BUILD_REMOTE"] = "local"
        self.env["CAGENTS_BUILD_PRUNE"] = f"echo pruned >> {self.pruned}"
        # The schedule is tested on its own; elsewhere it would add prunes.
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "0"
        self.env["CAGENTS_BUILD_PRUNE_STATE"] = str(self.root / "pruner")
        self.env["CAGENTS_BUILD_PRUNE_ROOTS"] = str(self.root / "roots")

    def guard(self, floor, directory=None, guard=GUARD):
        return subprocess.run(
            [str(guard), str(directory or self.root / "absent/build/target")],
            env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": str(floor)},
            capture_output=True, text=True, timeout=60,
        )

    def prunes(self):
        return self.pruned.read_text().count("pruned") if self.pruned.exists() else 0

    def test_enough_room_builds_without_reclaiming(self):
        result = self.guard(1)
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertEqual(self.prunes(), 0)

    def test_disabled_guard_always_builds(self):
        result = self.guard(0)
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertEqual(self.prunes(), 0)

    def test_below_the_floor_reclaims_once_then_refuses(self):
        result = self.guard(10**9)
        self.assertEqual(result.returncode, 2)
        self.assertIn("reclaiming", result.stderr)
        self.assertIn("refusing to build", result.stderr)
        self.assertIn("CAGENTS_BUILD_FREE_FLOOR_GIB=0", result.stderr)
        self.assertEqual(self.prunes(), 1)
        # A queue of builds must not each scan every build directory.
        result = self.guard(10**9)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("reclaiming", result.stderr)
        self.assertEqual(self.prunes(), 1)

    def test_running_low_reclaims_and_still_builds(self):
        free = shutil.disk_usage(self.root).free // 2**30
        if free < 4:
            self.skipTest("too little free space to place a floor below it")
        # Free space between the floor and twice it.
        result = self.guard(free * 2 // 3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("reclaiming", result.stderr)
        self.assertNotIn("refusing", result.stderr)
        self.assertEqual(self.prunes(), 1)

    def test_a_failed_reclaim_still_decides_by_free_space(self):
        self.env["CAGENTS_BUILD_PRUNE"] = "exit 1"
        result = self.guard(10**9)
        self.assertEqual(result.returncode, 2)
        self.assertIn("refusing to build", result.stderr)

    def test_an_invalid_floor_is_a_usage_error(self):
        for floor in ("lots", "08"):
            with self.subTest(floor=floor):
                result = self.guard(floor)
                self.assertEqual(result.returncode, 2)
                self.assertIn("Usage", result.stderr)

    def alone(self):
        """A copy of the guard with no pruner beside it."""
        directory = self.root / "guard"
        directory.mkdir()
        shutil.copy2(GUARD, directory / GUARD.name)
        return directory / GUARD.name

    def test_without_a_pruner_it_says_so_and_refuses(self):
        del self.env["CAGENTS_BUILD_PRUNE"]
        result = self.guard(10**9, guard=self.alone())
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("reclaiming", result.stderr)
        self.assertIn("nothing to reclaim build directories with", result.stderr)
        self.assertIn("prune-build-dirs is missing", result.stderr)
        self.assertIn("refusing to build", result.stderr)

    def test_without_a_pruner_and_with_room_it_says_nothing(self):
        del self.env["CAGENTS_BUILD_PRUNE"]
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        result = self.guard(1, guard=self.alone())
        self.assertEqual((result.returncode, result.stderr), (0, ""))

    def stand_in_pruner(self, status=0, then=""):
        """The guard beside a pruner that records how it was run, and as the
        real one does, stamps the schedule unless it was cut short."""
        guard = self.alone()
        calls = self.root / "calls"
        pruner = guard.with_name("prune-build-dirs")
        pruner.write_text(
            "#!/bin/sh\n"
            'arguments="$*"\n'
            "while [ $# -gt 0 ]; do\n"
            f'    [ "$1" != --stamp ] || [ {status} -eq 75 ] || touch "$2"\n'
            "    shift\n"
            "done\n"
            f"{then}"
            f'printf "%s\\n" "$arguments" >> {calls}\n'
            f"exit {status}\n")
        pruner.chmod(0o755)
        del self.env["CAGENTS_BUILD_PRUNE"]
        return guard, calls

    def calls(self, path, count, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            lines = path.read_text().splitlines() if path.exists() else []
            if len(lines) >= count:
                return lines
            time.sleep(0.05)
        return path.read_text().splitlines() if path.exists() else []

    def test_low_on_space_it_runs_the_pruner_beside_it(self):
        guard, calls = self.stand_in_pruner()
        checkout = self.root / "checkout"
        checkout.mkdir()
        subprocess.run(["git", "init", "-q", str(checkout)], check=True)
        result = subprocess.run(
            [str(guard), str(self.root)], cwd=checkout,
            env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": str(10**9)},
            capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 2)
        self.assertIn("reclaiming unused build directories", result.stderr)
        # A complete run in the background, and the one the build waits for.
        lines = self.calls(calls, 2)
        self.assertEqual(len(lines), 2)
        (background,) = [line for line in lines if "--output" in line]
        self.assertNotIn("--budget", background)
        self.assertIn("--orphan-hours 0 --evidence-hours 1", background)
        (call,) = [line for line in lines if "--output" not in line]
        self.assertIn("--apply --max-idle 7", call)
        self.assertIn("--orphan-hours 0 --evidence-hours 1 --budget 300 --wait", call)
        self.assertIn(f"--stamp {self.root}/pruner.scheduled", call)
        self.assertIn(f"--keep {self.root}", call)
        self.assertIn(f"--root {checkout.resolve()} --keep-checkout {checkout.resolve()}", call)
        self.assertIn(f"--log {self.root}/pruner.tsv", call)

    def test_under_pressure_the_schedule_starts_nothing_more(self):
        guard, calls = self.stand_in_pruner()
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        result = self.guard(10**9, guard=guard)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(len(self.calls(calls, 2)), 2)
        # That complete run was this period's: a build with room starts none.
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        time.sleep(0.5)
        self.assertEqual(len(self.calls(calls, 2)), 2)

    def test_a_reclaim_cut_short_leaves_the_scheduled_run_to_come(self):
        guard, calls = self.stand_in_pruner(status=75)
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        self.assertEqual(self.guard(10**9, guard=guard).returncode, 2)
        self.assertEqual(len(self.calls(calls, 2)), 2)
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        lines = self.calls(calls, 3)
        self.assertEqual(len(lines), 3)
        # The background runs, below the floor and on the schedule.
        self.assertEqual(len([line for line in lines if "--budget" not in line]), 2)

    def test_a_scheduled_run_cut_short_is_started_again(self):
        guard, calls = self.stand_in_pruner(status=75)
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        self.calls(calls, 1)
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        self.assertEqual(len(self.calls(calls, 2)), 2)

    def test_no_run_starts_while_one_is_pruning(self):
        guard, calls = self.stand_in_pruner()
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        with open(self.root / "pruner.lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertEqual(self.guard(1, guard=guard).returncode, 0)
            time.sleep(0.5)
            self.assertEqual(self.calls(calls, 0, timeout=0), [])

    def band(self):
        """A floor that leaves free space between it and twice it."""
        free = shutil.disk_usage(self.root).free // 2**30
        if free < 4:
            self.skipTest("too little free space to place a floor below it")
        return free * 2 // 3

    def test_running_low_the_pruner_runs_in_the_background(self):
        guard, calls = self.stand_in_pruner(then="sleep 3\n")
        started = time.monotonic()
        result = self.guard(self.band(), guard=guard)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("reclaiming unused build directories in the background", result.stderr)
        (call,) = self.calls(calls, 1)
        # Running low, a native-evidence build goes after an hour unused.
        self.assertIn("--orphan-hours 0 --evidence-hours 1", call)
        self.assertNotIn("--budget", call)
        self.assertIn(f"--output {self.root}/pruner.log", call)
        # Once it has searched everything, the next hour's builds start none.
        result = self.guard(self.band(), guard=guard)
        self.assertNotIn("reclaiming", result.stderr)
        time.sleep(0.5)
        self.assertEqual(len(self.calls(calls, 1)), 1)
        long_ago = time.time() - 2 * 3600
        os.utime(self.root / "pruner.scheduled", (long_ago, long_ago))
        self.assertIn("in the background", self.guard(self.band(), guard=guard).stderr)

    def stand_in_offload(self, guard):
        """A stand-in for the offload helper beside `guard`, recording how it
        was run in `trims`."""
        trims = self.root / "trims"
        offload = guard.with_name("cargo-offload")
        offload.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {trims}\n')
        offload.chmod(0o755)
        return trims

    def test_running_low_idle_offload_slots_are_trimmed_before_the_pruner_runs(self):
        guard, calls = self.stand_in_pruner()
        trims = self.stand_in_offload(guard)
        floor = self.band()
        result = self.guard(floor, guard=guard)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.calls(calls, 1)
        # Beside the pruner's detached run, without waiting for what it frees.
        self.assertEqual(self.calls(trims, 1), [f"--trim-slots {floor}"])

    def test_below_the_floor_idle_offload_slots_are_trimmed_and_waited_for(self):
        guard, calls = self.stand_in_pruner()
        trims = self.stand_in_offload(guard)
        result = self.guard(10**9, guard=guard)
        self.assertEqual(result.returncode, 2)
        self.calls(calls, 2)
        self.assertEqual(sorted(self.calls(trims, 2)),
                         [f"--trim-slots {10**9}", f"--trim-slots {10**9} --wait"])

    def test_without_timeout_the_trim_below_the_floor_is_not_waited_for(self):
        guard, calls = self.stand_in_pruner()
        trims = self.root / "trims"
        offload = guard.with_name("cargo-offload")
        offload.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {trims}\nsleep 5\n')
        offload.chmod(0o755)
        # Every command the guard could find but timeout.
        tools = self.root / "tools"
        tools.mkdir()
        path_bin = self.root / "path-bin"
        path_bin.mkdir()
        (path_bin / "dangling-command").symlink_to(self.root / "missing-command")
        # A repeated PATH directory must not recreate an already copied,
        # dangling symlink. Keep this reproducible with any caller's PATH.
        for directory in [str(path_bin), str(path_bin), *os.environ["PATH"].split(os.pathsep)]:
            for entry in os.scandir(directory) if os.path.isdir(directory) else ():
                target = tools / entry.name
                if entry.name != "timeout" and not (target.exists() or target.is_symlink()):
                    target.symlink_to(entry.path)
        self.assertTrue((tools / "dangling-command").is_symlink())
        self.assertFalse((tools / "dangling-command").exists())
        started = time.monotonic()
        result = subprocess.run(
            [str(guard), str(self.root / "absent/build/target")],
            env={**self.env, "PATH": str(tools), "CAGENTS_BUILD_FREE_FLOOR_GIB": str(10**9)},
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertLess(time.monotonic() - started, 4)
        self.assertEqual(self.calls(trims, 2), [f"--trim-slots {10**9}"] * 2)

    def test_a_reclaim_command_of_its_own_leaves_offload_slots_alone(self):
        guard = self.alone()
        trims = self.stand_in_offload(guard)
        result = self.guard(10**9, guard=guard)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.prunes(), 1)
        time.sleep(0.5)
        self.assertFalse(trims.exists())

    def test_the_background_trim_keeps_none_of_the_builds_descriptors(self):
        # As the real trim closes what it inherited as it starts, and then
        # runs on; the shell that starts it must not hold them meanwhile.
        guard, calls = self.stand_in_pruner(then="exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-\n")
        offload = guard.with_name("cargo-offload")
        offload.write_text("#!/bin/sh\nexec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-\nsleep 5\n")
        offload.chmod(0o755)
        read, write = os.pipe()
        started = time.monotonic()
        with subprocess.Popen([str(guard), str(self.root)], pass_fds=(write,),
                              env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": str(self.band())},
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as guarding:
            os.close(write)
            self.assertEqual(guarding.wait(timeout=30), 0)
            self.calls(calls, 1)
            # The pipe closes once nothing of the build's holds it.
            self.assertEqual(os.read(read, 1), b"")
        os.close(read)
        self.assertLess(time.monotonic() - started, 4)

    def test_the_background_run_keeps_none_of_the_builds_descriptors(self):
        # As the real pruner closes what it inherited, and then runs on.
        guard, calls = self.stand_in_pruner(
            then="exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-\nsleep 5\n")
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        # Like `flock FILE command`, which passes its lock to the command.
        read, write = os.pipe()
        started = time.monotonic()
        with os.fdopen(read, "rb") as reader:
            result = subprocess.run(
                [str(guard), str(self.root / "absent/target")], pass_fds=(write,),
                env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": "1"},
                capture_output=True, timeout=20)
            os.close(write)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(reader.read(), b"")
        self.assertLess(time.monotonic() - started, 4)
        self.calls(calls, 1)

    def test_a_build_directory_in_use_is_marked_so(self):
        build = self.root / "build/target"
        build.mkdir(parents=True)
        (build / ".rustc_info.json").write_text("{}")
        self.assertEqual(self.guard(1, directory=build).returncode, 0)
        self.assertTrue((build / ".cagents-gate-used").is_file())
        # Never one Cargo has not made.
        plain = self.root / "plain"
        plain.mkdir()
        self.assertEqual(self.guard(1, directory=plain).returncode, 0)
        self.assertEqual(list(plain.iterdir()), [])

    def test_the_build_directory_it_guards_is_kept(self):
        guard, calls = self.stand_in_pruner()
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        build = self.root / "it's here/target"
        self.assertEqual(self.guard(1, directory=build, guard=guard).returncode, 0)
        (call,) = self.calls(calls, 1)
        self.assertIn(f"--keep {build}", call)

    def test_on_schedule_it_prunes_in_the_background_whatever_the_space(self):
        guard, calls = self.stand_in_pruner()
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        self.env["CAGENTS_BUILD_PRUNE_IDLE_DAYS"] = "3"
        result = self.guard(1, guard=guard)
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        (call,) = self.calls(calls, 1)
        self.assertIn("--apply --max-idle 3", call)
        # The pruner's own grace periods, not those for running low.
        self.assertNotIn("--orphan-hours", call)
        self.assertNotIn("--evidence-hours", call)
        # Once it has finished, and within the interval, no build starts another.
        stamp = self.root / "pruner.scheduled"
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        time.sleep(0.5)
        self.assertEqual(len(self.calls(calls, 1)), 1)
        # Once it has passed, the next build does.
        long_ago = time.time() - 7 * 3600
        os.utime(stamp, (long_ago, long_ago))
        self.assertEqual(self.guard(1, guard=guard).returncode, 0)
        self.assertEqual(len(self.calls(calls, 2)), 2)

    def test_the_build_does_not_wait_for_the_scheduled_prune(self):
        guard = self.alone()
        pruner = guard.with_name("prune-build-dirs")
        pidfile = self.root / "pruner.pid"
        pruner.write_text(f'#!/bin/sh\necho $$ > {pidfile}\nexec sleep 30\n')
        pruner.chmod(0o755)
        del self.env["CAGENTS_BUILD_PRUNE"]
        self.env["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "6"
        started = time.monotonic()
        result = subprocess.run(
            [str(guard), str(self.root)],
            env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": "1"},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
        self.assertEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 10)
        deadline = time.monotonic() + 10
        while not pidfile.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        os.kill(int(pidfile.read_text()), 9)

    def test_an_invalid_schedule_is_a_usage_error(self):
        for name, value in (("CAGENTS_BUILD_PRUNE_INTERVAL_HOURS", "08"),
                            ("CAGENTS_BUILD_PRUNE_INTERVAL_HOURS", "999999"),
                            ("CAGENTS_BUILD_PRUNE_IDLE_DAYS", "week"),
                            ("CAGENTS_BUILD_PRUNE_IDLE_DAYS", "0"),
                            ("CAGENTS_BUILD_PRUNE_BUDGET_SECONDS", "-1"),
                            ("CAGENTS_BUILD_PRUNE_BUDGET_SECONDS", "0")):
            with self.subTest(name=name, value=value):
                self.env[name] = value
                result = self.guard(1)
                self.assertEqual(result.returncode, 2)
                self.assertIn("Usage", result.stderr)
                del self.env[name]

    def test_a_build_waits_for_another_builds_reclaim(self):
        self.env["CAGENTS_BUILD_PRUNE"] = f"sleep 2; echo pruned >> {self.pruned}"
        first = subprocess.Popen(
            [str(GUARD), str(self.root)],
            env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": str(10**9)},
            stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(first.kill)
        time.sleep(0.5)
        started = time.monotonic()
        result = self.guard(10**9)
        self.assertGreaterEqual(time.monotonic() - started, 1)
        self.assertIn("waiting for another build", result.stderr)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(first.wait(timeout=30), 2)
        first.stderr.close()
        self.assertEqual(self.prunes(), 1)

    def test_the_gate_refuses_before_admitting(self):
        gate = self.root / "gate"
        gate.mkdir()
        for name in ("cargo-gate", "cargo-gate-command.py", "build-disk-guard"):
            shutil.copy2(ROOT / "scripts" / name, gate / name)
        result = subprocess.run(
            [str(gate / "cargo-gate"), "exec", "sh", "-c", "echo admitted"],
            cwd=ROOT, env={**self.env, "CAGENTS_BUILD_FREE_FLOOR_GIB": str(10**9),
                           "CAGENTS_BUILD_SLOT_HELD": ""},
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertNotIn("admitted", result.stdout)
        self.assertIn("refusing to build", result.stderr)


class InstallCargoGate(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-install-gate-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CAGENTS_BUILD_")
        }

    def test_installs_the_gate_with_every_helper(self):
        result = subprocess.run(
            [str(ROOT / "scripts/install-cargo-gate"), str(self.root)],
            env=self.env, capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        names = ["build-disk-guard", "cagents-cargo-gate", "cargo-gate-command.py",
                 "cargo-host-test-helper.py",
                 "cargo-gate-rustc-guard", "cargo-gate-sync.py", "cargo-offload",
                 "memory-capped", "prune-build-dirs", "seed-build-dir"]
        self.assertEqual(
            sorted(path.name for path in self.root.iterdir()),
            sorted(names + ["cagents-cargo-gate.d", "cagents-cargo-gate.install.lock",
                            "cagents-cargo-gate.installation"]),
        )
        # Every name leads into one installation of its own, which has the
        # installer that made it.
        (installation,) = (self.root / "cagents-cargo-gate.d").iterdir()
        for name in names:
            self.assertEqual((self.root / name).resolve(), installation / name)
        self.assertTrue((installation / "install-cargo-gate").is_file())

    def test_an_installation_leaves_the_running_one_whole(self):
        install = [str(ROOT / "scripts/install-cargo-gate"), str(self.root)]
        subprocess.run(install, env=self.env, check=True, capture_output=True, timeout=60)
        (first,) = (self.root / "cagents-cargo-gate.d").iterdir()
        contents = {path.name: path.read_bytes() for path in first.iterdir()}
        subprocess.run(install, env=self.env, check=True, capture_output=True, timeout=60)
        self.assertEqual({path.name: path.read_bytes() for path in first.iterdir()}, contents)
        self.assertNotEqual((self.root / "cagents-cargo-gate").resolve().parent, first)

    def test_an_installation_replaced_long_ago_is_removed_and_a_recent_one_kept(self):
        install = [str(ROOT / "scripts/install-cargo-gate"), str(self.root)]
        versions = self.root / "cagents-cargo-gate.d"
        subprocess.run(install, env=self.env, check=True, capture_output=True, timeout=60)
        (old,) = versions.iterdir()
        ancient = time.time() - 30 * 86400
        os.utime(old, (ancient, ancient))
        # Made long ago, replaced just now: running builds may still use it.
        subprocess.run(install, env=self.env, check=True, capture_output=True, timeout=60)
        self.assertTrue(old.is_dir())
        (replaced,) = set(versions.iterdir()) - {old}
        os.utime(old, (ancient, ancient))
        os.utime(replaced, (ancient, ancient))
        subprocess.run(install, env=self.env, check=True, capture_output=True, timeout=60)
        self.assertFalse(old.exists())
        self.assertTrue(replaced.is_dir())

    def test_a_gate_that_cannot_run_is_not_installed(self):
        scripts = self.root / "checkout/scripts"
        scripts.mkdir(parents=True)
        for name in ("install-cargo-gate", "cargo-gate", "cargo-gate-command.py",
                     "cargo-host-test-helper.py",
                     "cargo-gate-sync.py", "cargo-gate-rustc-guard", "cargo-offload",
                     "seed-build-dir", "memory-capped",
                     "build-disk-guard", "prune-build-dirs"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        (scripts / "cargo-gate-command.py").write_text("import sys\nsys.exit(3)\n")
        installed = self.root / "installed"
        installed.mkdir()
        (installed / "cagents-cargo-gate").write_text("previous\n")
        result = subprocess.run(
            [str(scripts / "install-cargo-gate"), str(installed)],
            env=self.env, capture_output=True, text=True, timeout=60,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not run a command", result.stderr)
        self.assertEqual(sorted(path.name for path in installed.iterdir()),
                         ["cagents-cargo-gate", "cagents-cargo-gate.d",
                          "cagents-cargo-gate.install.lock"])
        self.assertEqual(list((installed / "cagents-cargo-gate.d").iterdir()), [])
        self.assertEqual((installed / "cagents-cargo-gate").read_text(), "previous\n")


if __name__ == "__main__":
    unittest.main()
