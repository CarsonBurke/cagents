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
            return lines[index:index + 5]

        deadline = time.monotonic() + 5
        while not any(os.readlink(f"/proc/{process.pid}/fd/{fd}") == str(lock)
                      for fd in os.listdir(f"/proc/{process.pid}/fd")):
            self.assertLess(time.monotonic(), deadline, "fixture daemon never opened its lock")
            time.sleep(0.01)
        block = listing()
        self.assertEqual(
            block[4],
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
        self.assertTrue(block[4].startswith(f"  stop:       kill {process.pid} "), block)
        # A session whose hosts can't be told is offered no way to stop it.
        unknown = self.below_daemon(
            ["--list-instances"], [f"XDG_RUNTIME_DIR={self.root / 'elsewhere'}"],
            ["XDG_RUNTIME_DIR=relative"])
        self.assertEqual(unknown.returncode, 0, unknown.stderr)
        lines = unknown.stdout.splitlines()
        index = lines.index(next(line for line in lines if f"pid {process.pid}," in line))
        self.assertEqual(
            lines[index + 4], "  stop:       not from here: which daemons host this session can't be told")
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


if __name__ == "__main__":
    unittest.main()
