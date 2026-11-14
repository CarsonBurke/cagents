#!/usr/bin/python3
"""Cargo artifact contracts without a compiler, native dependencies or network."""

import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from cargo_artifacts import CargoArtifactError, CargoArtifacts


class BinaryArtifacts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cargo-artifacts-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.product = self.root / 'target with spaces "quotes" \\backslash' / "tool"
        self.product.parent.mkdir()
        self.product.write_text("#!/bin/sh\nexit 0\n")
        self.product.chmod(0o700)
        self.artifacts = CargoArtifacts()

    def report(self, *, name="tool", kind=None, **fields):
        message = {"reason": "compiler-artifact", "target": {
            "name": name, "kind": ["bin"] if kind is None else kind},
            "executable": str(self.product)} | fields
        self.artifacts.read(json.dumps(message) + "\n")

    def test_selects_only_the_named_binary_in_a_mixed_stream(self):
        self.report(name="another-tool")
        self.report(kind=["lib"])
        self.report(kind=["custom-build"])
        self.report(executable=None)
        self.report()
        self.assertEqual(self.artifacts.executable("tool"), self.product)

    def test_repeated_reports_of_one_path_are_one_candidate(self):
        self.report()
        self.report(fresh=True)
        self.assertEqual(self.artifacts.executable("tool"), self.product)

    def test_different_executables_with_the_same_name_are_ambiguous(self):
        self.report()
        other = self.product.with_name("other-tool")
        other.write_bytes(self.product.read_bytes())
        other.chmod(0o700)
        self.report(executable=str(other))
        with self.assertRaisesRegex(CargoArtifactError, "exactly one.*found 2"):
            self.artifacts.executable("tool")

    def test_non_binary_and_unreported_executables_are_never_selected(self):
        self.report(kind=["lib"])
        self.report(name="another-tool")
        with self.assertRaisesRegex(CargoArtifactError, "exactly one.*found 0"):
            self.artifacts.executable("tool")

    def test_invalid_candidate_cannot_be_hidden_by_a_later_valid_report(self):
        for value in ("relative/tool", "", "bad\0path", 42, [], {}):
            with self.subTest(value=value):
                self.artifacts = CargoArtifacts()
                self.report(executable=value)
                self.report()
                with self.assertRaisesRegex(CargoArtifactError, "invalid executable path"):
                    self.artifacts.executable("tool")

    def test_target_kind_must_be_an_array_of_strings(self):
        for value in ("bin", {}, ["bin", 4]):
            with self.subTest(value=value):
                self.artifacts = CargoArtifacts()
                self.report(kind=value)
                with self.assertRaisesRegex(CargoArtifactError, "invalid target kind"):
                    self.artifacts.executable("tool")

    def test_selected_executable_must_exist_and_be_runnable(self):
        missing = self.root / "missing"
        plain = self.root / "plain"
        plain.write_text("not executable\n")
        for value in (missing, self.root, plain):
            with self.subTest(value=value):
                self.artifacts = CargoArtifacts()
                self.report(executable=str(value))
                with self.assertRaisesRegex(CargoArtifactError, "missing or not runnable"):
                    self.artifacts.executable("tool")

    def test_validity_is_checked_when_the_executable_is_used(self):
        self.report()
        self.product.unlink()
        with self.assertRaisesRegex(CargoArtifactError, "missing or not runnable"):
            self.artifacts.executable("tool")

    def test_diagnostics_and_wrapper_noise_do_not_enter_the_artifact_output(self):
        diagnostics = io.StringIO()
        self.artifacts.read("wrapper progress\n", diagnostics=diagnostics)
        self.artifacts.read(json.dumps({"reason": "compiler-message", "message": {
            "rendered": "warning: fixture diagnostic\n"}}), diagnostics=diagnostics)
        self.report()
        self.assertEqual(self.artifacts.executable("tool"), self.product)
        self.assertEqual(diagnostics.getvalue(), "wrapper progress\nwarning: fixture diagnostic\n")

    def test_build_script_messages_remain_available_to_the_qualifier(self):
        message = {"reason": "build-script-executed", "package_id": "generator", "out_dir": "/out"}
        self.assertEqual(self.artifacts.read(json.dumps(message)), message)

    def test_malformed_stream_validation_is_deferred_until_selection(self):
        for value in (None, 3, [], {"reason": "compiler-artifact", "target": None}):
            with self.subTest(value=value):
                self.artifacts = CargoArtifacts()
                self.artifacts.read(json.dumps(value))
                self.report()
                with self.assertRaises(CargoArtifactError):
                    self.artifacts.executable("tool")


if __name__ == "__main__":
    unittest.main()
