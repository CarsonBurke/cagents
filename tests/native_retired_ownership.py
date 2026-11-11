#!/usr/bin/env python3
"""Offline safety contracts for the isolated retired-ownership fault fixture."""

import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "fixtures/native-retired-ownership.py"
spec = importlib.util.spec_from_file_location("native_retired_ownership", SCRIPT)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RetiredOwnershipFixtureTests(unittest.TestCase):
    def test_recognized_schemas_and_exact_restore_guards(self):
        result = fixture.self_test()
        self.assertEqual(result["schemas"], [22, 23])
        self.assertEqual(result["selection_checks_passed"], 16)
        self.assertEqual(result["restore_checks_passed"], 18)

    def test_hosted_authority_refuses_both_mutations_before_opening_paths(self):
        environment = dict(os.environ, CAGENTS_AGENT_AUTHORITY="fixture-contract")
        for operation in [[], ["--restore"]]:
            with self.subTest(operation=operation):
                result = subprocess.run(
                    [sys.executable, "-B", str(SCRIPT), "/nonexistent/fixture", "invented-thread", *operation],
                    env=environment, capture_output=True, text=True, check=False,
                )
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertIn("genuine native-evidence User environment", result.stderr)

    def test_offline_checks_remain_available_with_hosted_authority(self):
        result = subprocess.run(
            [sys.executable, "-B", str(SCRIPT), "--self-test"],
            env=dict(os.environ, CAGENTS_AGENT_AUTHORITY="fixture-contract"),
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["restore_checks_passed"], 18)

    def test_backup_is_private_exclusive_and_refuses_symlink_targets(self):
        with tempfile.TemporaryDirectory(prefix="retired-ownership-contract-", dir="/var/tmp") as directory:
            root = Path(directory)
            backup = root / "backup.json"
            fixture.write_private(backup, {"invented": True})
            self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o600)
            self.assertEqual(backup.stat().st_uid, os.getuid())
            self.assertEqual(json.loads(backup.read_text()), {"invented": True})
            with self.assertRaises(FileExistsError):
                fixture.write_private(backup, {"replacement": True})
            symlink = root / "symlink.json"
            symlink.symlink_to(backup)
            with self.assertRaises(OSError):
                fixture.write_private(symlink, {"replacement": True})
            with self.assertRaises(ValueError):
                fixture.owned_regular(symlink, private=True)
            backup.chmod(0o644)
            with self.assertRaises(ValueError):
                fixture.owned_regular(backup, private=True)
            self.assertEqual(json.loads(backup.read_text()), {"invented": True})


if __name__ == "__main__":
    unittest.main()
