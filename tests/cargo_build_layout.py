#!/usr/bin/python3
"""Private Cargo layout contracts using synthetic trees, never a compiler."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from cargo_build_layout import ArtifactUnit, BuildProfile, CargoDirectories


class CargoBuildLayout(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cargo-build-layout-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.target = self.root / 'public target with spaces "quotes"'
        self.build = self.root / "private build"
        self.directories = CargoDirectories(self.target, self.build)

    def write(self, path, text="fixture\n"):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def legacy(self, package="some-crate", unit_hash="0a1b", profile="debug"):
        base = self.build / profile
        fingerprint = self.write(base / f".fingerprint/{package}-{unit_hash}/lib-some_crate").parent
        library = self.write(base / f"deps/libsome_crate-{unit_hash}.rlib")
        dependencies = self.write(base / f"deps/some_crate-{unit_hash}.d")
        output = self.write(base / f"build/{package}-{unit_hash}/output", "cargo:rerun-if-changed=input\n")
        return fingerprint, library, dependencies, output

    def modern(self, package="some-crate", unit_hash="0123456789abcdef", profile="debug"):
        unit = self.build / profile / "build" / package / unit_hash
        fingerprint = self.write(unit / "fingerprint/lib-some_crate").parent
        library = self.write(unit / "out/libsome_crate.rlib")
        dependencies = self.write(unit / "out/some_crate.d")
        output = self.write(unit / "run/stdout", "cargo::rerun-if-changed=input\n")
        return fingerprint, library, dependencies, output

    def assert_unknown(self, profile):
        self.assertIsInstance(profile, BuildProfile)
        self.assertFalse(profile.known_layout)
        self.assertIsNone(profile.layout)
        self.assertEqual(profile.units, ())
        self.assertEqual(profile.dependency_lists, ())
        self.assertEqual(profile.build_script_outputs, ())

    def test_metadata_keeps_separate_build_and_target_directories(self):
        metadata = json.loads(json.dumps({"target_directory": str(self.target), "build_directory": str(self.build)}))
        self.assertEqual(CargoDirectories.from_metadata(metadata), self.directories)

    def test_missing_null_or_empty_build_directory_falls_back_to_target(self):
        for extra in ({}, {"build_directory": None}, {"build_directory": ""}):
            with self.subTest(extra=extra):
                directories = CargoDirectories.from_metadata({"target_directory": str(self.target)} | extra)
                self.assertEqual(directories, CargoDirectories(self.target, self.target))

    def test_missing_or_invalid_target_metadata_disables_discovery(self):
        for metadata in (None, [], "{}", {}, {"build_directory": str(self.build)},
                         *({"target_directory": value} for value in (None, "", "relative", 3, [], "/bad\0path"))):
            with self.subTest(metadata=metadata):
                self.assertIsNone(CargoDirectories.from_metadata(metadata))

    def test_invalid_build_metadata_never_falls_back_silently(self):
        for value in (False, 3, [], {}, "relative", "/bad\0path"):
            with self.subTest(value=value):
                self.assertIsNone(CargoDirectories.from_metadata({"target_directory": str(self.target),
                                                                 "build_directory": value}))

    def test_legacy_units_keep_fingerprint_first_and_match_outputs_by_hash(self):
        fingerprint, library, dependencies, output = self.legacy()
        profile = self.directories.profile("debug")
        self.assertEqual(profile.layout, "legacy")
        self.assertEqual(profile.target_directory, self.target / "debug")
        self.assertEqual(profile.build_directory, self.build / "debug")
        self.assertEqual(profile.units, (ArtifactUnit("some-crate", "0a1b", fingerprint,
                                                     (fingerprint, library, dependencies, output.parent),
                                                     (dependencies,), (output,)),))
        self.assertEqual(profile.dependency_lists, (dependencies,))
        self.assertEqual(profile.build_script_outputs, (output,))

    def test_modern_units_report_whole_directory_and_immediate_records(self):
        fingerprint, library, dependencies, output = self.modern(package="ends-in-abc")
        profile = self.directories.profile("debug")
        self.assertEqual(profile.layout, "unit")
        self.assertEqual(profile.units, (ArtifactUnit("ends-in-abc", "0123456789abcdef", fingerprint,
                                                     (fingerprint.parent,), (dependencies,), (output,)),))
        self.assertTrue(library.is_file())
        self.assertEqual(profile.dependency_lists, (dependencies,))
        self.assertEqual(profile.build_script_outputs, (output,))

    def test_mixed_layout_reports_each_unit_once_in_stable_order(self):
        legacy = self.legacy(package="z-old")
        modern = self.modern(package="a-new")
        profile = self.directories.profile("debug")
        self.assertEqual(profile.layout, "mixed")
        self.assertEqual([unit.package for unit in profile.units], ["a-new", "z-old"])
        self.assertEqual(profile.dependency_lists, tuple(sorted((legacy[2], modern[2]))))
        self.assertEqual(profile.build_script_outputs, tuple(sorted((legacy[3], modern[3]))))

    def test_modern_package_ending_in_a_legacy_hash_is_not_that_legacy_unit(self):
        unit_hash = "0123456789abcdef"
        legacy = self.legacy(package="old", unit_hash=unit_hash)
        modern = self.modern(package=f"new-{unit_hash}", unit_hash="fedcba9876543210")
        profile = self.directories.profile("debug")
        self.assertEqual(profile.layout, "mixed")
        self.assertEqual(profile.units, (
            ArtifactUnit(f"new-{unit_hash}", "fedcba9876543210", modern[0],
                         (modern[0].parent,), (modern[2],), (modern[3],)),
            ArtifactUnit("old", unit_hash, legacy[0],
                         (legacy[0], legacy[1], legacy[2], legacy[3].parent), (legacy[2],), (legacy[3],)),
        ))
        self.assertEqual(profile.dependency_lists, tuple(sorted((legacy[2], modern[2]))))
        self.assertEqual(profile.build_script_outputs, tuple(sorted((legacy[3], modern[3]))))

    def test_legacy_build_directory_holding_modern_units_is_unknown(self):
        unit_hash = "0123456789abcdef"
        self.legacy(package="same", unit_hash=unit_hash)
        self.modern(package=f"same-{unit_hash}", unit_hash="fedcba9876543210")
        self.assert_unknown(self.directories.profile("debug"))

    def test_private_inventory_uses_build_directory_and_public_lists_use_target(self):
        private = self.modern()
        public = self.write(self.target / "debug/cagents.d")
        self.write(self.target / "debug/units/misleading.d")
        profile = self.directories.profile("debug")
        self.assertEqual(profile.dependency_lists, tuple(sorted((private[2], public))))
        self.assertEqual(profile.units[0].fingerprint, private[0])
        self.assertEqual(profile.build_script_outputs, (private[3],))

    def test_combined_build_and_target_deduplicates_public_lists(self):
        self.legacy()
        public = self.write(self.build / "debug/cagents.d")
        directories = CargoDirectories.from_metadata({"target_directory": str(self.build)})
        profile = directories.profile("debug")
        self.assertTrue(profile.known_layout)
        self.assertEqual(profile.dependency_lists.count(public), 1)

    def test_profiles_discover_custom_profiles_and_target_triples_at_bounded_depth(self):
        self.legacy(profile="dev-runtime")
        self.modern(profile="aarch64-unknown-linux-gnu/release")
        self.write(self.build / ".rustc_info.json", "{}")
        self.write(self.build / "CACHEDIR.TAG")
        profiles = self.directories.profiles()
        self.assertEqual([profile.build_directory.relative_to(self.build).as_posix() for profile in profiles],
                         ["aarch64-unknown-linux-gnu/release", "dev-runtime"])
        self.assertTrue(all(profile.known_layout for profile in profiles))

    def test_explicit_profile_rejects_paths_outside_the_two_supported_depths(self):
        for path in (".", "..", "../debug", "/debug", "triple/../debug", "debug/build/package"):
            with self.subTest(path=path):
                with self.assertRaises(ValueError):
                    self.directories.profile(path)

    def test_empty_missing_or_unknown_profiles_disable_shortcuts(self):
        self.assert_unknown(self.directories.profile("missing"))
        (self.build / "empty").mkdir(parents=True)
        self.assert_unknown(self.directories.profile("empty"))
        self.write(self.build / "future/units/package/compiled.rlib")
        self.assert_unknown(self.directories.profile("future"))
        self.assertEqual(CargoDirectories(self.target, self.root / "absent").profiles(), ())

    def test_unknown_candidate_is_not_reported_as_an_empty_known_profile(self):
        self.write(self.build / "debug/deps/libcrate-1234.rlib")
        profiles = self.directories.profiles()
        self.assertEqual(len(profiles), 1)
        self.assert_unknown(profiles[0])

    def test_empty_legacy_fingerprint_directory_is_a_known_layout(self):
        (self.build / "debug/.fingerprint").mkdir(parents=True)
        profile = self.directories.profile("debug")
        self.assertEqual(profile.layout, "legacy")
        self.assertEqual(profile.units, ())

    def test_malformed_legacy_unit_or_ambiguous_hash_disables_the_whole_profile(self):
        self.legacy()
        for name in ("not-a-hash", "other-crate-0a1b"):
            with self.subTest(name=name):
                bad = self.build / "debug/.fingerprint" / name
                bad.mkdir()
                self.assert_unknown(self.directories.profile("debug"))
                bad.rmdir()

    def test_unknown_deps_beside_known_units_disable_partial_shortcuts(self):
        self.legacy()
        self.write(self.build / "debug/deps/libuntracked-deadbeef.rlib")
        self.assert_unknown(self.directories.profile("debug"))

    def test_unknown_layout_beside_known_fingerprints_disables_partial_shortcuts(self):
        self.legacy()
        self.write(self.build / "debug/units/libfuture-0123456789abcdef.rlib")
        self.assert_unknown(self.directories.profile("debug"))
        profiles = self.directories.profiles()
        self.assertEqual(len(profiles), 1)
        self.assert_unknown(profiles[0])

    def test_modern_unit_requires_a_fingerprint_and_exact_lowercase_16_digit_hash(self):
        self.modern()
        for unit_hash in ("abc", "0123456789abcde", "0123456789abcdef0", "ABCDEF0123456789"):
            with self.subTest(unit_hash=unit_hash):
                bad = self.build / "debug/build/invalid" / unit_hash / "fingerprint"
                bad.mkdir(parents=True)
                self.assert_unknown(self.directories.profile("debug"))
                bad.rmdir()
                bad.parent.rmdir()
                bad.parent.parent.rmdir()
        self.write(self.build / "debug/build/unfingerprinted/fedcba9876543210/out/loose.rlib")
        self.assert_unknown(self.directories.profile("debug"))

    def test_legacy_out_dir_cannot_supply_a_modern_fingerprint_or_cargo_record(self):
        _, _, dependencies, output = self.legacy(unit_hash="0123456789abcdef")
        out = output.parent / "out"
        self.write(out / "fingerprint/generated")
        self.write(out / "0123456789abcdef/fingerprint/lib-fake")
        self.write(out / "run/stdout")
        self.write(out / "output")
        self.write(out / "generated.d")
        profiles = self.directories.profiles()
        self.assertEqual(len(profiles), 1)
        self.assertEqual(profiles[0].layout, "legacy")
        self.assertEqual(profiles[0].dependency_lists, (dependencies,))
        self.assertEqual(profiles[0].build_script_outputs, (output,))

    def test_modern_out_dir_only_contributes_immediate_dependency_lists(self):
        fingerprint, _, dependencies, output = self.modern()
        out = fingerprint.parent / "out"
        extra = self.write(out / "generated.d")
        self.write(out / "fingerprint/lib-fake")
        self.write(out / "output")
        self.write(out / "build/fake/0123456789abcdef/run/stdout")
        self.write(out / "nested/ignored.d")
        profiles = self.directories.profiles()
        self.assertEqual(len(profiles), 1)
        self.assertEqual(profiles[0].dependency_lists, tuple(sorted((dependencies, extra))))
        self.assertEqual(profiles[0].build_script_outputs, (output,))
        self.assertEqual(len(profiles[0].units), 1)

    def test_misleading_legacy_out_dir_without_a_real_fingerprint_is_unknown(self):
        self.write(self.build / "debug/build/older-0123456789abcdef/out/fingerprint/generated")
        self.assert_unknown(self.directories.profile("debug"))

    def test_symlinked_structural_directories_and_records_are_unknown(self):
        for relative in (".fingerprint", ".fingerprint/some-crate-0a1b", "deps", "deps/some_crate-0a1b.d",
                         "build", "build/some-crate-0a1b", "build/some-crate-0a1b/output",
                         "build/crate", "build/crate/0123456789abcdef",
                         "build/crate/0123456789abcdef/fingerprint",
                         "build/crate/0123456789abcdef/out",
                         "build/crate/0123456789abcdef/run",
                         "build/crate/0123456789abcdef/run/stdout",
                         "build/crate/0123456789abcdef/out/linked.d"):
            with self.subTest(relative=relative):
                profile_name = "case-" + relative.replace("/", "-")
                if relative.startswith("build/crate"):
                    self.modern(package="crate", profile=profile_name)
                else:
                    self.legacy(profile=profile_name)
                self.assertTrue(self.directories.profile(profile_name).known_layout)
                link = self.build / profile_name / relative
                # Existing fixture paths at .fingerprint/deps/build are moved
                # within this private tree so the link can replace them.
                if link.exists():
                    destination = self.root / (profile_name + "-real")
                    link.rename(destination)
                else:
                    destination = self.root / "absent"
                link.parent.mkdir(parents=True, exist_ok=True)
                link.symlink_to(destination)
                self.assert_unknown(self.directories.profile(profile_name))

    def test_symlinked_roots_and_ancestors_are_never_resolved(self):
        self.modern()
        for relative in ("linked-root", "linked-ancestor/build"):
            with self.subTest(relative=relative):
                if relative == "linked-root":
                    linked_build = self.root / relative
                    linked_build.symlink_to(self.build, target_is_directory=True)
                else:
                    ancestor = self.root / "linked-ancestor"
                    ancestor.symlink_to(self.root, target_is_directory=True)
                    linked_build = ancestor / self.build.name
                directories = CargoDirectories(self.target, linked_build)
                self.assert_unknown(directories.profile("debug"))
                self.assertEqual(len(directories.profiles()), 1)
                self.assert_unknown(directories.profiles()[0])

    def test_symlinked_public_dependency_list_disables_shortcuts(self):
        self.modern()
        self.target.joinpath("debug").mkdir(parents=True)
        (self.target / "debug/cagents.d").symlink_to(self.root / "absent")
        self.assert_unknown(self.directories.profile("debug"))

    def test_scan_failure_disables_shortcuts_without_a_partial_result(self):
        self.modern()
        original = Path.iterdir

        def unreadable(path):
            if path.name == "out":
                raise PermissionError("synthetic unreadable output")
            return original(path)

        with patch.object(Path, "iterdir", unreadable):
            self.assert_unknown(self.directories.profile("debug"))

    def test_inventory_never_writes_or_deletes_fixture_entries(self):
        self.legacy()
        self.modern()

        def snapshot():
            return {path.relative_to(self.root): (path.lstat().st_mode, path.lstat().st_mtime_ns,
                                                 path.read_bytes() if path.is_file() else None)
                    for path in self.root.rglob("*")}

        before = snapshot()
        profile = self.directories.profile("debug")
        self.assertTrue(profile.known_layout)
        self.assertEqual(self.directories.profiles(), (profile,))
        self.assertEqual(snapshot(), before)


if __name__ == "__main__":
    unittest.main()
