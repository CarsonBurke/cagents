#!/usr/bin/env python3
"""Exercise scripts/build-disk-guard with private locks and a stand-in pruner."""

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

    def guard(self, floor, directory=None):
        return subprocess.run(
            [str(GUARD), str(directory or self.root / "absent/build/target")],
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

    def test_without_a_pruner_it_only_refuses(self):
        del self.env["CAGENTS_BUILD_PRUNE"]
        tools = self.root / "bin"
        tools.mkdir()
        for tool in ("sh", "df", "awk", "dirname", "flock", "find", "date"):
            (tools / tool).symlink_to(shutil.which(tool))
        self.env["PATH"] = str(tools)
        result = self.guard(10**9)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("reclaiming", result.stderr)
        self.assertIn("refusing to build", result.stderr)

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
                 "cargo-gate-rustc-guard", "cargo-gate-sync.py", "cargo-offload",
                 "memory-capped", "seed-build-dir"]
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
                     "cargo-gate-sync.py", "cargo-gate-rustc-guard", "cargo-offload",
                     "seed-build-dir", "memory-capped",
                     "build-disk-guard"):
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
