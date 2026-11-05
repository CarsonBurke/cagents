#!/usr/bin/env python3
"""Exercise scripts/build-disk-guard with private locks and a stand-in pruner."""

import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import unittest


GUARD = Path(__file__).resolve().parents[1] / "scripts/build-disk-guard"


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
        result = self.guard("lots")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage", result.stderr)


if __name__ == "__main__":
    unittest.main()
