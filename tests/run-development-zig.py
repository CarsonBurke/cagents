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
            CAGENTS_BUILD_INSTALLED_GATE="", CAGENTS_BUILD_REMOTE="local",
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

    def test_in_session_rejects_lifecycle_options_and_the_login_desktop(self):
        for flags in ([], ["--keep"], ["--fresh-daemon"], ["--ttl", "60"],
                      ["--stop-instance", "review2571"]):
            with self.subTest(flags=flags):
                result = subprocess.run(
                    [str(self.runner), "--in-session", "--instance", "review2571", *flags],
                    env=(self.env if not flags else self.env | {
                        "XDG_RUNTIME_DIR": str(self.root / "nh-evidence"),
                        "DISPLAY": "", "NIRI_SOCKET": "private-niri.sock",
                    }), capture_output=True, text=True, timeout=3,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(self.cargo_calls.exists())

    def test_isolated_home_finds_pinned_zig_for_zon_and_json(self):
        for format_name in ("zon", "json"):
            with self.subTest(format=format_name):
                result, calls = self.run_runner(FAKE_ZIG_FORMAT=format_name)
                self.assertEqual(result.returncode, 83, result.stderr)
                self.assertEqual(len(calls), 1)
                self.assertIn("build --locked --profile dev-runtime --no-default-features", calls[0])
                self.assertIn(f"zig={self.real}", calls[0])
                self.assertIn(f"home={self.home}|xdg={self.xdg}|mise={self.data}", calls[0])
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


@unittest.skipUnless(shutil.which("cargo"), "needs real Cargo")
class RefuseCompile(unittest.TestCase):
    """The wrapper against real Cargo: a current build passes and stays fresh, and
    anything to compile fails naming the crate."""

    def cargo(self, *arguments, **changes):
        return subprocess.run(
            ["cargo", "build", "--quiet", *arguments], cwd=self.crate,
            env=os.environ | changes, capture_output=True, text=True, timeout=120,
        )

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-refuse-compile-")
        self.addCleanup(temporary.cleanup)
        self.crate = Path(temporary.name)
        (self.crate / "src").mkdir()
        (self.crate / "Cargo.toml").write_text(
            '[package]\nname = "probe"\nversion = "0.0.0"\nedition = "2021"\n')
        (self.crate / "src/main.rs").write_text("fn main() {}\n")
        self.wrapper = {"RUSTC_WRAPPER": str(SOURCE.with_name("refuse-compile")),
                        "CAGENTS_NATIVE_LOCK_HELD": "/the/lock"}

    def test_a_current_build_passes_and_a_stale_one_is_refused(self):
        result = self.cargo()
        self.assertEqual(result.returncode, 0, result.stderr)
        binary = self.crate / "target/debug/probe"
        built = binary.stat().st_mtime_ns

        result = self.cargo(**self.wrapper)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(binary.stat().st_mtime_ns, built, "the wrapper made Cargo rebuild")

        (self.crate / "src/main.rs").write_text("fn main() { println!(); }\n")
        result = self.cargo(**self.wrapper)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to compile probe while holding the native lock /the/lock", result.stderr)
        self.assertEqual(binary.stat().st_mtime_ns, built)

    def test_the_wrapper_answers_questions_about_rustc_and_refuses_the_rest(self):
        wrapper = self.wrapper["RUSTC_WRAPPER"]
        asked = subprocess.run([wrapper, "printf", "-vV"], capture_output=True, text=True)
        self.assertEqual((asked.returncode, asked.stdout), (0, "-vV"))
        refused = subprocess.run(
            [wrapper, "false", "--crate-name", "probe", "src/main.rs"], capture_output=True, text=True,
        )
        self.assertEqual(refused.returncode, 78)
        self.assertIn("refusing to compile probe", refused.stderr)


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

    def under_native_lock(self, *holder, **changes):
        """Launch below `holder`, which holds the native lock in a directory with a space."""
        lock = self.root / "native lock"
        lock.touch()
        environment = self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock)} | changes
        return subprocess.run(
            [*holder, str(lock), str(self.runner), "--browserless"], cwd=self.root,
            env=environment, capture_output=True, text=True, timeout=10,
        )

    def fake_cargo_reports_its_guard(self):
        cargo = self.root / "bin/cargo"
        text = cargo.read_text()
        cargo.write_text(text.replace(
            """    build)
""", """    build)
        printf 'wrapper=%s lock=%s seed=%s remote=%s slots=%s held=%s\\n' "${RUSTC_WRAPPER:-}" "${CAGENTS_BUILD_LOCK:-}" "${CAGENTS_BUILD_SEED:-}" "${CAGENTS_BUILD_REMOTE:-}" "${CAGENTS_BUILD_SLOTS:-}" "${CAGENTS_NATIVE_LOCK_HELD:-}" >> "$GUARD_CALLS"
""", 1).replace("""printf '#!/bin/sh\\nexit 0\\n' > "$FAKE_TARGET/dev-runtime/$binary\"""",
                """printf '#!/bin/sh\\n[ -z "${APP_ENV:-}" ] || env >>"$APP_ENV"\\nexit 0\\n' > "$FAKE_TARGET/dev-runtime/$binary\"""", 1))
        guard_calls = self.root / "guard-calls"
        self.env["GUARD_CALLS"] = str(guard_calls)
        return guard_calls

    def assert_compiling_is_forbidden(self, result, guard_calls):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("may only confirm that nothing needs compiling", result.stderr)
        lines = guard_calls.read_text().splitlines()
        self.assertEqual(len(lines), 3)
        for line in lines:
            self.assertIn(f"wrapper={self.runner.with_name('refuse-compile')} ", line)
            self.assertNotIn(f"lock={self.env['CAGENTS_BUILD_LOCK']} ", line)
            self.assertIn(f"lock={self.target}/dev-runtime/.native-lock-gate ", line)
            self.assertIn("seed=0 remote=local slots=1 held=", line)

    def test_named_instance_runs_in_the_current_session_without_a_compositor(self):
        self.fake_cargo_reports_its_guard()
        app_environment = self.root / "app-environment"
        harness_calls = self.root / "harness-calls"
        harness = self.root / "bin/niri-harness"
        harness.write_text(f'#!/bin/sh\nprintf called >> "{harness_calls}"\nexit 99\n')
        harness.chmod(0o755)
        log = self.root / "ui.log"
        self.launch("--browserless", "--instance", "review2571", "--in-session",
                    "--log-file", str(log),
                    XDG_RUNTIME_DIR=str(self.root / "nh-evidence"),
                    DISPLAY="", NIRI_SOCKET="private-niri.sock", APP_ENV=str(app_environment))
        self.assertIn("CAGENTS_DEVELOPMENT_INSTANCE=review2571", app_environment.read_text())
        self.assertFalse(harness_calls.exists(), "the runner started another compositor")
        self.assertTrue(log.exists())

    def test_a_launch_below_the_lock_holder_cannot_compile(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        for holder in (["flock"], ["flock", "--close"]):
            with self.subTest(holder=holder):
                result = self.under_native_lock(*holder)
                self.assert_compiling_is_forbidden(result, guard_calls)
                guard_calls.unlink()
                self.touch_source()

    def touch_source(self):
        self.source.write_text(self.source.read_text() + "\n")

    def test_a_grandchild_of_the_lock_holder_cannot_compile(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        lock = self.root / "native-lock"
        lock.touch()
        result = subprocess.run(
            ["flock", "--close", str(lock), "sh", "-c", f'"{self.runner}" --browserless; exit $?'],
            cwd=self.root, env=self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock)},
            capture_output=True, text=True, timeout=10,
        )
        self.assert_compiling_is_forbidden(result, guard_calls)

    def test_a_symlinked_lock_path_is_the_same_lock(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        lock = self.root / "real-lock"
        lock.touch()
        link = self.root / "linked-lock"
        link.symlink_to(lock)
        result = subprocess.run(
            ["flock", "--close", str(lock), str(self.runner), "--browserless"],
            cwd=self.root, env=self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(link)},
            capture_output=True, text=True, timeout=10,
        )
        self.assert_compiling_is_forbidden(result, guard_calls)

    def holding_the_lock_elsewhere(self):
        lock = self.root / "native-lock"
        lock.touch()
        holder = subprocess.Popen(["flock", "--close", str(lock), "sleep", "30"], start_new_session=True)
        self.addCleanup(holder.wait)
        self.addCleanup(os.killpg, holder.pid, signal.SIGKILL)
        deadline = time.monotonic() + 5
        while subprocess.run(["flock", "-n", str(lock), "true"]).returncode == 0:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.01)
        return lock

    def compositor(self, name="niri", **environment):
        """A live process with this name; its name comes from its executable."""
        executable = self.root / f"compositor-{time.monotonic_ns()}/{name}"
        executable.parent.mkdir()
        shutil.copy(shutil.which("sleep"), executable)
        without_display = {key: value for key, value in self.env.items()
                           if key not in ("WAYLAND_DISPLAY", "DISPLAY")}
        process = subprocess.Popen([str(executable), "30"], env=without_display | environment)
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        return f"/run/user/1000/niri.wayland-2.{process.pid}.sock"

    def test_a_nested_session_counts_as_under_a_held_lock(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        lock = self.holding_the_lock_elsewhere()
        session_runtime = self.root / "nh-fixture"
        session_runtime.mkdir()
        for name, changes in (
            ("harness exec", {"XDG_RUNTIME_DIR": str(session_runtime)}),
            # Whether niri-harness is still its parent or has exited: niri under
            # systemd is the normal state once `session start` has returned.
            ("nested in Wayland", {"NIRI_SOCKET": self.compositor(WAYLAND_DISPLAY="wayland-0")}),
            ("nested in X11", {"NIRI_SOCKET": self.compositor(DISPLAY=":0")}),
        ):
            with self.subTest(route=name):
                environment = self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock)} | changes
                result = subprocess.run(
                    [str(self.runner), "--browserless"], cwd=self.root, env=environment,
                    capture_output=True, text=True, timeout=10,
                )
                self.assert_compiling_is_forbidden(result, guard_calls)
                guard_calls.unlink()
                self.touch_source()

    def test_the_desktops_compositor_and_a_free_lock_do_not_count(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        lock = self.holding_the_lock_elsewhere()
        session_runtime = self.root / "nh-fixture"
        session_runtime.mkdir()
        free = self.root / "free-lock"
        free.touch()
        unreadable = self.root / "unreadable-lock"
        unreadable.touch()
        unreadable.chmod(0)
        missing = self.root / "missing-lock"
        held = {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock)}
        session = {"XDG_RUNTIME_DIR": str(session_runtime)}
        cases = [
            ("someone else holds it", held),
            ("desktop niri", held | {"NIRI_SOCKET": self.compositor()}),
            ("reused process ID", held | {"NIRI_SOCKET": self.compositor("other", WAYLAND_DISPLAY="wayland-0")}),
            ("gone compositor", held | {"NIRI_SOCKET": "/run/user/1000/niri.wayland-2.2147483646.sock"}),
            ("harness session, lock free", {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(free)} | session),
            ("no lock file", {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(missing)} | session),
        ]
        if os.geteuid() != 0:
            cases.append(("unreadable lock", {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(unreadable)} | session))
        for name, changes in cases:
            with self.subTest(case=name):
                result, _calls = self.launch("--browserless", **changes)
                self.assertNotIn("may only confirm", result.stderr)
                self.assertIn(f"lock={self.env['CAGENTS_BUILD_LOCK']} ", guard_calls.read_text().splitlines()[0])
                self.assertIn("wrapper= ", guard_calls.read_text().splitlines()[0])
                guard_calls.unlink()
                self.touch_source()
        self.assertFalse(missing.exists(), "checking for the lock created it")

    def test_the_guard_overrides_the_callers_gate_and_stays_out_of_the_app(self):
        guard_calls = self.fake_cargo_reports_its_guard()
        app_environment = self.root / "app-environment"
        result = self.under_native_lock(
            "flock", "--close", CAGENTS_BUILD_REMOTE="elsewhere", CAGENTS_BUILD_SLOTS="4",
            CAGENTS_BUILD_SEED="1", APP_ENV=str(app_environment),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lock = self.root / "native lock"
        for line in guard_calls.read_text().splitlines():
            self.assertIn(f"seed=0 remote=local slots=1 held={lock}", line)
        launched = app_environment.read_text()
        self.assertNotIn("RUSTC_WRAPPER=", launched)
        self.assertNotIn("CAGENTS_NATIVE_LOCK_HELD=", launched)
        self.assertIn(f"CAGENTS_BUILD_LOCK={self.env['CAGENTS_BUILD_LOCK']}\n", launched)

    def test_a_guarded_build_records_the_same_identity_as_any_other(self):
        result = self.under_native_lock("flock", "--close")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.cargo_calls.read_text().splitlines()), 3)
        self.cargo_calls.unlink()
        result = self.under_native_lock("flock", "--close")
        self.assertIn("skipping the build", result.stderr)
        self.assert_skipped("--browserless")

    def test_a_guarded_build_that_outlasts_its_bound_gives_up_with_everything_it_started(self):
        started = time.monotonic()
        result = self.under_native_lock(
            "flock", "--close", FAKE_BUILD_SECONDS="30", CAGENTS_NATIVE_LOCK_BUILD_SECONDS="1",
        )
        # capture_output returns only once nothing below the launch keeps its pipes open.
        self.assertLess(time.monotonic() - started, 8)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("refusing to keep waiting while holding the native lock", result.stderr)

    def test_ending_a_guarded_launch_ends_its_build_and_frees_a_plain_flocks_lock(self):
        lock = self.root / "native-lock"
        lock.touch()
        for name, deliver in (
            ("kill", lambda holder, runner: os.kill(runner, signal.SIGKILL)),
            ("terminate", lambda holder, runner: os.kill(runner, signal.SIGTERM)),
            ("interrupt the group", lambda holder, runner: os.killpg(holder, signal.SIGINT)),
            ("hang up the group", lambda holder, runner: os.killpg(holder, signal.SIGHUP)),
        ):
            with self.subTest(signal=name):
                holder = subprocess.Popen(
                    ["flock", str(lock), str(self.runner), "--browserless"], cwd=self.root,
                    env=self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock), "FAKE_BUILD_SECONDS": "60"},
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
                )
                self.addCleanup(holder.wait)
                self.addCleanup(lambda process=holder: process.poll() is None and process.kill())
                deadline = time.monotonic() + 8
                while not self.cargo_calls.exists():
                    self.assertLess(time.monotonic(), deadline, "the build never started")
                    time.sleep(0.02)
                runner = int(subprocess.run(
                    ["pgrep", "-P", str(holder.pid)], capture_output=True, text=True,
                ).stdout.split()[0])
                deliver(holder.pid, runner)
                deadline = time.monotonic() + 5
                while subprocess.run(["flock", "-n", str(lock), "true"]).returncode != 0:
                    self.assertLess(time.monotonic(), deadline, "the build kept the native lock")
                    time.sleep(0.05)
                self.cargo_calls.unlink()

    def test_a_launch_that_ends_before_its_build_can_follow_it_starts_no_build(self):
        arming = self.root / "arming"
        real = shutil.which("setpriv")
        slow = self.root / "bin/setpriv"
        slow.write_text(f'#!/bin/sh\n: >"{arming}"\nsleep 1\nexec "{real}" "$@"\n')
        slow.chmod(0o755)
        lock = self.root / "native-lock"
        lock.touch()
        holder = subprocess.Popen(
            ["flock", str(lock), str(self.runner), "--browserless"], cwd=self.root,
            env=self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock), "FAKE_BUILD_SECONDS": "60"},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
        )
        self.addCleanup(holder.wait)
        self.addCleanup(lambda: holder.poll() is None and holder.kill())
        deadline = time.monotonic() + 8
        while not arming.exists():
            self.assertLess(time.monotonic(), deadline, "the build never began")
            time.sleep(0.01)
        runner = int(subprocess.run(
            ["pgrep", "-P", str(holder.pid)], capture_output=True, text=True,
        ).stdout.split()[0])
        os.kill(runner, signal.SIGKILL)
        deadline = time.monotonic() + 5
        while subprocess.run(["flock", "-n", str(lock), "true"]).returncode != 0:
            self.assertLess(time.monotonic(), deadline, "the build kept the native lock")
            time.sleep(0.05)
        self.assertFalse(self.cargo_calls.exists())

    def test_a_bound_that_is_not_a_positive_number_refuses_to_build(self):
        for value in ("abc", "-1", "0", "00", "0.5"):
            with self.subTest(value=value):
                result = self.under_native_lock(
                    "flock", "--close", CAGENTS_NATIVE_LOCK_BUILD_SECONDS=value,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("must be a positive number of seconds", result.stderr)
                self.assertFalse(self.cargo_calls.exists())

    def test_a_guarded_launch_without_a_target_directory_fails_before_building(self):
        cargo = self.root / "bin/cargo"
        cargo.write_text(cargo.read_text().replace("    metadata)\n", "    metadata)\n        exit 101\n", 1))
        result = self.under_native_lock("flock", "--close")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cannot find the Cargo target directory", result.stderr)
        self.assertFalse(self.cargo_calls.exists())

    def test_a_current_launch_holding_the_lock_starts_without_cargo(self):
        self.assert_built("--browserless")
        result = self.under_native_lock("flock", "--close")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping the build", result.stderr)
        self.assertFalse(self.cargo_calls.exists())

    def test_a_refused_build_fails_the_launch(self):
        result = self.under_native_lock("flock", "--close", FAKE_FAIL_BINARY="cagents-ui")
        self.assertEqual(result.returncode, 101, result.stderr)

    def test_a_hot_launch_holding_the_native_lock_refuses(self):
        lock = self.root / "native-lock"
        lock.touch()
        hot = self.runner.with_name("hot-development")
        hot.write_text('#!/bin/sh\nexit 0\n')
        hot.chmod(0o755)
        result = subprocess.run(
            ["flock", "--close", str(lock), str(self.runner), "--hot"], cwd=self.root,
            env=self.env | {"CAGENTS_EVIDENCE_NATIVE_LOCK": str(lock)},
            capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn("hot reload always builds", result.stderr)

    def test_up_to_date_launch_skips_the_build(self):
        self.assert_built("--browserless")
        self.assert_skipped("--browserless")
        self.assert_skipped("--browserless")

    def test_the_app_runs_under_a_memory_limit(self):
        ran = self.root / "limited"
        limit = self.runner.with_name("memory-capped")
        limit.write_text(f'#!/bin/sh\necho "$2" >"{ran}"\nshift 2\necho "$1" >>"{ran}"\nexec "$@"\n')
        limit.chmod(0o755)
        self.assert_built("--browserless")
        name, program = ran.read_text().splitlines()
        self.assertEqual(name, "cagents-ui")
        self.assertTrue(program.endswith("/cagents-ui"))

    def test_an_instance_runs_under_a_memory_limit(self):
        limit = self.runner.with_name("memory-capped")
        limit.write_text('#!/bin/sh\nshift 2\nexec "$@"\n')
        limit.chmod(0o755)
        started = self.root / "started"
        harness = self.root / "bin/niri-harness"
        harness.write_text(f'#!/bin/sh\ncase " $* " in *" start "*) printf "%s\\n" "$@" >"{started}" ;; *) exit 1 ;; esac\n')
        harness.chmod(0o755)
        self.launch("--browserless", "--instance", "capped")
        words = started.read_text().splitlines()
        program = words.index(str(limit))
        self.assertEqual(words[program + 1:program + 3], ["--name", "cagents-ui"])
        self.assertTrue(words[program + 3].endswith("/cagents-ui"), words)

    def test_an_instance_daemon_log_is_chosen_by_the_flag_alone(self):
        started = self.root / "started"
        harness = self.root / "bin/niri-harness"
        harness.write_text(
            '#!/bin/sh\ncase " $* " in *" start "*) printf "%s|%s\\n" '
            f'"${{CAGENTS_DEVELOPMENT_DAEMON_LOG-unset}}" "${{CAGENTS_LOCAL_DEBUG-unset}}" >"{started}" ;;'
            ' *) exit 1 ;; esac\n'
        )
        harness.chmod(0o755)
        caller = self.root / "caller"
        caller.mkdir()
        result = subprocess.run(
            [str(self.runner), "--browserless", "--instance", "logged", "--daemon-log", "daemon.log"],
            cwd=caller, env=self.env, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(started.read_text(), f"{caller / 'daemon.log'}|1\n")
        self.launch("--browserless", "--instance", "unlogged",
                    CAGENTS_DEVELOPMENT_DAEMON_LOG="/inherited.log")
        self.assertEqual(started.read_text(), "unset|unset\n")

    def test_the_memory_limit_speaks_to_the_terminal_and_the_app_to_the_log(self):
        limit = self.runner.with_name("memory-capped")
        limit.write_text('#!/bin/sh\nshift 2\n"$@"\necho limit-message >&2\n')
        limit.chmod(0o755)
        log = self.root / "ui.log"
        result, _ = self.launch("--browserless", "--log-file", str(log))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("limit-message", result.stderr)
        self.assertTrue(log.exists())
        self.assertNotIn("limit-message", log.read_text())

    def test_a_launch_building_on_the_build_host_always_asks_it(self):
        # Cargo's dependency lists stay there, so nothing here can say the
        # products are current.
        offload = self.runner.with_name("cargo-offload")
        # One slot there serves all three builds: the hold runs the script.
        offload.write_text('#!/bin/sh\n[ "$1" != --host ] || { echo build-host; exit 0; }\n'
                           '[ "$1" != --hold ] || { shift; printf "hold\\n" >>"$CARGO_CALLS"; exec "$@"; }\n'
                           'exec cargo "$@"\n')
        offload.chmod(0o755)
        for _ in range(2):
            result, calls = self.launch("--browserless")
            self.assertEqual(calls[0], "hold", result.stderr)
            self.assertEqual(len(calls), 4, calls)
            self.assertNotIn("skipping the build", result.stderr)

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

    def test_listing_shows_a_development_daemon_its_checkout_and_starter(self):
        checkout = self.root / "worktree"
        daemon = checkout / "target/dev-runtime/cagentsd"
        daemon.parent.mkdir(parents=True)
        shutil.copy2("/bin/sleep", daemon)
        runtime = self.root / "nh-fixture"
        runtime.mkdir()
        starter = os.getpid()
        start = Path(f"/proc/{starter}/stat").read_text().rsplit(") ", 1)[1].split()[19]
        (runtime / "cagents-run-development.owner").write_text(
            f"checkout={checkout}\nowner={starter}\nowner_start={start}\n"
            "owner_name=fixture-agent\nhosted=true\n")
        lock = runtime / "cagents-development-fx1/cagentsd.lock"
        lock.parent.mkdir()
        # Like a daemon, it keeps its instance's lock open.
        process = subprocess.Popen(
            ["/bin/sh", "-c", 'exec 9>>"$1" && exec "$2" 30', "sh", str(lock), str(daemon)],
            env={"XDG_RUNTIME_DIR": str(runtime), "CAGENTS_DEVELOPMENT_INSTANCE": "fx1"})
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)

        def listing():
            result = subprocess.run(
                [str(self.runner), "--list-instances"], cwd=self.root,
                env=self.env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            lines = result.stdout.splitlines()
            index = lines.index(next(line for line in lines if f"pid {process.pid}," in line))
            return lines[index:index + 6]

        deadline = time.monotonic() + 5
        while not any(os.readlink(f"/proc/{process.pid}/fd/{fd}") == str(lock)
                      for fd in os.listdir(f"/proc/{process.pid}/fd")):
            self.assertLess(time.monotonic(), deadline, "fixture daemon never opened its lock")
            time.sleep(0.01)
        block = listing()
        self.assertEqual(
            block[4],
            "  lifetime:   started by an older runner: runs until stopped, or scripts/run-development --gc once its starter has ended")
        self.assertEqual(
            block[5],
            f"  stop:       scripts/run-development --stop-instance fx1, once niri-harness is installed; or kill {process.pid}")
        # A harness that knows no such session.
        harness = Path(self.env["PATH"].split(":")[0]) / "niri-harness"
        harness.write_text("#!/bin/sh\nexit 1\n")
        harness.chmod(0o755)
        block = listing()
        self.assertTrue(block[0].startswith(f"fx1, pid {process.pid}, running "), block)
        self.assertEqual(block[1], f"  checkout:   {checkout}")
        self.assertEqual(block[2], f"  runtime:    {runtime}")
        self.assertEqual(
            block[3],
            f"  started by: fixture-agent (pid {starter}), still running, in a cagents session")
        # Without a runner session, only its process can be named.
        self.assertTrue(block[5].startswith(f"  stop:       kill {process.pid} "), block)
        # What each recorded lifetime says, with no watcher running.
        record = runtime / "cagents-run-development.owner"
        base = record.read_text()
        for lifetime, said in (
                ("lifetime=kept\n", "kept until stopped"),
                ("lifetime=kept\nttl=60\n", "kept until stopped, or 60 seconds after it started; "
                 "its watcher is gone, so the next launch or --gc applies this"),
                ("lifetime=owner\ngrace=600\n", "stops 600 seconds after its starter ends; its "
                 "watcher is gone, so the next launch or --gc applies this")):
            with self.subTest(lifetime):
                record.write_text(base + lifetime)
                self.assertEqual(listing()[4], f"  lifetime:   {said}")
        record.write_text(base)
        # A session whose hosts can't be told is offered no way to stop it.
        unknown = self.below_daemon(
            ["--list-instances"], [f"XDG_RUNTIME_DIR={self.root / 'elsewhere'}"],
            ["XDG_RUNTIME_DIR=relative"])
        self.assertEqual(unknown.returncode, 0, unknown.stderr)
        lines = unknown.stdout.splitlines()
        index = lines.index(next(line for line in lines if f"pid {process.pid}," in line))
        self.assertEqual(
            lines[index + 5], "  stop:       not from here: which daemons host this session can't be told")
        shutil.rmtree(checkout)
        self.assertEqual(listing()[1], f"  checkout:   {checkout} (removed)")
        self.assertFalse(self.cargo_calls.exists(), "listing built")

    def below_daemon(self, arguments, daemon_environment, inner_environment=None, **changes):
        """Run the runner below a shell named cagentsd with `daemon_environment`,
        as a session a daemon hosts is; with `inner_environment`, below a second
        such daemon that runs below the first."""
        daemon = self.root / "daemon/cagentsd"
        if not daemon.exists():
            daemon.parent.mkdir()
            shutil.copy2("/bin/sh", daemon)
        environment = self.env | {"CAGENTS_AGENT_AUTHORITY": "inert-fixture-role"} | changes
        for name in ("CAGENTS_DEVELOPMENT_INSTANCE", "CAGENTS_DEVELOPMENT_HOSTED_BY"):
            environment.pop(name, None)
        environment.update(variable.split("=", 1) for variable in daemon_environment)
        environment = {name: value for name, value in environment.items() if value is not None}
        # The trailing command keeps the shell from replacing itself.
        command = [str(daemon), "-c", '"$@"; exit $?', "sh", str(self.runner), *arguments]
        if inner_environment is not None:
            command = [str(daemon), "-c", '"$@"; exit $?', "sh", "env", *inner_environment, *command]
        return subprocess.run(
            command,
            cwd=self.root, env=environment, capture_output=True, text=True, timeout=10)

    def fake_harness_session(self):
        """A harness whose instance session is live, with its runtime directory."""
        runtime = self.root / "harness-runtime/nh-fixture"
        runtime.mkdir(parents=True)
        harness = Path(self.env["PATH"].split(":")[0]) / "niri-harness"
        harness.write_text(f"""#!/bin/sh
for argument do
    case $argument in
        env) printf "XDG_RUNTIME_DIR='%s'\\n" '{runtime}'; exit 0 ;;
        stop) exit 0 ;;
        exec) exit 1 ;;
    esac
done
exit 1
""")
        harness.chmod(0o755)
        return runtime

    def test_a_hosted_stop_refuses_an_instance_that_hosts_the_session_or_its_starter(self):
        runtime = self.fake_harness_session()
        elsewhere = self.root / "elsewhere"
        hosts = "the daemon hosting this session"
        starter = "the daemons hosting the session that started this one"
        for name, daemon_environment, refused, *inner in (
            (hosts, [f"XDG_RUNTIME_DIR={runtime}"], "hosts this session"),
            (hosts + " under its instance name",
             [f"XDG_RUNTIME_DIR={runtime}", "CAGENTS_DEVELOPMENT_INSTANCE=hx"], "hosts this session"),
            # A line that names no directory doesn't end the record.
            (starter, [f"XDG_RUNTIME_DIR={elsewhere}",
                       f"CAGENTS_DEVELOPMENT_HOSTED_BY={elsewhere}/other\nrelative\n{runtime}/cagents-development"],
             "hosts this session"),
            ("a daemon above, nearer one whose runtime can't be read",
             [f"XDG_RUNTIME_DIR={elsewhere}"], "cannot tell", ["XDG_RUNTIME_DIR=relative"]),
            ("a daemon elsewhere", [f"XDG_RUNTIME_DIR={elsewhere}",
                                    f"CAGENTS_DEVELOPMENT_HOSTED_BY={elsewhere}/other"], None),
        ):
            with self.subTest(name):
                result = self.below_daemon(["--stop-instance", "hx"], daemon_environment, *inner)
                if refused:
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(refused, result.stderr)
                    self.assertNotIn("Stopping instance", result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("stopped instance hx", result.stdout)
        self.assertFalse(self.cargo_calls.exists(), "stopping built")

    def test_a_hosted_stop_with_no_daemon_above_it_is_refused(self):
        process = os.getpid()
        while process > 1:
            try:
                executable = os.readlink(f"/proc/{process}/exe")
            except OSError:
                executable = ""
            if os.path.basename(executable).startswith("cagentsd"):
                self.skipTest("these tests run in a session a daemon hosts")
            process = int(Path(f"/proc/{process}/stat").read_text().rsplit(") ", 1)[1].split()[1])
        self.fake_harness_session()
        hosted = subprocess.run(
            [str(self.runner), "--stop-instance", "hx"], cwd=self.root,
            env=self.env | {"CAGENTS_AGENT_AUTHORITY": "inert-fixture-role"},
            capture_output=True, text=True, timeout=10)
        self.assertEqual(hosted.returncode, 1, hosted.stderr)
        self.assertIn("cannot tell", hosted.stderr)
        # A person's own terminal has no daemon above it either.
        environment = self.env.copy()
        environment.pop("CAGENTS_AGENT_AUTHORITY", None)
        own = subprocess.run(
            [str(self.runner), "--stop-instance", "hx"], cwd=self.root,
            env=environment, capture_output=True, text=True, timeout=10)
        self.assertEqual(own.returncode, 0, own.stderr)

    def test_the_app_keeps_the_daemons_hosting_its_starter(self):
        self.fake_cargo_reports_its_guard()
        app_environment = self.root / "app-environment"
        elsewhere = self.root / "elsewhere"
        result = self.below_daemon(
            ["--browserless"],
            [f"XDG_RUNTIME_DIR={elsewhere}", "CAGENTS_DEVELOPMENT_INSTANCE=parent",
             f"CAGENTS_DEVELOPMENT_HOSTED_BY={elsewhere}/first\n{elsewhere}/second"],
            APP_ENV=str(app_environment))
        self.assertEqual(result.returncode, 0, result.stderr)
        launched = app_environment.read_text()
        record = launched.split("CAGENTS_DEVELOPMENT_HOSTED_BY=", 1)[1].splitlines()
        for line in ("cagents", "cagents-development", "cagents-development-parent", "first", "second"):
            self.assertIn(f"{elsewhere}/{line}", record)
        self.assertNotIn("warning:", result.stderr)

    def test_a_hosted_launch_warns_of_a_host_it_cant_read(self):
        self.fake_cargo_reports_its_guard()
        app_environment = self.root / "app-environment"
        elsewhere = self.root / "elsewhere"
        for hosted in (True, False):
            with self.subTest(hosted=hosted):
                changes = {} if hosted else {"CAGENTS_AGENT_AUTHORITY": None}
                result = self.below_daemon(
                    ["--browserless"], [f"XDG_RUNTIME_DIR={elsewhere}"],
                    ["XDG_RUNTIME_DIR=relative"], APP_ENV=str(app_environment), **changes)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual("can't all be told" in result.stderr, hosted, result.stderr)
                launched = app_environment.read_text()
                app_environment.unlink()
                record = launched.split("CAGENTS_DEVELOPMENT_HOSTED_BY=", 1)[1].splitlines()
                self.assertIn(f"{elsewhere}/cagents", record)
                self.assertNotIn("?", record)

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

    def test_both_lanes_build_one_cli_and_daemon_and_differ_only_in_the_ui(self):
        _, browserless = self.launch("--browserless")
        _, integrated = self.launch()
        common = "build --locked --profile dev-runtime"
        self.assertEqual(browserless[:2], [
            f"{common} --no-default-features --features development --bin cagents",
            f"{common} --no-default-features --features development,host --bin cagentsd",
        ])
        self.assertEqual(integrated[:2], browserless[:2])
        self.assertEqual(browserless[2:], [
            f"{common} --no-default-features --features development,ui --bin cagents-ui"])
        self.assertEqual(integrated[2:], [
            f"{common} --features development,integrated-browser --bin cagents-ui"])

    def test_the_other_lane_rebuilds_and_invalidates_this_one(self):
        self.assert_built("--browserless")
        self.assert_built()
        self.assert_skipped()
        self.assert_built("--browserless")


def process_start(pid):
    return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19]


def wait_for(predicate, timeout=10.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


class InstanceLifetime(unittest.TestCase):
    """An instance stops once the session that started it has been gone for its
    grace period, or its --ttl passes, unless it is kept; and what dead
    sessions and unused instances leave behind can be cleared."""

    def setUp(self):
        # Its fake Cargo builds, so a launch gets as far as starting a session.
        LaunchFreshness.setUp(self)
        self.runtime_root = self.root / "rt"
        self.runtime_root.mkdir()
        self.sessions = self.root / "harness-sessions"
        self.sessions.mkdir()
        self.harness_calls = self.root / "harness-calls"
        # A harness that knows the sessions described in self.sessions: one file
        # per session, holding its state and runtime directory.
        harness = self.root / "bin/niri-harness"
        harness.write_text(f"""#!/bin/sh
sessions='{self.sessions}'
printf '%s\\n' "$*" >>'{self.harness_calls}'
session=
while [ "$#" -gt 0 ]; do
    case $1 in
        --session) session=$2; shift 2 ;;
        list)
            for file in "$sessions"/*; do
                [ -f "$file" ] || continue
                read -r state runtime <"$file"
                printf '%s %s headless 0%%  %s/%s root\\n' "${{file##*/}}" "$state" "$sessions" "${{file##*/}}.d"
            done
            exit 0 ;;
        env)
            [ -f "$sessions/$session" ] || exit 1
            read -r state runtime <"$sessions/$session"
            printf "export XDG_RUNTIME_DIR='%s'\\n" "$runtime"
            exit 0 ;;
        stop) rm -f "$sessions/$session"; exit 0 ;;
        start)
            # Started as the runner names it, with a fresh runtime.
            while [ "$1" != --name ]; do shift; done
            mkdir -p "$sessions/$2.runtime"
            printf 'live %s\n' "$sessions/$2.runtime" >"$sessions/$2"
            exit 0 ;;
        gc) exit 0 ;;
        *) shift ;;
    esac
done
exit 1
""")
        harness.chmod(0o755)
        self.env.update(XDG_RUNTIME_DIR=str(self.runtime_root),
                        XDG_STATE_HOME=str(self.root / "state"),
                        XDG_CONFIG_HOME=str(self.root / "config"),
                        XDG_CACHE_HOME=str(self.root / "cache"))
        for name in ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_DEVELOPMENT_INSTANCE",
                     "CAGENTS_DEVELOPMENT_HOSTED_BY", "CAGENTS_INSTANCE_GRACE_SECONDS"):
            self.env.pop(name, None)

    def session(self, name, state="asleep", record=None, age=None):
        """A harness session for instance `name`, with a daemon-like process
        started with its runtime, and the runner's record when given."""
        runtime = self.runtime_root / f"nh-{name}"
        runtime.mkdir()
        (self.sessions / f"cagents-{name}").write_text(f"{state} {runtime}\n")
        root = self.sessions / f"cagents-{name}.d root"
        root.mkdir()
        if age is not None:
            then = time.time() - age
            os.utime(root, (then, then))
        if record is not None:
            (runtime / "cagents-run-development.owner").write_text(
                "".join(f"{key}={value}\n" for key, value in record.items()))
        process = subprocess.Popen(["sleep", "60"], env={"XDG_RUNTIME_DIR": str(runtime),
                                                          "PATH": "/usr/bin:/bin"})
        self.addCleanup(lambda: (process.kill(), process.wait()))
        return runtime, process

    def ended_owner(self):
        owner = subprocess.Popen(["true"])
        owner.wait()
        return {"owner": owner.pid, "owner_start": "1", "owner_name": "ended-agent"}

    def living_owner(self):
        return {"owner": os.getpid(), "owner_start": process_start(os.getpid()),
                "owner_name": "living-agent"}

    def run_runner(self, *arguments, **changes):
        return subprocess.run([str(self.runner), *arguments], cwd=self.root,
                              env=self.env | changes, capture_output=True, text=True, timeout=30)

    def assert_stopped(self, runtime, process):
        self.assertEqual(process.wait(timeout=15), -signal.SIGTERM)
        # A watcher sweeps once the processes it signalled have gone.
        deadline = time.monotonic() + 15
        while runtime.exists():
            self.assertLess(time.monotonic(), deadline, "the runtime was not swept")
            time.sleep(0.1)

    def test_gc_stops_an_instance_whose_starter_ended_past_its_grace(self):
        old = int(time.time()) - 100
        runtime, process = self.session("ended", record=self.ended_owner() | {
            "lifetime": "owner", "grace": 50, "started_at": old})
        result = self.run_runner("--gc")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("instance ended: stopped it because ended-agent", result.stderr)
        self.assertIn("which started it, has ended, and it started over 50 seconds ago", result.stderr)
        self.assert_stopped(runtime, process)
        self.assertFalse((self.sessions / "cagents-ended").exists())
        log = (self.root / "state/cagents-development-ended/run-development.log").read_text()
        self.assertIn("stopped: ended-agent", log)
        self.assertFalse(self.cargo_calls.exists(), "collecting built")

    def test_gc_leaves_instances_within_grace_kept_or_still_owned(self):
        now = int(time.time())
        cases = {
            "fresh": self.ended_owner() | {"lifetime": "owner", "grace": 600, "started_at": now},
            "kept": self.ended_owner() | {"lifetime": "kept", "grace": 0, "started_at": now - 100},
            "owned": self.living_owner() | {"lifetime": "owner", "grace": 0, "started_at": now - 100},
        }
        sessions = {name: self.session(name, record=record) for name, record in cases.items()}
        result = self.run_runner("--gc")
        self.assertEqual(result.returncode, 0, result.stderr)
        for name, (runtime, process) in sessions.items():
            with self.subTest(name):
                self.assertIsNone(process.poll())
                self.assertTrue(runtime.exists())

    def test_a_ttl_stops_even_a_kept_instance_whose_starter_runs(self):
        runtime, process = self.session("brief", record=self.living_owner() | {
            "lifetime": "kept", "grace": 600, "started_at": int(time.time()) - 30, "ttl": 20})
        result = self.run_runner("--gc")
        self.assertIn("instance brief: stopped it because its --ttl of 20 seconds passed", result.stderr)
        self.assert_stopped(runtime, process)

    def test_gc_tells_of_a_running_instance_nothing_records_and_leaves_it(self):
        runtime, process = self.session("unrecorded")
        result = self.run_runner("--gc")
        self.assertIn("instance unrecorded: left running, since nothing records who started it; "
                      "stop it with --stop-instance unrecorded", result.stderr)
        self.assertIsNone(process.poll())

    def test_a_dead_session_is_cleared_once_it_has_been_quiet_for_a_minute(self):
        stale, stale_process = self.session("stale", state="dead", age=120)
        starting, starting_process = self.session("starting", state="dead")
        result = self.run_runner("--gc")
        self.assertIn("instance stale: its session had ended without being stopped; cleared what it left",
                      result.stderr)
        self.assert_stopped(stale, stale_process)
        # It may still be starting: it has not yet recorded its compositor.
        self.assertNotIn("instance starting", result.stderr)
        self.assertIsNone(starting_process.poll())
        self.assertTrue(starting.exists())

    def test_a_launch_clears_its_names_dead_session_instead_of_refusing(self):
        runtime, process = self.session("again", state="dead", age=120)
        limit = self.runner.with_name("memory-capped")
        limit.write_text('#!/bin/sh\nshift 2\nexec "$@"\n')
        limit.chmod(0o755)
        result = self.run_runner("--browserless", "--instance", "again")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("start --backend headless --name cagents-again", self.harness_calls.read_text())
        self.assertIn("instance again: its session had ended without being stopped", result.stderr)
        self.assert_stopped(runtime, process)

    def test_a_launch_records_its_lifetime_and_leaves_a_watcher(self):
        result = self.run_runner("--browserless", "--instance", "kept", "--keep", "--ttl", "600",
                                 CAGENTS_INSTANCE_WATCH_SECONDS="0.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Instance kept is running in niri-harness session cagents-kept; it runs until "
                      "you stop it, or 600 seconds after it started.", result.stdout)
        record = self.sessions / "cagents-kept.runtime/cagents-run-development.owner"
        deadline = time.monotonic() + 10
        while "watcher=" not in record.read_text():
            self.assertLess(time.monotonic(), deadline, "no watcher recorded itself")
            time.sleep(0.05)
        text = record.read_text()
        self.assertIn("lifetime=kept\n", text)
        self.assertIn("ttl=600\n", text)
        watcher = int(text.split("watcher=", 1)[1].split()[0])
        self.addCleanup(lambda: os.kill(watcher, signal.SIGKILL) if Path(f"/proc/{watcher}").exists() else None)
        # The watcher outlives the launch, and ends with its session.
        self.assertTrue(Path(f"/proc/{watcher}").exists())
        (self.sessions / "cagents-kept").unlink()
        deadline = time.monotonic() + 10
        while Path(f"/proc/{watcher}").exists() and \
                Path(f"/proc/{watcher}/stat").read_text().rsplit(") ", 1)[1][0] != "Z":
            self.assertLess(time.monotonic(), deadline, "the watcher outlived its session")
            time.sleep(0.05)

    def test_a_kept_launch_without_a_ttl_leaves_no_watcher(self):
        result = self.run_runner("--browserless", "--instance", "forever", "--keep")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("it runs until you stop it.", result.stdout)
        time.sleep(0.5)
        record = self.sessions / "cagents-forever.runtime/cagents-run-development.owner"
        self.assertIn("lifetime=kept\n", record.read_text())
        self.assertNotIn("watcher=", record.read_text())

    def test_a_starter_whose_start_was_not_recorded_counts_as_ended(self):
        record = self.living_owner() | {"lifetime": "owner", "grace": 0, "started_at": 1}
        record["owner_start"] = ""
        runtime, process = self.session("unstarted", record=record)
        result = self.run_runner("--gc")
        self.assertIn("instance unstarted: stopped it because living-agent", result.stderr)
        self.assert_stopped(runtime, process)

    def test_a_stop_never_ends_the_processes_it_runs_under(self):
        # A caller started with the instance's runtime, as a shell inside it
        # would be, that runs the runner with another.
        runtime, process = self.session("parent", record=self.ended_owner() | {
            "lifetime": "owner", "grace": 0, "started_at": 1})
        result = subprocess.run(
            ["sh", "-c", 'XDG_RUNTIME_DIR="$1" "$2" --gc; echo caller-survived', "sh",
             str(self.runtime_root), str(self.runner)],
            cwd=self.root, env=self.env | {"XDG_RUNTIME_DIR": str(runtime)},
            capture_output=True, text=True, timeout=30)
        self.assertIn("caller-survived", result.stdout, result.stderr)
        self.assert_stopped(runtime, process)

    def test_only_a_private_session_runtime_is_stopped_or_swept(self):
        shared = self.root / "shared-runtime"
        shared.mkdir()
        (self.sessions / "cagents-odd").write_text(f"dead {shared}\n")
        root = self.sessions / "cagents-odd.d root"
        root.mkdir()
        os.utime(root, (time.time() - 120, time.time() - 120))
        process = subprocess.Popen(["sleep", "60"], env={"XDG_RUNTIME_DIR": str(shared)})
        self.addCleanup(lambda: (process.kill(), process.wait()))
        result = self.run_runner("--gc")
        self.assertEqual(result.returncode, 0, result.stderr)
        time.sleep(0.5)
        self.assertIsNone(process.poll())
        self.assertTrue(shared.exists())

    def test_gc_from_a_shell_without_a_runtime_directory_checks_the_users(self):
        # Cron, or an ssh login without a user session, has no XDG_RUNTIME_DIR.
        shared = self.root / "shared-runtime"
        shared.mkdir()
        (self.sessions / "cagents-odd").write_text(f"dead {shared}\n")
        root = self.sessions / "cagents-odd.d root"
        root.mkdir()
        os.utime(root, (time.time() - 120, time.time() - 120))
        environment = {key: value for key, value in self.env.items() if key != "XDG_RUNTIME_DIR"}
        result = subprocess.run([str(self.runner), "--gc"], cwd=self.root, env=environment,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("XDG_RUNTIME_DIR", result.stderr)
        # Outside the user's runtime directory, so not a session's own.
        self.assertTrue(shared.exists())

    def test_gc_says_so_and_fails_when_the_harness_cannot_list(self):
        harness = self.root / "bin/niri-harness"
        harness.write_text("#!/bin/sh\necho 'state directory is locked' >&2\nexit 3\n")
        harness.chmod(0o755)
        result = self.run_runner("--gc", "--storage", "7")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("niri-harness list failed, so no instance was checked: state directory is locked",
                      result.stderr)
        self.assertIn("no storage was removed either", result.stderr)

    def test_the_watcher_does_not_carry_the_callers_session_runtime(self):
        # Started from inside another instance's session, it must not be one
        # of the processes that stopping that session ends.
        caller = self.runtime_root / "nh-caller"
        caller.mkdir()
        result = self.run_runner("--browserless", "--instance", "inner",
                                 XDG_RUNTIME_DIR=str(caller), CAGENTS_INSTANCE_WATCH_SECONDS="0.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        record = self.sessions / "cagents-inner.runtime/cagents-run-development.owner"
        self.assertTrue(wait_for(lambda: "watcher=" in record.read_text()))
        watcher = int(record.read_text().split("watcher=", 1)[1].split()[0])
        self.addCleanup(lambda: os.kill(watcher, signal.SIGKILL) if Path(f"/proc/{watcher}").exists() else None)
        environment = Path(f"/proc/{watcher}/environ").read_bytes().split(b"\0")
        self.assertIn(f"XDG_RUNTIME_DIR={self.runtime_root}".encode(), environment)
        (self.sessions / "cagents-inner").unlink()

    def test_a_live_session_still_refuses_a_second_launch(self):
        self.session("busy", record=self.living_owner() | {"lifetime": "owner", "grace": 600,
                                                             "started_at": int(time.time())})
        result = self.run_runner("--browserless", "--instance", "busy")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("instance busy already has a running session; relaunch it with --fresh-daemon "
                      "or stop it with --stop-instance busy", result.stderr)

    def test_collection_never_stops_the_instance_hosting_the_caller(self):
        runtime, process = self.session("host", record=self.ended_owner() | {
            "lifetime": "owner", "grace": 0, "started_at": 1})
        result = LaunchFreshness.below_daemon(self, ["--gc"], [f"XDG_RUNTIME_DIR={runtime}"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(process.poll())
        self.assertTrue(runtime.exists())
        # Nor anything, when its hosts can't be told.
        unknown = LaunchFreshness.below_daemon(
            self, ["--gc"], [f"XDG_RUNTIME_DIR={self.root / 'elsewhere'}"], ["XDG_RUNTIME_DIR=relative"])
        self.assertIn("cannot tell which daemons host this session", unknown.stderr)
        self.assertIsNone(process.poll())

    def test_the_watcher_stops_its_instance_once_the_starter_has_been_gone_for_the_grace(self):
        owner = subprocess.Popen(["sleep", "60"])
        self.addCleanup(lambda: (owner.kill(), owner.wait()))
        runtime, process = self.session("watched", record={
            "owner": owner.pid, "owner_start": process_start(owner.pid), "owner_name": "sleep",
            "lifetime": "owner", "grace": 1, "started_at": int(time.time())})
        watcher = subprocess.Popen([str(self.runner), "__watch-instance", "watched", str(runtime)],
                                   env=self.env | {"CAGENTS_INSTANCE_WATCH_SECONDS": "0.2"},
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: (watcher.kill(), watcher.wait()))
        time.sleep(1.5)
        # While its starter runs, the instance does.
        self.assertIsNone(process.poll())
        self.assertIn(f"watcher={watcher.pid}\n", (runtime / "cagents-run-development.owner").read_text())
        owner.kill()
        owner.wait()
        self.assert_stopped(runtime, process)
        self.assertEqual(watcher.wait(timeout=10), 0)
        log = (self.root / "state/cagents-development-watched/run-development.log").read_text()
        self.assertIn("stopped: sleep (pid", log)

    def test_the_watcher_ends_with_its_session(self):
        runtime, process = self.session("short", record=self.living_owner() | {
            "lifetime": "owner", "grace": 600, "started_at": int(time.time())})
        watcher = subprocess.Popen([str(self.runner), "__watch-instance", "short", str(runtime)],
                                   env=self.env | {"CAGENTS_INSTANCE_WATCH_SECONDS": "0.2"})
        self.addCleanup(lambda: (watcher.kill(), watcher.wait()))
        (self.sessions / "cagents-short").unlink()
        self.assertEqual(watcher.wait(timeout=10), 0)
        self.assertIsNone(process.poll(), "a watcher whose session ended stopped something")

    def test_storage_unused_for_the_given_days_is_removed_but_not_running_or_unnamed(self):
        old = time.time() - 10 * 86400
        roots = [self.xdg, self.root / "config", self.root / "state", self.root / "cache"]

        def storage(name, age):
            for root in roots:
                directory = root / name
                (directory / "inner").mkdir(parents=True)
                (directory / "inner/file").write_text("x")
                for path in (directory / "inner/file", directory / "inner", directory):
                    os.utime(path, (age, age))

        storage("cagents-development-unused", old)
        storage("cagents-development-recent", time.time())
        storage("cagents-development-running", old)
        storage("cagents-development", old)
        storage("cagents-other", old)
        # Only cached data left of this one.
        (self.root / "cache/cagents-development-cacheonly").mkdir()
        os.utime(self.root / "cache/cagents-development-cacheonly", (old, old))
        daemon = subprocess.Popen(["sleep", "60"], env={"CAGENTS_DEVELOPMENT_INSTANCE": "running"})
        self.addCleanup(lambda: (daemon.kill(), daemon.wait()))
        result = self.run_runner("--gc", "--storage", "7")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("removed the storage of instance unused (", result.stdout)
        self.assertIn("removed the storage of instance cacheonly (", result.stdout)
        for root in roots:
            self.assertFalse((root / "cagents-development-unused").exists())
            for kept in ("recent", "running"):
                self.assertTrue((root / f"cagents-development-{kept}").exists(), kept)
            self.assertTrue((root / "cagents-development").exists())
            self.assertTrue((root / "cagents-other").exists())

    def test_lifetime_options_are_checked_before_anything_runs(self):
        for arguments, message in (
            (["--keep"], "--keep and --ttl go with --instance NAME"),
            (["--ttl", "60", "--stop-instance", "x"], "--keep and --ttl go with --instance NAME"),
            (["--instance", "x", "--ttl", "soon"], "--ttl requires a whole number of seconds"),
            (["--storage", "3"], "--storage only goes with --gc"),
        ):
            with self.subTest(arguments):
                result = self.run_runner(*arguments)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(message, result.stderr)
        self.assertFalse(self.harness_calls.exists())


if __name__ == "__main__":
    unittest.main()
