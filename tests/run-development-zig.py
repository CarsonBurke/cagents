#!/usr/bin/env python3
"""Exercise Zig discovery before any Cargo build, and the decision to skip an
up-to-date build, without building the app."""

import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "scripts/run-development"
GATE = SOURCE.with_name("cargo-gate")


class ZigDiscovery(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-zig-runner-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        checkout = self.root / "checkout"
        (checkout / "scripts").mkdir(parents=True)
        self.runner = checkout / "scripts/run-development"
        shutil.copy2(SOURCE, self.runner)
        shutil.copy2(GATE, checkout / "scripts/cargo-gate")
        shutil.copy2(GATE.with_name("cargo-gate-command.py"), checkout / "scripts/cargo-gate-command.py")
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
if [ "$1" = metadata ]; then exit 1; fi
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
            # The runner builds through the gate; keep it off the host's locks.
            CAGENTS_BUILD_LOCK=str(self.root / "cargo.lock"),
        )
        self.env.pop("CAGENTS_BUILD_SLOT_HELD", None)
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


class LaunchFreshness(unittest.TestCase):
    """A launch whose matched set is built skips the build gate; anything that
    could make Cargo rebuild sends the launch back through it."""

    def setUp(self):
        ZigDiscovery.setUp(self)
        checkout = self.runner.parents[1]
        self.source = checkout / "src/main.rs"
        self.source.parent.mkdir()
        self.source.write_text("fn main() {}\n")
        self.target = self.root / "target with spaces"
        fake_bin = self.root / "bin"
        (fake_bin / "cargo").write_text('''#!/bin/sh
case $1 in
    metadata)
        printf '{"packages":[],"target_directory":"%s","build_directory":"%s","version":1}\\n' \\
            "$FAKE_TARGET" "$FAKE_BUILD"
        exit 0 ;;
    build)
        printf '%s\\n' "$*" >> "$CARGO_CALLS"
        sleep "${FAKE_BUILD_SECONDS:-0}"
        for argument do binary=$argument; done
        if [ "$binary" = "${FAKE_FAIL_BINARY:-}" ]; then exit 101; fi
        mkdir -p "$FAKE_TARGET/dev-runtime" "$FAKE_BUILD/dev-runtime/build/script-1" \\
            "$FAKE_BUILD/dev-runtime/deps"
        printf '# env-dep:FAKE_COMPILED=\\n' > "$FAKE_BUILD/dev-runtime/deps/unit.d"
        printf '#!/bin/sh\\nexit 0\\n' > "$FAKE_TARGET/dev-runtime/$binary"
        chmod +x "$FAKE_TARGET/dev-runtime/$binary"
        printf '%s: %s %s\\n' "$FAKE_TARGET/dev-runtime/$binary" "$FAKE_SOURCE" "$FAKE_VENDORED" \\
            > "$FAKE_TARGET/dev-runtime/$binary.d"
        printf 'cargo:rerun-if-changed=%s\\ncargo:rerun-if-changed=%s\\ncargo::rerun-if-changed=build.rs\\ncargo:rerun-if-env-changed=FAKE_WATCHED\\ncargo:rerun-if-env-changed=AR_x86_64-unknown-linux-gnu\\n' \\
            "$FAKE_GIT_INDEX" "$FAKE_WATCHED_DIRECTORY" > "$FAKE_BUILD/dev-runtime/build/script-1/output"
        exit 0 ;;
esac
exit 83
''')
        rustc = fake_bin / "rustc"
        rustc.write_text('#!/bin/sh\nprintf "rustc %s\\n" "${FAKE_RUSTC:-1.93.0}"\n')
        rustc.chmod(0o755)
        # Inputs outside the checkout: a path dependency from Cargo's
        # dependency list, and Git metadata a build script watches.
        self.vendored = self.root / "shared/vendored.rs"
        self.git_index = self.root / "shared/index"
        self.vendored.parent.mkdir()
        self.vendored.write_text("")
        self.git_index.write_text("")
        self.watched_asset = self.root / "shared/assets/icons/app.svg"
        self.watched_asset.parent.mkdir(parents=True)
        self.watched_asset.write_text("")
        self.ancestor_config = self.root / ".cargo/config.toml"
        self.env.update(
            FAKE_TARGET=str(self.target), FAKE_BUILD=str(self.root / "build"),
            FAKE_SOURCE=str(self.source), FAKE_VENDORED=str(self.vendored),
            FAKE_GIT_INDEX=str(self.git_index),
            FAKE_WATCHED_DIRECTORY=str(self.root / "shared/assets"),
        )
        for name in ("FAKE_WATCHED", "FAKE_COMPILED", "FAKE_BUILD_SECONDS"):
            self.env.pop(name, None)

    def launch(self, *arguments, **changes):
        environment = self.env.copy()
        environment.update(changes)
        result = subprocess.run(
            [str(self.runner), *arguments], cwd=self.root,
            env=environment, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.cargo_calls.read_text().splitlines() if self.cargo_calls.exists() else []
        if self.cargo_calls.exists():
            self.cargo_calls.unlink()
        return result, calls

    def assert_built(self, *arguments, **changes):
        result, calls = self.launch(*arguments, **changes)
        self.assertEqual(len(calls), 3, result.stderr)
        self.assertNotIn("skipping the build", result.stderr)

    def assert_skipped(self, *arguments, **changes):
        result, calls = self.launch(*arguments, **changes)
        self.assertEqual(calls, [], "an up-to-date launch built")
        self.assertIn("skipping the build", result.stderr)

    def test_up_to_date_launch_skips_the_build(self):
        self.assert_built("--browserless")
        self.assert_skipped("--browserless")
        self.assert_skipped("--browserless")

    def test_identity_records_hide_environment_values_and_stay_private(self):
        # Build tools are inert; these are synthetic values, including an
        # arbitrary variable that Cargo asks the runner to watch.
        values = {
            "CARGO_REGISTRIES_SYNTHETIC_TOKEN": "synthetic-registry-credential-2138",
            "RUSTFLAGS": "synthetic-compiler-option-2138",
            "FAKE_WATCHED": "synthetic-watched-credential-2138",
            "FAKE_COMPILED": "synthetic-compiled-credential-2138",
        }
        previous_umask = os.umask(0)
        try:
            self.assert_built("--browserless", **values)
        finally:
            os.umask(previous_umask)
        record = self.target / "dev-runtime/.run-development-ui"
        content = record.read_text()
        for value in values.values():
            self.assertNotIn(value, content)
        self.assertRegex(content.splitlines()[0], r"^identity-sha256 [0-9a-f]{64}$")
        self.assertEqual(record.stat().st_mode & 0o777, 0o600)
        self.assert_skipped("--browserless", **values)
        for name in values:
            with self.subTest(variable=name):
                changed = {**values, name: values[name] + "-changed"}
                self.assert_built("--browserless", **changed)
                self.assert_skipped("--browserless", **changed)
                self.assert_built("--browserless", **values)
        self.assertEqual(
            sorted(path.name for path in record.parent.glob(".run-development-*")),
            [".run-development-ui"],
        )

    def test_legacy_plaintext_record_is_replaced_privately(self):
        self.assert_built("--browserless")
        record = self.target / "dev-runtime/.run-development-ui"
        record.write_text("legacy plaintext identity\n")
        record.chmod(0o644)
        self.assert_built("--browserless")
        self.assertRegex(record.read_text().splitlines()[0], r"^identity-sha256 [0-9a-f]{64}$")
        self.assertEqual(record.stat().st_mode & 0o777, 0o600)
        self.assert_skipped("--browserless")

    def test_identity_producer_and_hasher_fail_closed(self):
        self.assert_built("--browserless")
        fake_bin = self.root / "bin"
        for command in ("rustc", "sha256sum"):
            with self.subTest(command=command):
                tool = fake_bin / command
                original = tool.read_bytes() if tool.exists() else None
                tool.write_text("#!/bin/sh\nprintf 'incomplete identity\\n'\nexit 77\n")
                tool.chmod(0o755)
                try:
                    self.assert_built("--browserless")
                    self.assertFalse((self.target / "dev-runtime/.run-development-ui").exists())
                    self.assertEqual(list(self.target.glob("dev-runtime/.run-development-started*")), [])
                    self.assertEqual(list(self.target.glob("dev-runtime/.run-development-record*")), [])
                finally:
                    if original is None:
                        tool.unlink()
                    else:
                        tool.write_bytes(original)
                self.assert_built("--browserless")
                self.assert_skipped("--browserless")

    def test_changes_send_the_launch_through_the_build(self):
        self.assert_built("--browserless")
        for name, change in (
            ("source edit", lambda: self.source.write_text("fn main() { }\n")),
            ("new file", lambda: (self.source.parent / "new.rs").write_text("")),
            ("executable replaced", lambda: (self.target / "dev-runtime/cagentsd").write_text("#!/bin/sh\n")),
            ("record removed", lambda: (self.target / "dev-runtime/.run-development-ui").unlink()),
            ("dependency outside the checkout", lambda: self.vendored.write_text("//\n")),
            ("path a build script watches", lambda: self.git_index.write_text("staged")),
            ("file in a directory a build script watches", lambda: self.watched_asset.write_text("<svg/>")),
            ("dependency list removed", lambda: (self.target / "dev-runtime/cagents.d").unlink()),
            ("Cargo configuration in a parent directory", self.write_ancestor_config),
        ):
            with self.subTest(change=name):
                change()
                self.assert_built("--browserless")
                self.assert_skipped("--browserless")
        for name, changes in (
            ("toolchain", {"FAKE_RUSTC": "1.94.0"}),
            ("compiler flags", {"RUSTFLAGS": "-Ctarget-cpu=native"}),
            ("variable a build script watches", {"FAKE_WATCHED": "1"}),
            ("variable read at compile time", {"FAKE_COMPILED": "1"}),
        ):
            with self.subTest(change=name):
                self.assert_built("--browserless", **changes)
                self.assert_skipped("--browserless", **changes)
                self.assert_built("--browserless")

    def write_ancestor_config(self):
        self.ancestor_config.parent.mkdir(exist_ok=True)
        self.ancestor_config.write_text("[build]\n")

    def test_overlapping_launches_record_their_own_start(self):
        """An edit made while one launch builds is not hidden by a second
        launch that started later."""
        self.assert_built("--browserless")
        self.source.write_text("fn main() { first }\n")
        slow = dict(self.env, FAKE_BUILD_SECONDS="1")
        first = subprocess.Popen(
            [str(self.runner), "--browserless"], cwd=self.root, env=slow,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        deadline = time.monotonic() + 5
        while not self.cargo_calls.exists():
            self.assertLess(time.monotonic(), deadline, "the first launch never built")
            time.sleep(0.02)
        self.source.write_text("fn main() { edited }\n")
        second = subprocess.Popen(
            [str(self.runner), "--browserless"], cwd=self.root, env=slow,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        self.assertEqual(first.wait(timeout=10), 0, first.stderr.read())
        third = subprocess.run(
            [str(self.runner), "--browserless"], cwd=self.root, env=self.env,
            capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(third.returncode, 0, third.stderr)
        self.assertNotIn("skipping the build", third.stderr,
                         "a launch skipped the build after an edit made during one")
        self.assertEqual(second.wait(timeout=15), 0, second.stderr.read())
        self.cargo_calls.unlink()
        self.assert_skipped("--browserless")

    def test_a_failed_build_leaves_no_record(self):
        self.assert_built("--browserless")
        self.source.write_text("fn main() { broken }\n")
        result = subprocess.run(
            [str(self.runner), "--browserless"], cwd=self.root,
            env=dict(self.env, FAKE_FAIL_BINARY="cagents-ui"),
            capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 101, result.stderr)
        self.cargo_calls.unlink()
        self.assert_built("--browserless")
        leftovers = [path.name for path in (self.target / "dev-runtime").iterdir()
                     if path.name.startswith((".run-development-started", ".run-development-record"))]
        self.assertEqual(leftovers, [])

    def test_nested_only_refuses_the_callers_desktop_before_building(self):
        session_runtime = self.root / "nh-fixture"
        session_runtime.mkdir()
        # What niri-harness exec leaves: its runtime and no X11 or inherited
        # Wayland socket.
        for name in ("DISPLAY", "WAYLAND_SOCKET"):
            self.env.pop(name, None)
        guarded = dict(CAGENTS_DEVELOPMENT_NESTED_ONLY="1", XDG_RUNTIME_DIR=str(session_runtime))
        for name, changes in (
            ("caller's runtime", {"XDG_RUNTIME_DIR": "/run/user/1000"}),
            ("caller's X display", {"DISPLAY": ":0"}),
            ("inherited Wayland socket", {"WAYLAND_SOCKET": "3"}),
        ):
            with self.subTest(desktop=name):
                result = subprocess.run(
                    [str(self.runner), "--browserless"], cwd=self.root,
                    env={**self.env, **guarded, **changes},
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("not a niri-harness session", result.stderr)
                self.assertFalse(self.cargo_calls.exists(), "the refused launch built")
        self.assert_built("--browserless", **guarded)

    def test_a_terminated_launch_leaves_no_stamps(self):
        launch = subprocess.Popen(
            [str(self.runner), "--browserless"], cwd=self.root,
            env=dict(self.env, FAKE_BUILD_SECONDS="1"),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 5
        while not self.cargo_calls.exists():
            self.assertLess(time.monotonic(), deadline, "the launch never built")
            time.sleep(0.02)
        # The runner alone, as a supervisor stopping it would.
        launch.send_signal(signal.SIGTERM)
        self.assertEqual(launch.wait(timeout=10), 143)
        leftovers = [path.name for path in (self.target / "dev-runtime").iterdir()
                     if path.name.startswith((".run-development-started", ".run-development-record"))]
        self.assertEqual(leftovers, [])

    def test_the_other_lane_rebuilds_and_invalidates_this_one(self):
        self.assert_built("--browserless")
        self.assert_built()
        self.assert_skipped()
        self.assert_built("--browserless")


if __name__ == "__main__":
    unittest.main()
