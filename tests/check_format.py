#!/usr/bin/env python3
"""Check routing and rustfmt's include! gap without Cargo/application builds.

CHECK_FORMAT_CARGO_FIXTURE=1 also exercises Cargo in a dependency-free fixture.
"""

import json
import importlib.util
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts/check-format"
CACHE_HELPER = ROOT / "scripts/check-format-cache.py"
INCLUDED = (
    "src/modules.rs",
    "src/app/repository_clone_lsp_ownership_tests.rs",
    "src/control/handshake_deadline_tests.rs",
    "src/terminal/worker/bounded_input_tests.rs",
    "src/host/model/views/placement_tests.rs",
    "tests/support/iced_tree.rs",
    "tests/support/repository_scale.rs",
)


class FormatCheck(unittest.TestCase):
    def setUp(self):
        # Keep inherited build-lane overrides out of temporary formatter fixtures.
        environment = mock.patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop("CARGO_TARGET_DIR", None)
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-format-check-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write("scripts/check-format", CHECKER.read_text())
        self.write("scripts/check-format-cache.py", CACHE_HELPER.read_text())
        self.script = self.root / "scripts/check-format"
        self.script.chmod(0o755)

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run_checker(self, env=None):
        # Calling from outside the fixture verifies checkout-relative resolution.
        return subprocess.run(
            [str(self.script)], cwd=self.root.parent, env=env,
            capture_output=True, text=True, timeout=30,
        )

    def mock_tools(self, cargo_status=0, rustfmt_status=0):
        log = self.root / "calls.jsonl"
        for tool, status in (("cargo", cargo_status), ("rustfmt", rustfmt_status)):
            self.write(f"tools/{tool}", f"""#!{shutil.which('python3')}
import json
from pathlib import Path
import sys
with Path({str(log)!r}).open("a") as output:
    output.write(json.dumps([Path.cwd().as_posix(), Path(sys.argv[0]).name, *sys.argv[1:]]) + "\\n")
print({tool + ' diagnostic'!r}, file=sys.stderr)
sys.exit({status})
""")
            (self.root / "tools" / tool).chmod(0o755)
        env = os.environ | {"PATH": str(self.root / "tools") + os.pathsep + os.environ["PATH"]}
        return log, env

    def cache_fixture(self):
        for name in ("src", "tests", "crates"):
            (self.root / name).mkdir()
        for relative in INCLUDED:
            self.write(relative, "fn clean() {}\n")
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(
            ["git", "-C", str(self.root), "-c", "user.name=Fixture",
             "-c", "user.email=fixture@example.invalid", "commit",
             "--allow-empty", "-qm", "base"], check=True,
        )
        spec = importlib.util.spec_from_file_location(
            "format_cache_fixture", self.root / "scripts/check-format-cache.py",
        )
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        real_add_directory = helper.add_directory

        def fixture_directory(hasher, path):
            if path == self.root or self.root in path.parents:
                return real_add_directory(hasher, path)
            # Other workers constantly change /tmp and other shared ancestors.
            # Hold that unrelated input constant in these fixtures; directories
            # and file metadata inside this fixture retain the real ABA guard.
            if not stat.S_ISDIR(path.lstat().st_mode):
                raise helper.Uncacheable("special fixture ancestor")
            helper.add(hasher, str(path))
            helper.add(hasher, "fixed-external-fixture-directory")

        directory_patch = mock.patch.object(helper, "add_directory", side_effect=fixture_directory)
        directory_patch.start()
        self.addCleanup(directory_patch.stop)
        return helper, ["--edition", "2024", "--check", *INCLUDED]

    def test_checks_both_routes_and_preserves_failures_and_diagnostics(self):
        for cargo_status, rustfmt_status in ((0, 0), (0, 1), (7, 0), (7, 2)):
            with self.subTest(cargo=cargo_status, rustfmt=rustfmt_status):
                log, env = self.mock_tools(cargo_status, rustfmt_status)
                log.unlink(missing_ok=True)
                result = self.run_checker(env)
                self.assertEqual(result.returncode, cargo_status or rustfmt_status)
                self.assertEqual(
                    [json.loads(line) for line in log.read_text().splitlines()],
                    [
                        [str(self.root), "cargo", "fmt", "--check"],
                        [str(self.root), "rustfmt", "--edition", "2024", "--check", *INCLUDED],
                    ],
                )
                self.assertIn("cargo diagnostic", result.stderr)
                self.assertIn("rustfmt diagnostic", result.stderr)

    def test_absolute_cargo_target_keeps_formatter_cache_out_of_checkout(self):
        target = tempfile.TemporaryDirectory(prefix="cagents-format-target-")
        self.addCleanup(target.cleanup)
        _, env = self.mock_tools()
        env["CARGO_TARGET_DIR"] = target.name
        result = self.run_checker(env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((Path(target.name) / ".check-format-cache").is_dir())
        self.assertFalse((self.root / "target").exists())

    def test_relative_cargo_target_uses_checker_checkout_directory(self):
        target = tempfile.TemporaryDirectory(prefix="cagents-format-target-")
        self.addCleanup(target.cleanup)
        _, env = self.mock_tools()
        env["CARGO_TARGET_DIR"] = os.path.relpath(target.name, self.root)
        result = self.run_checker(env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((Path(target.name) / ".check-format-cache").is_dir())
        self.assertFalse((self.root / "target").exists())

    def test_unusable_cargo_target_preserves_formatter_failure_without_local_cache(self):
        occupied = self.root / "occupied-target"
        occupied.write_text("preserve this file")
        _, env = self.mock_tools(rustfmt_status=7)
        env["CARGO_TARGET_DIR"] = str(occupied)
        result = self.run_checker(env)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("rustfmt diagnostic", result.stderr)
        self.assertEqual(occupied.read_text(), "preserve this file")
        self.assertFalse((self.root / "target").exists())

    def test_empty_cargo_target_retains_default_formatter_cache(self):
        _, env = self.mock_tools()
        env["CARGO_TARGET_DIR"] = ""
        result = self.run_checker(env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "target" / ".check-format-cache").is_dir())

    def test_missing_byte_environment_preserves_ordinary_formatter_status(self):
        helper, args = self.cache_fixture()
        with mock.patch.object(helper.os, "environb", create=True):
            del helper.os.environb
            with mock.patch.object(helper, "run", return_value=7) as run:
                self.assertEqual(helper.main(args), 7)
                self.assertEqual(helper.main(args), 7)
                self.assertEqual(run.call_count, 2)

    def test_success_cache_reuses_only_identical_sources_and_environment(self):
        helper, args = self.cache_fixture()
        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)
            for relative, text in (
                ("src/modules.rs", "fn edited() {}\n"),
                ("src/added.rs", "fn added() {}\n"),
                ("rustfmt.toml", "max_width = 90\n"),
            ):
                self.write(relative, text)
                self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 4)
            (self.root / "src/added.rs").unlink()
            self.assertEqual(helper.main(args), 0)
            with mock.patch.dict(os.environ, {"FORMAT_ENV_PROBE": "changed"}):
                self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 6)

    def test_failure_and_mid_check_source_change_never_cache_success(self):
        helper, args = self.cache_fixture()
        with mock.patch.object(helper, "run", return_value=1) as run:
            self.assertEqual(helper.main(args), 1)
            self.assertEqual(helper.main(args), 1)
            self.assertEqual(run.call_count, 2)

        def mutate(_command):
            with (self.root / "src/modules.rs").open("a") as output:
                output.write("fn changed_during_check() {}\n")
            return 0

        with mock.patch.object(helper, "run", side_effect=mutate) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)
        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)

    def test_external_module_path_and_symlink_disable_cache(self):
        helper, args = self.cache_fixture()
        self.write("src/modules.rs", '#[path = "../outside.rs"]\nmod outside;\n')
        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 2)
            self.write("src/modules.rs", "fn clean() {}\n")
            (self.root / "src/linked.rs").symlink_to(self.root / "tests/support/iced_tree.rs")
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 4)

    def test_edit_and_restore_during_check_does_not_cache_success(self):
        helper, args = self.cache_fixture()
        path = self.root / "src/modules.rs"
        original = path.read_bytes()

        def mutate_and_restore(_command):
            path.write_bytes(b"fn temporary_source() {}\n")
            path.write_bytes(original)
            return 0

        with mock.patch.object(helper, "run", side_effect=mutate_and_restore) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)
        self.assertFalse(any(helper.CACHE.iterdir()))
        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)

    def test_past_edit_restore_and_directory_churn_reuse_known_good_content(self):
        helper, args = self.cache_fixture()
        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            path = self.root / "src/modules.rs"
            original = path.read_bytes()
            path.write_bytes(b"fn temporary_source() {}\n")
            path.write_bytes(original)
            for relative in ("src/transient.rs", "rustfmt.toml"):
                transient = self.root / relative
                transient.write_text("past temporary input\n")
                transient.unlink()
            self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 1)

    def test_transient_source_and_config_files_do_not_cache_success(self):
        helper, args = self.cache_fixture()
        for relative in ("src/transient.rs", "rustfmt.toml"):
            with self.subTest(relative=relative):
                path = self.root / relative

                def create_and_remove(_command):
                    path.write_text("temporary input\n")
                    path.unlink()
                    return 0

                with mock.patch.object(helper, "run", side_effect=create_and_remove):
                    self.assertEqual(helper.main(args), 0)
                self.assertFalse(any(helper.CACHE.iterdir()))

    def test_cache_hit_rechecks_the_current_metadata_guard(self):
        helper, args = self.cache_fixture()
        path = self.root / "src/modules.rs"
        original = path.read_bytes()
        real_cached = helper.cached

        def mutate_after_lookup(key):
            found = real_cached(key)
            path.write_bytes(b"fn temporary_source() {}\n")
            path.write_bytes(original)
            return found

        with mock.patch.object(helper, "run", return_value=0) as run:
            self.assertEqual(helper.main(args), 0)
            with mock.patch.object(helper, "cached", side_effect=mutate_after_lookup):
                self.assertEqual(helper.main(args), 0)
            self.assertEqual(run.call_count, 2)

    def test_commented_external_paths_are_checked_after_external_file_changes(self):
        helper, args = self.cache_fixture()
        self.write("outside.rs", "fn clean() {}\n")
        forms = (
            '#[path /* comment */ = "../outside.rs"]',
            '#/* comment */[path = "../outside.rs"]',
            '#[/* ] */ path = "../outside.rs"]',
            '#[cfg_attr(any(), path /* comment */ = "../outside.rs")]',
            '#[r#path = "../outside.rs"]',
            '#[r#cfg_attr(all(), path = "../outside.rs")]',
            '#[unsafe(path = "../outside.rs")]',
            '#[r#unsafe(path = "../outside.rs")]',
            '#[arbitrary_attr(path = "../outside.rs")]',
            '#[path = r###"../outside.rs"###]',
            """#[cfg_attr(all(), doc = '"', path = "../outside.rs")]""",
        )
        for attribute in forms:
            with self.subTest(attribute=attribute):
                self.write("src/modules.rs", attribute + "\nmod outside;\n")
                with mock.patch.object(helper, "run", side_effect=(0, 1)) as run:
                    self.assertEqual(helper.main(args), 0)
                    self.write("outside.rs", "fn   malformed( ) { }\n")
                    self.assertEqual(helper.main(args), 1)
                    self.assertEqual(run.call_count, 2)
                self.write("outside.rs", "fn clean() {}\n")

    @unittest.skipUnless(shutil.which("rustfmt"), "needs rustfmt")
    def test_real_rustfmt_rechecks_commented_external_module(self):
        helper, args = self.cache_fixture()
        self.write("src/outside.rs", "fn local_module() {}\n")
        def checked(command):
            return subprocess.run(
                ["rustfmt", *command], cwd=self.root, capture_output=True, timeout=10,
            ).returncode

        for attribute in (
            '#[path /* comment */ = "../outside.rs"]',
            '#[unsafe(path = "../outside.rs")]',
            '#[arbitrary_attr(path = "../outside.rs")]',
            '#[doc(path = "../outside.rs")]',
            '#[cfg(path = "../outside.rs")]',
            '#[allow(path = "../outside.rs")]',
            """#[cfg_attr(all(), doc = '"', path = "../outside.rs")]""",
            '#[path\u0085= "../outside.rs"]',
            '#\u200e[path = "../outside.rs"]',
        ):
            with self.subTest(attribute=attribute):
                self.write("src/modules.rs", attribute + "\nmod outside;\n")
                self.write("outside.rs", "fn clean() {}\n")
                # Rustfmt normalizes Unicode spacing even with a clean child;
                # the helper must preserve that direct-check result as well.
                expected = checked(args)
                if attribute.isascii():
                    self.assertEqual(expected, 0)
                with mock.patch.object(helper, "run", side_effect=checked) as run:
                    self.assertEqual(helper.main(args), expected)
                    self.write("outside.rs", "fn   malformed( ) { }\n")
                    self.assertEqual(helper.main(args), 1)
                    self.assertEqual(run.call_count, 2)

    def test_non_rust_module_targets_cannot_hide_an_external_path_chain(self):
        helper, args = self.cache_fixture()
        self.write("src/modules.rs", '#[path = "child.txt"]\nmod child;\n')
        self.write("src/child.txt", '#[path = "../outside.rs"]\nmod outside;\n')
        self.write("outside.rs", "fn clean() {}\n")
        with mock.patch.object(helper, "run", side_effect=(0, 1)) as run:
            self.assertEqual(helper.main(args), 0)
            self.write("outside.rs", "fn   malformed( ) { }\n")
            self.assertEqual(helper.main(args), 1)
            self.assertEqual(run.call_count, 2)

    def test_xdg_config_contents_invalidate_without_environment_change(self):
        helper, args = self.cache_fixture()
        xdg = self.root / "external-config"
        with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(xdg)}):
            with mock.patch.object(helper, "run", return_value=0) as run:
                self.assertEqual(helper.main(args), 0)
                self.write("external-config/rustfmt/rustfmt.toml", "max_width = 42\n")
                self.assertEqual(helper.main(args), 0)
                self.write("external-config/rustfmt/rustfmt.toml", "max_width = 43\n")
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(run.call_count, 3)

    def test_rustup_proxy_key_tracks_selected_binary_contents(self):
        helper, _args = self.cache_fixture()
        proxy = self.root / "fake/rustup"
        actual = self.root / "fake/toolchain/rustfmt"
        self.write("fake/rustup", "\x7fELFproxy")
        real_which = shutil.which

        def which(name):
            if name == "rustup":
                return str(proxy)
            return real_which(name)

        real_check_output = subprocess.check_output

        def check_output(command, **kwargs):
            if command[:2] == [str(proxy), "which"]:
                return (str(actual) + "\n").encode()
            return real_check_output(command, **kwargs)

        for kind in ("symlink", "hardlink"):
            with self.subTest(kind=kind):
                link = self.root / f"fake/rustfmt-{kind}"
                if kind == "symlink":
                    link.symlink_to(proxy)
                else:
                    os.link(proxy, link)
                self.write("fake/toolchain/rustfmt", "\x7fELFfirst formatter")
                with mock.patch.object(helper.shutil, "which", side_effect=which), \
                        mock.patch.object(helper.subprocess, "check_output", side_effect=check_output) as lookup, \
                        mock.patch.object(helper, "formatter_libraries"):
                    first = helper.InputHashes()
                    helper.rustfmt_binary(first, str(link))
                    self.write("fake/toolchain/rustfmt", "\x7fELFsecond formatter")
                    second = helper.InputHashes()
                    helper.rustfmt_binary(second, str(link))
                self.assertNotEqual(first.hexdigest(), second.hexdigest())
                self.assertEqual(lookup.call_count, 2)

    def test_unverified_hardlinked_formatter_disables_cache(self):
        helper, _args = self.cache_fixture()
        proxy = self.root / "fake/rustup"
        formatter = self.root / "fake/rustfmt"
        other = self.root / "fake/other-rustup"
        self.write("fake/rustup", "\x7fELFproxy")
        self.write("fake/other-rustup", "\x7fELFother")
        os.link(proxy, formatter)
        for discovered in (None, str(other)):
            with self.subTest(discovered=discovered):
                with mock.patch.object(helper.shutil, "which", return_value=discovered), \
                        mock.patch.object(helper.subprocess, "check_output") as lookup:
                    with self.assertRaises(helper.Uncacheable):
                        helper.rustfmt_binary(helper.InputHashes(), str(formatter))
                lookup.assert_not_called()
        with mock.patch.object(helper.shutil, "which", return_value=str(proxy)), \
                mock.patch.object(helper.subprocess, "check_output", return_value=(str(formatter) + "\n").encode()):
            with self.assertRaises(helper.Uncacheable):
                helper.rustfmt_binary(helper.InputHashes(), str(formatter))

    def test_formatter_library_contents_and_unverified_graphs(self):
        helper, _args = self.cache_fixture()
        library = self.root / "fake/libformatter.so"
        self.write("fake/libformatter.so", "first library")
        output = (f"\tlinux-vdso.so.1 (0x1234)\n"
                  f"\tlibformatter.so => {library} (0x1234)\n").encode()
        traced = subprocess.CompletedProcess([], 0, stdout=output, stderr=b"")
        with mock.patch.object(helper.subprocess, "run", return_value=traced):
            first = helper.InputHashes()
            helper.formatter_libraries(first, Path("/verified/rustfmt"))
            self.write("fake/libformatter.so", "second library")
            second = helper.InputHashes()
            helper.formatter_libraries(second, Path("/verified/rustfmt"))
        self.assertNotEqual(first.hexdigest(), second.hexdigest())
        for stdout, stderr in ((b"libformatter.so => not found\n", b""), (output, b"warning")):
            with self.subTest(stdout=stdout, stderr=stderr):
                traced = subprocess.CompletedProcess([], 0, stdout=stdout, stderr=stderr)
                with mock.patch.object(helper.subprocess, "run", return_value=traced):
                    with self.assertRaises(helper.Uncacheable):
                        helper.formatter_libraries(helper.InputHashes(), Path("/verified/rustfmt"))

    def test_external_license_templates_and_missing_toml_parser_disable_cache(self):
        helper, args = self.cache_fixture()
        for key in ("license_template_path", '"license_\\u0074emplate_path"'):
            with self.subTest(key=key):
                self.write("rustfmt.toml", f'{key} = "../external-license.txt"\n')
                with mock.patch.object(helper, "run", return_value=0) as run:
                    self.assertEqual(helper.main(args), 0)
                    self.assertEqual(helper.main(args), 0)
                    self.assertEqual(run.call_count, 2)
        self.write("rustfmt.toml", "max_width = 90\n")
        with mock.patch.object(helper, "tomllib", None):
            with mock.patch.object(helper, "run", return_value=0) as run:
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(run.call_count, 2)

    def test_nested_source_config_cannot_hide_an_external_license_template(self):
        helper, args = self.cache_fixture()
        self.write("tests/support/rustfmt.toml", 'license_template_path = "../../outside.txt"\n')
        self.write("outside.txt", "first template")
        with mock.patch.object(helper, "run", side_effect=(0, 1)) as run:
            self.assertEqual(helper.main(args), 0)
            self.write("outside.txt", "changed template")
            self.assertEqual(helper.main(args), 1)
            self.assertEqual(run.call_count, 2)

    def test_linked_configuration_parents_disable_cache(self):
        helper, args = self.cache_fixture()
        self.write("config-target/rustfmt/rustfmt.toml", "max_width = 90\n")
        (self.root / "config-link").symlink_to(self.root / "config-target", target_is_directory=True)
        with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.root / "config-link")}):
            with mock.patch.object(helper, "run", return_value=0) as run:
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(helper.main(args), 0)
                self.assertEqual(run.call_count, 2)

    @unittest.skipUnless(shutil.which("rustfmt"), "needs rustfmt")
    def test_real_rustfmt_traversal_and_nested_fragments_without_rewrites(self):
        # Model Cargo's root rustfmt call, without executing Cargo or metadata.
        self.write("tools/cargo", '#!/bin/sh\nexec rustfmt --edition 2024 --check src/lib.rs\n')
        (self.root / "tools/cargo").chmod(0o755)
        env = os.environ | {"PATH": str(self.root / "tools") + os.pathsep + os.environ["PATH"]}
        self.exercise_included_sources(env)

    @unittest.skipUnless(
        os.environ.get("CHECK_FORMAT_CARGO_FIXTURE") == "1"
        and shutil.which("cargo") and shutil.which("rustfmt"),
        "opt in with CHECK_FORMAT_CARGO_FIXTURE=1 for the tiny Cargo fixture",
    )
    def test_actual_cargo_root_omission_and_explicit_include_checks(self):
        self.exercise_included_sources(os.environ)

    def exercise_included_sources(self, env):
        self.write("Cargo.toml", """[workspace]
[package]
name = "format-fixture"
version = "0.0.0"
edition = "2024"
""")
        self.write("src/lib.rs", 'include!("modules.rs");\n')
        self.write("src/modules.rs", '#[cfg(feature = "ui")]\nmod gated;\n')
        for relative in INCLUDED[1:]:
            self.write(relative, "fn included_fragment() {}\n")
        # Include fragments remain invisible even when encountered in a module
        # that rustfmt can traverse; direct checking must cover every one.
        self.write("src/gated.rs", """pub fn gated() {}
include!("terminal/worker/bounded_input_tests.rs");
include!("host/model/views/placement_tests.rs");
include!("../tests/support/iced_tree.rs");
include!("../tests/support/repository_scale.rs");
""")
        clean = self.run_checker(env)
        self.assertEqual(clean.returncode, 0, clean.stdout + clean.stderr)
        for relative in ("src/gated.rs", *INCLUDED[1:]):
            with self.subTest(source=relative):
                path = self.root / relative
                original = path.read_text()
                path.write_text(original + "fn   malformed( ) { }\n")
                before = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
                cargo = subprocess.run(
                    ["cargo", "fmt", "--check"], cwd=self.root, env=env,
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(cargo.returncode, 0, cargo.stdout + cargo.stderr)
                checked = self.run_checker(env)
                self.assertNotEqual(checked.returncode, 0)
                self.assertIn(path.name, checked.stdout + checked.stderr)
                self.assertEqual(before, {path: path.read_bytes() for path in before})
                path.write_text(original)
                repaired = self.run_checker(env)
                self.assertEqual(repaired.returncode, 0, repaired.stdout + repaired.stderr)


if __name__ == "__main__":
    unittest.main()
