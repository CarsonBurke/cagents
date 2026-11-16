#!/usr/bin/python3
"""Offline contracts: invented private directories and TOML, no native app."""
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parent / "fixtures/native-account-registry.py"
spec = importlib.util.spec_from_file_location("native_account_registry", SCRIPT)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
KEY = "11111111-2222-4333-8444-555555555555"


class AccountRegistryFixtureTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="account-registry-contract-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.task = Path(self.directory.name)
        (self.task / ".native-evidence").touch(mode=0o600)
        self.root = self.task / "fixture"
        self.root.mkdir(mode=0o700)
        for name in ("home", "data", "config"):
            (self.root / name).mkdir(mode=0o700)
        # Only this in-process invented filesystem test models a User caller.
        # No subprocess app, provider, daemon or native-evidence is invoked.
        self.environment = mock.patch.object(fixture.os, "environ", {})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def registered(self):
        fixture.prepare(self.root)
        home, path = fixture.paths(self.root)
        path.parent.mkdir(mode=0o700)
        text = (f'version = 5\nrevision = 2\n[defaults]\nclaude = "{KEY}"\n'
                f'[[account]]\nprovider = "claude"\nkey = "{KEY}"\n'
                f'linked_home = "{home}"\nadded_at = 1\n')
        path.write_text(text)
        path.chmod(0o600)
        return home, path, text.encode()

    def test_prepare_only_creates_empty_home_and_never_registers_or_adopts(self):
        result = fixture.prepare(self.root)
        home, registry = fixture.paths(self.root)
        self.assertEqual(list(home.iterdir()), [])
        self.assertEqual(stat.S_IMODE(home.stat().st_mode), 0o700)
        self.assertEqual(result["home"], str(home))
        self.assertFalse(registry.exists())
        (home / "keep").write_text("invented")
        with self.assertRaises(FileExistsError):
            fixture.prepare(self.root)
        self.assertEqual((home / "keep").read_text(), "invented")

    def test_fault_and_restore_preserve_exact_bytes_inode_mode_and_evidence(self):
        home, path, original = self.registered()
        before = fixture.owned(path, private=True)
        fault = fixture.fault(self.root)
        self.assertEqual(path.read_bytes(), fixture.FAULT)
        self.assertEqual(fault["account"], "claude/" + KEY)
        self.assertEqual(fixture.owned(path, private=True), before)
        backup = self.root / "original-account-registry.json"
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o600)
        restored = fixture.restore(self.root)
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(fixture.owned(path, private=True), before)
        self.assertEqual(list(home.iterdir()), [])
        self.assertEqual(json.loads((self.root / "restored-account-registry.json").read_text()), restored)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)

    def test_system_default_unregistered_wrong_provider_and_archived_refuse(self):
        home, path, original = self.registered()
        variants = [original.replace(KEY.encode(), b"system", 1),
                    b"version=5\n", original.replace(b'provider = "claude"', b'provider = "codex"'),
                    ('version=5\narchived=["' + KEY + '"]\n').encode() + original.split(b"revision", 1)[1].replace(b" = 2", b"revision = 2", 1)]
        for raw in variants:
            with self.subTest(raw=raw):
                path.write_bytes(raw)
                with self.assertRaises((ValueError, KeyError)):
                    fixture.fault(self.root)
                self.assertEqual(path.read_bytes(), raw)
                self.assertFalse((self.root / "original-account-registry.json").exists())

    def test_duplicate_matching_registrations_refuse(self):
        home, path, original = self.registered()
        path.write_bytes(original + original[original.index(b"[[account]]"):])
        with self.assertRaises(ValueError):
            fixture.fault(self.root)

    def test_malformed_original_and_unknown_version_refuse_before_backup(self):
        _, path, original = self.registered()
        for raw in [b"[broken", original.replace(b"version = 5", b"version = 6")]:
            path.write_bytes(raw)
            with self.assertRaises(ValueError):
                fixture.fault(self.root)
            self.assertFalse((self.root / "original-account-registry.json").exists())

    def test_no_unknown_registry_overwrite_or_fault_rearming(self):
        _, path, original = self.registered()
        fixture.fault(self.root)
        with self.assertRaises(ValueError):
            fixture.fault(self.root)
        foreign = b"version=5\n# another writer\n"
        path.write_bytes(foreign)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)
        self.assertEqual(path.read_bytes(), foreign)
        self.assertTrue((self.root / "original-account-registry.json").exists())

    def test_replaced_registry_inode_refuses_even_same_fault_bytes(self):
        _, path, _ = self.registered()
        fixture.fault(self.root)
        old = path.with_suffix(".old")
        path.rename(old)
        path.write_bytes(fixture.FAULT)
        path.chmod(0o600)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)
        self.assertEqual(path.read_bytes(), fixture.FAULT)

    def test_replaced_home_identity_refuses(self):
        home, _, _ = self.registered()
        fixture.fault(self.root)
        home.rename(home.with_name("prior-home"))
        home.mkdir(mode=0o700)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)

    def test_symlink_registry_and_parent_never_followed(self):
        _, path, original = self.registered()
        outside = self.task / "outside.toml"
        path.rename(outside)
        path.symlink_to(outside)
        with self.assertRaises(ValueError):
            fixture.fault(self.root)
        self.assertEqual(outside.read_bytes(), original)
        path.unlink()
        path.parent.rename(self.root / "prior-config")
        path.parent.symlink_to(self.root / "prior-config", target_is_directory=True)
        with self.assertRaises(ValueError):
            fixture.fault(self.root)

    def test_registry_backup_modes_and_symlinks_refuse(self):
        _, path, _ = self.registered()
        path.chmod(0o644)
        with self.assertRaises(ValueError):
            fixture.fault(self.root)
        path.chmod(0o600)
        fixture.fault(self.root)
        backup = self.root / "original-account-registry.json"
        backup.chmod(0o644)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)
        backup.chmod(0o600)
        old = backup.with_suffix(".old")
        backup.rename(old)
        backup.symlink_to(old)
        with self.assertRaises(ValueError):
            fixture.restore(self.root)

    def test_hardlinked_registry_cannot_change_an_outside_file(self):
        _, path, original = self.registered()
        outside = self.task / "outside.toml"
        path.rename(outside)
        os.link(outside, path)
        with self.assertRaises(ValueError):
            fixture.fault(self.root)
        self.assertEqual(outside.read_bytes(), original)
        self.assertFalse((self.root / "original-account-registry.json").exists())

    def test_lock_contention_refuses_without_backup(self):
        _, path, _ = self.registered()
        with (path.parent / "accounts.toml.lock").open("a") as lock:
            os.chmod(lock.name, 0o600)
            fixture.fcntl.flock(lock, fixture.fcntl.LOCK_EX)
            with self.assertRaises(BlockingIOError):
                fixture.fault(self.root)
        self.assertFalse((self.root / "original-account-registry.json").exists())

    def test_restore_report_failure_preserves_original_and_backup(self):
        _, path, original = self.registered()
        fixture.fault(self.root)
        with mock.patch.object(fixture, "write_private", side_effect=OSError("invented report failure")):
            with self.assertRaises(OSError):
                fixture.restore(self.root)
        self.assertEqual(path.read_bytes(), original)
        self.assertTrue((self.root / "original-account-registry.json").is_file())
        with self.assertRaises(ValueError):
            fixture.restore(self.root)

    def test_duplicate_backup_refuses_without_mutating_registry(self):
        _, path, original = self.registered()
        fixture.write_private(self.root / "original-account-registry.json", {"existing": True})
        with self.assertRaises(FileExistsError):
            fixture.fault(self.root)
        self.assertEqual(path.read_bytes(), original)

    def test_wrong_task_environment_unmarked_task_and_real_home_refuse(self):
        with mock.patch.object(fixture.os, "environ", {"CAGENTS_EVIDENCE_FIXTURE": "/foreign/fixture"}):
            with self.assertRaises(ValueError):
                fixture.prepare(self.root)
        with mock.patch.object(fixture.pwd, "getpwuid", return_value=type("User", (), {"pw_dir": str(self.task)})()):
            with self.assertRaises(ValueError):
                fixture.prepare(self.root)
        (self.task / ".native-evidence").unlink()
        with self.assertRaises(FileNotFoundError):
            fixture.prepare(self.root)

    def test_inherited_authority_refuses_every_operation_before_path_access(self):
        for name in fixture.AUTHORITY:
            for operation in ("prepare", "fault", "restore"):
                with self.subTest(name=name, operation=operation):
                    result = subprocess.run([sys.executable, "-B", str(SCRIPT), operation, "/nonexistent/fixture"],
                                            env={name: "invented"}, capture_output=True, text=True, check=False)
                    self.assertEqual(result.returncode, 77)
                    self.assertIn("genuine native-evidence User", result.stderr)


if __name__ == "__main__":
    unittest.main()
