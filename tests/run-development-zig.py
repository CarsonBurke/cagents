#!/usr/bin/env python3
"""Exercise Zig discovery before any Cargo build, without building the app."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "scripts/run-development"


class ZigDiscovery(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-zig-runner-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        checkout = self.root / "checkout"
        (checkout / "scripts").mkdir(parents=True)
        self.runner = checkout / "scripts/run-development"
        shutil.copy2(SOURCE, self.runner)
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')

        self.data = self.root / "data with spaces/mise"
        shim = self.data / "shims/zig"
        real = self.data / "installs/zig/0.15.2/zig"
        shim.parent.mkdir(parents=True)
        real.parent.mkdir(parents=True)
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        self.shim_calls = self.root / "shim-calls"
        mise = fake_bin / "mise"
        mise.write_text('#!/bin/sh\nprintf "called\\n" >> "$SHIM_CALLS"\nexit 99\n')
        mise.chmod(0o755)
        shim.symlink_to(mise)
        self.real = real
        real.write_text('''#!/bin/sh
if [ "$1" != env ]; then exit 2; fi
zig_exe=${FAKE_ZIG_EXE:-$0}
version=${FAKE_ZIG_VERSION:-0.15.2}
if [ "${FAKE_ZIG_FORMAT:-zon}" = json ]; then
    printf '{\\n  "zig_exe": "%s",\\n  "version": "%s"\\n}\\n' "$zig_exe" "$version"
else
    printf '.{\\n    .zig_exe = "%s",\\n    .version = "%s",\\n}\\n' "$zig_exe" "$version"
fi
''')
        real.chmod(0o755)
        self.cargo_calls = self.root / "cargo-calls"
        cargo = fake_bin / "cargo"
        cargo.write_text('''#!/bin/sh
printf '%s|zig=%s|home=%s|xdg=%s|mise=%s\\n' "$*" "$(command -v zig)" "$HOME" "$XDG_DATA_HOME" "${MISE_DATA_DIR-unset}" >> "$CARGO_CALLS"
exit 83
''')
        cargo.chmod(0o755)
        self.home = self.root / "private-home"
        self.xdg = self.root / "private-xdg"
        self.home.mkdir()
        self.xdg.mkdir()
        self.env = os.environ.copy()
        self.env.update(
            PATH=f"{fake_bin}:{shim.parent}:/usr/bin:/bin",
            HOME=str(self.home), XDG_DATA_HOME=str(self.xdg),
            SHIM_CALLS=str(self.shim_calls), CARGO_CALLS=str(self.cargo_calls),
        )
        self.env.pop("MISE_DATA_DIR", None)
        self.env.pop("CAGENTS_ZIG", None)

    def run_runner(self, **changes):
        environment = self.env.copy()
        environment.update(changes)
        result = subprocess.run(
            [str(self.runner), "--browserless"], cwd=self.root,
            env=environment, capture_output=True, text=True, timeout=3,
        )
        calls = self.cargo_calls.read_text().splitlines() if self.cargo_calls.exists() else []
        self.assertFalse(self.shim_calls.exists(), "mise shim was executed")
        return result, calls

    def test_isolated_home_finds_pinned_zig_for_zon_and_json(self):
        for format_name in ("zon", "json"):
            with self.subTest(format=format_name):
                result, calls = self.run_runner(FAKE_ZIG_FORMAT=format_name)
                self.assertEqual(result.returncode, 83, result.stderr)
                self.assertEqual(len(calls), 1)
                self.assertIn("build --locked --profile dev-runtime --no-default-features", calls[0])
                self.assertIn(f"zig={self.real}", calls[0])
                self.assertIn(f"home={self.home}|xdg={self.xdg}|mise=unset", calls[0])
                self.cargo_calls.unlink()

    def test_explicit_paths_and_failure_before_build(self):
        result, calls = self.run_runner(MISE_DATA_DIR=str(self.data))
        self.assertEqual(result.returncode, 83, result.stderr)
        self.assertIn(f"mise={self.data}", calls[0])
        self.cargo_calls.unlink()

        result, calls = self.run_runner(CAGENTS_ZIG=str(self.real))
        self.assertEqual(result.returncode, 83, result.stderr)
        self.assertIn(f"zig={self.real}", calls[0])
        self.cargo_calls.unlink()

        for changes, expected in (
            ({"MISE_DATA_DIR": ""}, "absolute nonempty"),
            ({"MISE_DATA_DIR": str(self.root / "missing")}, "not installed"),
            ({"CAGENTS_ZIG": "zig"}, "absolute executable path"),
            ({"CAGENTS_ZIG": str(self.root / "missing")}, "not executable"),
            ({"FAKE_ZIG_VERSION": "0.14.0"}, "does not match"),
            ({"FAKE_ZIG_EXE": "relative/zig"}, "absolute zig_exe"),
            ({"FAKE_ZIG_EXE": str(self.root / "missing")}, "not runnable"),
        ):
            with self.subTest(changes=changes):
                result, calls = self.run_runner(**changes)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)
                self.assertEqual(calls, [], "Cargo started before Zig was ready")

    def test_custom_mise_data_directory_with_and_without_explicit_selection(self):
        custom = self.root / "custom data"
        self.data.rename(custom)
        self.real = custom / "installs/zig/0.15.2/zig"
        self.env["PATH"] = self.env["PATH"].replace(str(self.data / "shims"), str(custom / "shims"))
        for changes, selection in (({}, "unset"), ({"MISE_DATA_DIR": str(custom)}, str(custom))):
            with self.subTest(changes=changes):
                result, calls = self.run_runner(**changes)
                self.assertEqual(result.returncode, 83, result.stderr)
                self.assertEqual(len(calls), 1)
                self.assertIn(f"zig={self.real}", calls[0])
                self.assertIn(f"home={self.home}|xdg={self.xdg}|mise={selection}", calls[0])
                self.cargo_calls.unlink()

    def test_renamed_zig_override_fails_before_cargo_can_find_the_old_shim(self):
        renamed = self.root / "renamed tool with spaces" / "zig-0.15.2"
        renamed.parent.mkdir()
        self.real.rename(renamed)
        result, calls = self.run_runner(CAGENTS_ZIG=str(renamed))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("executable named zig", result.stderr)
        self.assertEqual(calls, [], "Cargo could resolve the original unverified shim")


if __name__ == "__main__":
    unittest.main()
