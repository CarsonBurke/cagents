#!/usr/bin/env python3
"""Contract tests for scripts/native-evidence, with the compositor and Cargo stubbed."""

from __future__ import annotations

import fcntl
import json
import os
import re
import shutil
import stat
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
SCRIPT = REPOSITORY / "scripts" / "native-evidence"
AUTHORITY_NAMES = ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN")

STUBS = {
    "dbus-run-session": """
        #!/bin/sh
        shift
        exec "$@"
    """,
    # Runs its session command like kwin, and records what the session inherited.
    "kwin_wayland": """
        #!/bin/bash
        printf '%s\\n' "$*" >> "$STUB_LOG/kwin"
        ls -l /proc/self/fd > "$STUB_LOG/kwin-fds" 2>&1
        env > "$STUB_LOG/kwin-env"
        [ ! -f "$XDG_CONFIG_HOME/kwinrulesrc" ] || cp "$XDG_CONFIG_HOME/kwinrulesrc" "$STUB_LOG/kwinrules"
        while [ "$#" -gt 0 ]; do
            if [ "$1" = --exit-with-session ]; then session=$2; fi
            shift
        done
        export WAYLAND_DISPLAY=evidence DBUS_SESSION_BUS_ADDRESS=stub
        over_limit() {
            printf 'memory-capped: stopped the native evidence session because of the stub\\n' >&2
            exit 137
        }
        [ "${STUB_OVER_LIMIT:-}" != at-start ] || over_limit
        if [ "${STUB_OVER_LIMIT:-}" = later ]; then
            $session &
            until [ -e "$STUB_LOG/go-over" ]; do sleep 0.1; done
            over_limit
        fi
        $session
    """,
    "niri-harness": """
        #!/bin/bash
        printf '%s\\n' "$*" >> "$STUB_LOG/harness"
        printf '%s\\n' "$PATH" >> "$STUB_LOG/harness-path"
        ls -l /proc/self/fd >> "$STUB_LOG/harness-fds" 2>&1
        if [ -e "$STUB_LOG/stop-wedges" ] && [ "$1 $2" = "session stop" ]; then exec sleep 300; fi
        args=("$@")
        for ((i = 0; i < $#; i++)); do
            case ${args[i]} in
                --root) root=${args[i + 1]} ;;
                --cmd) cmd=${args[i + 1]} ;;
                shot) shot=${args[i + 1]} ;;
                env) printf 'export NESTED_MARKER=nested\\n'; exit 0 ;;
                observe)
                    [ ! -e "$STUB_LOG/ui-absent" ] || exit 0
                    if [ -e "$STUB_LOG/cropped" ]; then
                        printf '{"window":{"size":[900,600]},"output":{"logical":{"width":900,"height":561}},"text":"'
                    else
                        printf '{"window":{"size":[900,600]},"output":{"logical":{"width":1500,"height":900}},"text":"'
                    fi
                    if [ -e "$STUB_LOG/observe-large" ]; then
                        head -c 1000000 /dev/zero | tr '\\0' x
                        [ "${PIPESTATUS[1]}" = 0 ] || exit 1
                    fi
                    printf '"}\\n'
                    exit 0
                    ;;
            esac
        done
        if [ -n "${cmd:-}" ]; then
            [ -z "${STUB_APP_FAILS:-}" ] || exit 3
            mkdir -p "$root"
            sh -c "$cmd" >>"$STUB_LOG/app-output" 2>&1 &
            # The real harness waits for a window that a failed runner never shows.
            [ ! -e "$STUB_LOG/harness-waits" ] || exec sleep 200
            # Nor does it report ready until the app is up, unless told not to wait.
            if [[ " $* " != *" --no-window-wait "* ]]; then
                for _ in {1..300}; do
                    [ ! -e "$STUB_LOG/ui-ran" ] || break
                    sleep 0.1
                done
                [ -e "$STUB_LOG/ui-ran" ] || exit 1
                [ -z "${STUB_DIRTY_AFTER_WINDOW:-}" ] || : > "$STUB_DIRTY_AFTER_WINDOW"
            fi
        fi
        if [ -n "${shot:-}" ]; then
            mkdir -p "$STUB_TASK/niri/shots"
            printf png > "$STUB_TASK/niri/shots/$shot.png"
        fi
    """,
    "magick": """
        #!/bin/bash
        if [ "$1" = identify ]; then printf '1500x900'; exit 0; fi
        cp "$1" "${@: -1}"
    """,
    "rustup": """
        #!/bin/sh
        printf '%s\\n' "$STUB_BIN/cargo"
    """,
    "cargo": """
        #!/bin/bash
        # The gate asks where the build directory is before it takes a slot.
        if [ "$1" = metadata ]; then
            printf '%s\\n' "$*" >> "$STUB_LOG/cargo-metadata"
            printf '{"target_directory":"/stub","workspace_root":"%s"}\\n' "$PWD"
            exit 0
        fi
        printf '%s\\n' "$*" >> "$STUB_LOG/cargo"
        ls -l /proc/self/fd >> "$STUB_LOG/cargo-fds" 2>&1
        [ -z "${STUB_CARGO_FAILS:-}" ] || exit 101
        printf '%s %s\\n' "$CARGO_TARGET_DIR" "$CARGO_BUILD_JOBS" >> "$STUB_LOG/cargo-env"
        printf '%s\\n' "${CAGENTS_BUILD_SEED:-unset}" >> "$STUB_LOG/cargo-seed"
        printf '%s\\n' "${CAGENTS_BUILD_REMOTE_LOOPBACK:-unset}" >> "$STUB_LOG/cargo-loopback"
        mkdir -p "$CARGO_TARGET_DIR/dev-runtime"
        for name in cagents cagentsd cagents-ui; do
            cp "$STUB_BIN/fixture-cli" "$CARGO_TARGET_DIR/dev-runtime/$name"
        done
    """,
    "fixture-cli": """
        #!/bin/bash
        if [ "${0##*/}" = cagents-ui ]; then
            rm -f "$STUB_LOG/ui-absent"
            printf '%s\\n' "$$" > "$STUB_LOG/ui-pid"
            printf '%s\\n' "$*" > "$STUB_LOG/ui-ran"
            exec sleep 300
        fi
        printf '%s\\n' "$*" >> "$STUB_LOG/cli"
        if [ "$*" = '--no-start daemon preflight' ]; then printf 'capability'; exit 0; fi
        if [ "${1:-}" = print-env ]; then env; fi
    """,
}


def write_executable(path: Path, body: str, logs: Path | None = None) -> None:
    # Fixture commands start from an empty environment, so their stubs cannot
    # find the log directory through it.
    text = textwrap.dedent(body).lstrip()
    if logs is not None:
        text = text.replace("$STUB_LOG", str(logs))
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def live_lock_held(path: Path) -> bool:
    with path.open("a") as probe:
        try:
            fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(probe, fcntl.LOCK_UN)
    return False


class Evidence(unittest.TestCase):
    def setUp(self) -> None:
        # Not /tmp, /var/tmp or ~/.cache: sandboxes leave empty .git stubs there,
        # which the script rightly refuses, and this suite does not test that.
        state = Path.home() / ".local" / "state"
        state.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="ne-", dir=state))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.task = self.root / "task"
        self.logs = self.root / "logs"
        self.logs.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in STUBS.items():
            write_executable(self.bin / name, body, self.logs)
        # Installed apart from Cargo: the runner puts this directory first on PATH.
        zig = self.root / "zig-install"
        zig.mkdir()
        write_executable(
            zig / "zig",
            """
            #!/bin/sh
            printf '{\\n  "zig_exe": "%s",\\n  "version": "0.15.2"\\n}\\n' "$0"
            """,
            self.logs,
        )
        self.native_lock = self.root / "native.lock"
        self.cargo_lock = self.root / "cargo.lock"
        self.env = {
            # Under this private HOME, a mise shim would re-execute itself forever.
            "PATH": os.pathsep.join([str(self.bin), str(zig), *(
                entry for entry in os.environ["PATH"].split(os.pathsep)
                if not entry.endswith("/mise/shims")
            )]),
            "HOME": str(self.root / "real-home"),
            "STUB_LOG": str(self.logs),
            "STUB_BIN": str(self.bin),
            "STUB_TASK": str(self.task),
            "CLAUDE_CONFIG_DIR": "/must/not/reach/the/fixture",
            "CAGENTS_EVIDENCE_NATIVE_LOCK": str(self.native_lock),
            "CAGENTS_BUILD_LOCK": str(self.cargo_lock),
            "CAGENTS_BUILD_SLOTS": "1",
            # These fixtures exercise this host's queue; Rust never compiles.
            "CAGENTS_BUILD_REMOTE": "local",
            "CAGENTS_BUILD_INSTALLED_GATE": "",
            # The disk guard must not start the host's own pruner from a fixture.
            "CAGENTS_BUILD_FREE_FLOOR_GIB": "0",
            "CAGENTS_EVIDENCE_TASK": str(self.task),
            "GIT_CONFIG_GLOBAL": "/dev/null",
        }
        self.addCleanup(self.stop_quietly)

    def stop_quietly(self) -> None:
        self.run_tool("stop", check=False)

    def run_tool(self, *args: str, check: bool = True, extra=None,
                 cwd=None) -> subprocess.CompletedProcess:
        env = {**self.env, **(extra or {})}
        result = subprocess.run(
            [str(SCRIPT), *args],
            env=env,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        if check and result.returncode:
            self.fail(f"{args} exited {result.returncode}:\n{result.stdout}\n{result.stderr}")
        return result

    def checkout(self, name: str = "after") -> Path:
        path = self.root / name
        (path / "scripts").mkdir(parents=True)
        (path / "Cargo.lock").write_text('[[package]]\nname = "cagents"\nversion = "0.1.0"\n')
        write_executable(
            path / "scripts" / "run-development",
            """
            #!/bin/bash
            printf '%s\\n' "$*" >> "$STUB_LOG/runner"
            env >> "$STUB_LOG/runner-env"
            [ ! -e "$STUB_LOG/runner-checks" ] || cargo check --locked
            [ ! -e "$STUB_LOG/runner-exits" ] || exit 7
            common="--locked --profile dev-runtime --no-default-features --features"
            [ ! -e "$STUB_LOG/runner-other-features" ] || cargo build $common other --bin cagents-ui
            GATE_PLACEHOLDER build $common development --bin cagents
            GATE_PLACEHOLDER build $common development,host --bin cagentsd
            GATE_PLACEHOLDER build $common development,ui --bin cagents-ui
            exec cargo run --quiet $common development,ui --bin cagents-ui -- --app-argument
            """,
            self.logs,
        )
        runner = path / "scripts" / "run-development"
        runner.write_text(runner.read_text().replace(
            "GATE_PLACEHOLDER", str(REPOSITORY / "scripts" / "cargo-gate")
        ))
        git = ["git", "-C", str(path), "-c", "user.name=t", "-c", "user.email=t@example.invalid"]
        subprocess.run(["git", "init", "-q", str(path)], check=True, env=self.env)
        subprocess.run([*git, "add", "."], check=True, env=self.env)
        subprocess.run([*git, "commit", "-qm", name], check=True, env=self.env)
        return path

    def built(self, side: str = "after") -> Path:
        path = self.checkout(side)
        self.run_tool("build", side, str(path))
        return path

    def lock_is_free(self, path: Path) -> bool:
        with open(path, "a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(lock, fcntl.LOCK_UN)
            return True

    def test_narrow_viewport_keeps_its_explicit_height(self):
        self.built("after")
        self.run_tool("start", "after", "--width", "900", "--height", "800")
        rules = (self.logs / "kwinrules").read_text()
        self.assertIn("size=900,800", rules)
        self.assertIn("sizerule=2", rules)
        self.assertIn("--width 900 --height 800", (self.logs / "kwin").read_text())
        self.run_tool("stop")

    def test_shot_refuses_a_window_cropped_by_its_viewport(self):
        self.built("after")
        self.run_tool("start", "after", "--width", "900")
        (self.logs / "cropped").touch()
        result = self.run_tool("shot", "cropped", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("900x600 exceeds viewport 900x561", result.stderr)
        self.assertFalse((self.task / "shots/after-cropped.webp").exists())
        self.run_tool("stop")

    def test_a_session_has_a_memory_limit_made_before_its_private_environment(self):
        source = (REPOSITORY / "scripts/native-evidence").read_text()
        self.assertIn(
            """setpriv --pdeathsig TERM "$tool_root/scripts/memory-capped" --name 'the native evidence session' env -u DISPLAY""",
            source)

    def test_a_session_over_its_limit_at_start_says_so(self):
        self.built()
        result = self.run_tool("start", "after", check=False, extra={"STUB_OVER_LIMIT": "at-start"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("memory-capped: stopped the native evidence session", result.stderr)
        self.assertNotIn("the compositor exited", result.stderr)

    def test_a_session_over_its_limit_later_says_so_to_whoever_comes_next(self):
        self.built()
        self.run_tool("start", "after", extra={"STUB_OVER_LIMIT": "later"})
        (self.logs / "go-over").touch()
        for _ in range(100):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))
        notice = "memory-capped: stopped the native evidence session"
        self.assertIn(f"last session: {notice}", self.run_tool("status").stdout)
        driven = self.run_tool("drive", "observe", check=False)
        self.assertNotEqual(driven.returncode, 0)
        self.assertIn(notice, driven.stderr)
        # The next session starts clean.
        self.run_tool("start", "after")
        self.assertNotIn(notice, self.run_tool("status").stdout)
        self.run_tool("stop")

    def test_inherited_authority_is_refused_and_never_removed(self) -> None:
        for name in AUTHORITY_NAMES:
            for value in ("", "token"):
                with self.subTest(name=name, value=value):
                    result = self.run_tool("status", check=False, extra={name: value})
                    self.assertEqual(result.returncode, 77)
                    self.assertIn("genuine User shell", result.stderr)
                    self.assertFalse(self.task.exists())
        source = SCRIPT.read_text()
        for name in AUTHORITY_NAMES:
            self.assertNotRegex(source, rf"(unset|env -u|-u)\s+{name}")

    def test_task_directory_must_be_outside_git_ancestry(self) -> None:
        (self.root / ".git").mkdir()
        result = self.run_tool("status", check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("outside Git ancestry", result.stderr)

    def test_build_refuses_a_zig_other_than_the_checkouts_pin(self) -> None:
        checkout = self.checkout("pinned")
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.0.1"\n')
        result = self.run_tool("build", "after", str(checkout), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pins Zig 0.0.1", result.stderr)

    def zig_behind_a_shim(self, checkout: Path) -> Path:
        """mise's layout, whose shim would loop under a moved HOME: this stand-in fails instead."""
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        data = self.root / "elsewhere" / "mise"
        (data / "shims").mkdir(parents=True)
        write_executable(data / "mise", "#!/bin/sh\nexit 99\n", self.logs)
        (data / "shims" / "zig").symlink_to(data / "mise")
        return data

    def test_build_goes_past_a_mise_shim_to_the_pinned_zig(self) -> None:
        checkout = self.checkout("shimmed")
        data = self.zig_behind_a_shim(checkout)
        install = data / "installs" / "zig" / "0.15.2"
        install.mkdir(parents=True)
        (install / "zig").symlink_to(self.root / "zig-install" / "zig")
        path = os.pathsep.join([str(data / "shims"), self.env["PATH"]])
        self.run_tool("build", "after", str(checkout), extra={"PATH": path})
        self.assertNotIn(str(data / "shims"),
                         (self.task / "target-after" / "evidence-source").read_text())

    def test_build_says_when_the_pinned_zig_is_not_installed(self) -> None:
        checkout = self.checkout("shimmed")
        data = self.zig_behind_a_shim(checkout)
        path = os.pathsep.join([str(data / "shims"), self.env["PATH"]])
        result = self.run_tool("build", "after", str(checkout), check=False, extra={"PATH": path})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pinned Zig 0.15.2 is not installed", result.stderr)

    def test_build_holds_only_a_build_slot_and_sets_a_private_target(self) -> None:
        self.built()
        target = self.task / "target-after"
        self.assertTrue(all(
            line.startswith(str(target))
            for line in (self.logs / "cargo-env").read_text().splitlines()
        ))
        # The gate would otherwise seed the build directory of whatever checkout it's run in.
        self.assertEqual(set((self.logs / "cargo-seed").read_text().split()), {"0"})
        self.assertTrue(self.lock_is_free(self.cargo_lock))
        self.assertFalse(self.native_lock.exists())
        # A build outlives nothing, but a compiler daemon must not keep the slot.
        self.assertNotIn(str(self.cargo_lock), (self.logs / "cargo-fds").read_text())

    def test_start_refuses_a_checkout_changed_since_its_build(self) -> None:
        checkout = self.built()
        (checkout / "edited").write_text("new")
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("changed since its build", result.stderr)
        self.assertFalse((self.logs / "kwin").exists())
        (checkout / "edited").unlink()
        self.run_tool("start", "after")

    def test_start_notices_a_staged_edit(self) -> None:
        checkout = self.built()
        (checkout / "staged").write_text("one")
        subprocess.run(["git", "-C", str(checkout), "add", "staged"], check=True, env=self.env)
        self.run_tool("build", "after", str(checkout))
        (checkout / "staged").write_text("two")
        subprocess.run(["git", "-C", str(checkout), "add", "staged"], check=True, env=self.env)
        self.assertIn("changed since its build", self.run_tool("start", "after", check=False).stderr)

    def test_a_failed_build_leaves_no_record_from_its_last_build(self) -> None:
        after = self.checkout("after")
        failed = self.run_tool("build", "after", str(after), check=False,
                               extra={"STUB_CARGO_FAILS": "1"})
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("has not been built", self.run_tool("start", "after", check=False).stderr)
        self.run_tool("build", "after", str(after))
        self.assertNotEqual(self.run_tool("build", "after", str(after), check=False,
                                          extra={"STUB_CARGO_FAILS": "1"}).returncode, 0)
        self.assertIn("has not been built", self.run_tool("start", "after", check=False).stderr)

    def test_a_running_side_cannot_be_rebuilt_under_its_evidence(self) -> None:
        checkout = self.built()
        self.run_tool("start", "after")
        result = self.run_tool("build", "after", str(checkout), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("is running", result.stderr)

    def test_an_edit_during_the_lock_wait_stops_the_runner(self) -> None:
        checkout = self.built()
        with open(self.native_lock, "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            start = subprocess.Popen(
                [str(SCRIPT), "start", "after", "--wait-lock", "30"],
                env=self.env, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            time.sleep(2)
            (checkout / "edited-while-queued").write_text("new")
            fcntl.flock(lock, fcntl.LOCK_UN)
            _, stderr = start.communicate(timeout=40)
        self.assertEqual(start.returncode, 1)
        self.assertIn("changed", stderr)
        self.assertFalse((self.logs / "runner").exists())

    def test_stop_ends_a_session_still_waiting_for_the_lock(self) -> None:
        self.built()
        with open(self.native_lock, "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            start = subprocess.Popen(
                [str(SCRIPT), "start", "after", "--wait-lock", "300"],
                env=self.env, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            time.sleep(2)
            self.run_tool("stop")
            _, stderr = start.communicate(timeout=30)
        self.assertEqual(start.returncode, 1)
        self.assertIn("stopped while waiting", stderr)
        self.assertFalse((self.logs / "kwin").exists())

    def assert_fails_promptly(self, message: str) -> None:
        started = time.monotonic()
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn(message, result.stderr)
        for _ in range(100):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))
        # Well under the 180 seconds the harness would wait for a window.
        self.assertLess(time.monotonic() - started, 60)

    def test_a_session_missing_an_executable_refuses_to_compile_and_releases(self) -> None:
        self.built()
        (self.task / "target-after" / "dev-runtime" / "cagents-ui").unlink()
        (self.logs / "harness-waits").touch()
        self.assert_fails_promptly("refusing to compile cagents-ui while holding the native lock")

    def test_a_session_refuses_any_other_cargo_command_and_releases(self) -> None:
        self.built()
        (self.logs / "runner-checks").touch()
        (self.logs / "harness-waits").touch()
        self.assert_fails_promptly("refusing to run cargo check while holding the native lock")

    def test_a_session_refuses_features_that_build_did_not_compile(self) -> None:
        self.built()
        (self.logs / "runner-other-features").touch()
        (self.logs / "harness-waits").touch()
        self.assert_fails_promptly("build did not compile that")

    def test_builds_use_the_build_hosts_shared_slots(self) -> None:
        self.built()
        # These fixtures build locally; on the build host the gate leases a slot.
        self.assertEqual(set((self.logs / "cargo-loopback").read_text().split()), {"1"})
        # Nothing copies a seed into the task any more, and saying so is enough.
        before = self.checkout("before")
        result = self.run_tool("build", "before", str(before), "--seed-target",
                               str(self.task / "target-after"))
        self.assertIn("ignoring --seed-target", result.stderr)
        self.assertEqual(sorted(path.name for path in self.task.glob("target-*")),
                         ["target-after", "target-before"])
        self.assertFalse((self.task / "target-before/debug").exists())

    def test_a_build_with_too_little_disk_is_refused_before_it_starts(self) -> None:
        # Slot builds skip the gate's own disk guard, so the tool runs it.
        checkout = self.checkout()
        result = self.run_tool("build", "after", str(checkout), check=False, extra={
            "CAGENTS_BUILD_FREE_FLOOR_GIB": "999999999", "CAGENTS_BUILD_PRUNE": "true"})
        self.assertEqual(result.returncode, 2)
        self.assertIn("refusing to build", result.stderr)
        self.assertFalse((self.logs / "cargo").exists())

    def test_a_build_on_another_host_leaves_this_disk_to_the_gate(self) -> None:
        # Only its executables come back here, so a full disk here is no
        # reason to refuse it; the build host's own runs guard its disk.
        checkout = self.checkout()
        started = time.monotonic()
        result = self.run_tool("build", "after", str(checkout), check=False, extra={
            "CAGENTS_BUILD_REMOTE": "build-host.invalid",
            "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "1",
            "CAGENTS_BUILD_FREE_FLOOR_GIB": "999999999", "CAGENTS_BUILD_PRUNE": "true"})
        self.assertNotIn("refusing to build", result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 60)

    def test_its_builds_and_sessions_keep_their_target_from_the_pruner(self) -> None:
        self.built()
        target = self.task / "target-after"
        marker = target / ".native-evidence-used"
        # A build holds the target open, but its Cargo does not: a compiler
        # daemon that outlived the build would keep it from the pruner forever.
        self.assertNotIn(str(target), (self.logs / "cargo-fds").read_text())
        started = time.time() - 1
        self.run_tool("start", "after")
        self.assertGreater(marker.stat().st_mtime, started)
        old = time.time() - 86400
        pid = (self.task / "session/pid").read_text().strip()
        self.assertIn(str(target), [os.readlink(f"/proc/{pid}/fd/{fd}")
                                    for fd in os.listdir(f"/proc/{pid}/fd")])
        # Its errors still reach the log that start names.
        self.assertEqual(os.readlink(f"/proc/{pid}/fd/2"), str(self.task / "logs/session-after.log"))
        # Only the supervisor holds it: a process the session leaves behind must not.
        self.assertNotIn(str(target), (self.logs / "kwin-fds").read_text())
        os.utime(marker, (old, old))
        self.run_tool("stop")
        # Unused from when the session ended.
        self.assertGreater(marker.stat().st_mtime, old)

    def test_a_target_pruned_before_its_session_starts_says_to_build_again(self) -> None:
        checkout = self.built()
        shutil.rmtree(self.task / "target-after")
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("after has not been built, or its unused build was reclaimed; "
                      "run: build after CHECKOUT", result.stderr)
        self.run_tool("build", "after", str(checkout))
        self.run_tool("start", "after")

    def test_a_runner_that_exits_fails_at_once_and_releases(self) -> None:
        self.built()
        (self.logs / "runner-exits").touch()
        (self.logs / "harness-waits").touch()
        self.assert_fails_promptly("the runner exited with status 7")
        # Where its stderr went: the log the app writes may not exist yet.
        failure = next(self.task.rglob("failed")).read_text()
        self.assertIn(f"{self.task}/niri/session.log", failure)

    def test_a_session_runs_no_cargo_and_never_joins_the_shared_queue(self) -> None:
        self.built()
        before = (self.logs / "cargo").read_text()
        with self.cargo_lock.open("a") as shared:
            fcntl.flock(shared, fcntl.LOCK_EX)
            # The runner goes through the real gate while the shared slot is busy.
            self.run_tool("start", "after")
        env = (self.logs / "runner-env").read_text()
        lock = next(l.split("=", 1)[1] for l in env.splitlines() if l.startswith("CAGENTS_BUILD_LOCK="))
        self.assertTrue(lock.startswith(str(self.task)), lock)
        self.assertIn("CAGENTS_BUILD_SEED=0", env.splitlines())
        self.assertEqual((self.logs / "cargo").read_text(), before)
        # The stub runner execs this last, after its gated builds.
        for _ in range(100):
            if (self.logs / "ui-ran").exists():
                break
            time.sleep(0.1)
        self.assertTrue((self.logs / "ui-ran").exists(), "the recorded executable did not start")
        # After the window, cargo is real, so nothing in the app can end the session.
        session_cargo = next(self.task.rglob("session-bin/cargo"))
        probe = subprocess.run(
            [str(session_cargo), "--version"],
            capture_output=True,
            env={**self.env, "CARGO_TARGET_DIR": str(self.root / "probe-target")},
        )
        self.assertEqual(probe.returncode, 0, probe.stderr)
        self.assertIn("--version", (self.logs / "cargo").read_text())
        self.assertIn("session", self.run_tool("status").stdout)
        self.assertTrue(live_lock_held(self.native_lock))
        self.run_tool("stop")

    def test_the_repositorys_own_runner_starts_through_the_session_cargo(self) -> None:
        checkout = self.checkout("after")
        shutil.copytree(REPOSITORY / "scripts", checkout / "scripts", dirs_exist_ok=True)
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        git = ["git", "-C", str(checkout), "-c", "user.name=t", "-c", "user.email=t@example.invalid"]
        subprocess.run([*git, "add", "."], check=True, env=self.env)
        subprocess.run([*git, "commit", "-qm", "real runner"], check=True, env=self.env)
        self.run_tool("build", "after", str(checkout))
        built = (self.logs / "cargo").read_text()
        metadata = (self.logs / "cargo-metadata").read_text()
        started = self.run_tool("start", "after", check=False)
        self.assertEqual(started.returncode, 0, (self.logs / "app-output").read_text())
        for _ in range(100):
            if (self.logs / "ui-ran").exists():
                break
            time.sleep(0.1)
        self.assertTrue((self.logs / "ui-ran").exists(), "the runner did not start the recorded UI")
        self.assertEqual((self.logs / "cargo").read_text(), built)
        self.assertEqual((self.logs / "cargo-metadata").read_text(), metadata)
        self.run_tool("stop")

    def test_a_refusals_next_step_works_straight_away(self) -> None:
        path = self.built()
        (self.logs / "runner-checks").touch()
        (self.logs / "harness-waits").touch()
        self.assertEqual(self.run_tool("start", "after", check=False).returncode, 1)
        (self.logs / "runner-checks").unlink()
        (self.logs / "harness-waits").unlink()
        self.run_tool("build", "after", str(path))
        self.run_tool("start", "after")
        self.run_tool("stop")

    def test_a_checkout_that_changes_while_the_session_starts_is_refused(self) -> None:
        path = self.built()
        result = self.run_tool(
            "start", "after", check=False, extra={"STUB_DIRTY_AFTER_WINDOW": str(path / "edited")}
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("changed while the session was starting", result.stderr)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_a_session_is_ready_only_once_the_harness_saw_a_window(self) -> None:
        self.built()
        self.run_tool("start", "after")
        start = next(line for line in (self.logs / "harness").read_text().splitlines() if "--cmd" in line)
        self.assertNotIn("--no-window-wait", start)
        self.run_tool("stop")

    def test_shot_does_not_mistake_long_observe_output_for_no_window(self) -> None:
        self.built()
        (self.logs / "observe-large").touch()
        self.run_tool("start", "after")
        self.run_tool("shot", "large")
        self.assertIn("observe --no-text", (self.logs / "harness").read_text())
        self.run_tool("stop")

    def test_a_cargo_beside_zig_is_refused_before_it_can_shadow_the_session(self) -> None:
        path = self.checkout("after")
        write_executable(self.root / "zig-install" / "cargo", "#!/bin/sh\n", self.logs)
        result = self.run_tool("build", "after", str(path), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("cargo beside it", result.stderr)
        self.assertFalse((self.logs / "cargo").exists())

    def test_the_callers_desktop_settings_do_not_reach_the_session(self) -> None:
        self.built()
        names = ("XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "XCURSOR_THEME", "XCURSOR_SIZE")
        self.run_tool("start", "after", extra=dict.fromkeys(names, "caller"))
        env = (self.logs / "runner-env").read_text()
        for name in names:
            self.assertNotIn(f"{name}=caller", env)
        self.run_tool("stop")

    def test_a_tilde_left_by_fish_means_home(self) -> None:
        home = self.root / "h"
        home.mkdir()
        self.run_tool("status", extra={"CAGENTS_EVIDENCE_TASK": "~/t", "HOME": str(home)})
        self.assertTrue((home / "t").is_dir())

    def test_a_refused_task_path_leaves_nothing_behind(self) -> None:
        cwd = self.root / "cwd"
        cwd.mkdir()
        for task in ("~other/x", "./~x", "has space/x"):
            result = subprocess.run(
                [str(SCRIPT), "status"],
                env={**self.env, "CAGENTS_EVIDENCE_TASK": task},
                cwd=cwd,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2, task)
        self.assertEqual(list(cwd.iterdir()), [])

    def test_a_clear_that_ends_on_the_timeout_says_it_timed_out(self) -> None:
        self.built()
        write_executable(
            self.bin / "rm",
            """
            #!/bin/bash
            [ "$1" = -rf ] || exec /usr/bin/rm "$@"
            exec sleep 300
            """,
            self.logs,
        )
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("still going after 30 seconds", result.stderr)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_a_wedged_clear_gives_up_and_does_not_hold_the_lock(self) -> None:
        self.built()
        write_executable(
            self.bin / "rm",
            """
            #!/bin/bash
            [ "$1" = -rf ] || exec /usr/bin/rm "$@"
            ls -l /proc/self/fd > "$STUB_LOG/rm-fds" 2>&1
            # Ignores TERM, so the clear's timeout has to kill it.
            trap '' TERM
            exec sleep 300
            """,
            self.logs,
        )
        write_executable(
            self.bin / "awk",
            """
            #!/bin/bash
            ls -l /proc/self/fd > "$STUB_LOG/awk-fds" 2>&1
            exec /usr/bin/awk "$@"
            """,
            self.logs,
        )
        # The mount scan runs only over a runtime left by an earlier session.
        (self.task / "rt").mkdir(exist_ok=True)
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("still going after 30 seconds", result.stderr)
        self.assertNotIn(str(self.native_lock), (self.logs / "rm-fds").read_text())
        self.assertNotIn(str(self.native_lock), (self.logs / "awk-fds").read_text())
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_a_runtime_it_cannot_clear_fails_with_the_reason(self) -> None:
        self.built()
        stuck = self.task / "rt" / "stuck"
        stuck.mkdir(parents=True)
        (stuck / "file").touch()
        stuck.chmod(0o500)
        self.addCleanup(stuck.chmod, 0o700)
        result = self.run_tool("start", "after", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("could not clear the last session", result.stderr)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_a_build_queued_before_a_session_starts_does_not_run_under_it(self) -> None:
        self.built()
        before = (self.logs / "cargo").read_text()
        with self.cargo_lock.open("a") as shared:
            fcntl.flock(shared, fcntl.LOCK_EX)
            queued = subprocess.Popen(
                [str(SCRIPT), "build", "after", str(self.root / "after")],
                env=self.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            )
            time.sleep(1)
            self.run_tool("start", "after")
        output, _ = queued.communicate(timeout=30)
        self.assertNotEqual(queued.returncode, 0)
        self.assertIn("after is running", output)
        self.assertEqual((self.logs / "cargo").read_text(), before)
        self.run_tool("stop")

    def test_a_wedged_harness_stop_cannot_keep_the_lock(self) -> None:
        self.built()
        self.run_tool("start", "after")
        (self.logs / "stop-wedges").touch()
        self.run_tool("stop", check=False)
        for _ in range(350):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_the_harness_keeps_no_lock_and_avoids_mise_shims(self) -> None:
        self.built()
        shims = self.root / "mise" / "shims"
        shims.mkdir(parents=True)
        self.run_tool("start", "after", extra={"PATH": f"{shims}:{self.env['PATH']}"})
        self.run_tool("stop")
        self.assertNotIn(str(shims), (self.logs / "harness-path").read_text())
        self.assertNotIn(str(self.native_lock), (self.logs / "harness-fds").read_text())

    def test_a_directory_with_other_contents_is_not_adopted(self) -> None:
        self.task.mkdir()
        (self.task / "unrelated").write_text("keep")
        result = self.run_tool("status", check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("not a native-evidence task", result.stderr)

    def test_compositor_and_fixture_do_not_share_the_callers_environment(self) -> None:
        self.built()
        extra = {
            "GH_TOKEN": "secret",
            "SSH_AUTH_SOCK": "/real/agent",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/real/bus",
            "NIRI_SOCKET": "/real/niri",
        }
        self.env.update(extra)
        self.run_tool("start", "after")
        compositor = dict(
            line.split("=", 1) for line in (self.logs / "kwin-env").read_text().splitlines() if "=" in line
        )
        home = self.task / "compositor"
        self.assertEqual(compositor["HOME"], f"{home}/home")
        self.assertEqual(compositor["XDG_CONFIG_HOME"], f"{home}/config")
        self.assertEqual(compositor["XDG_DATA_HOME"], f"{home}/data")
        self.assertNotIn("NIRI_SOCKET", compositor)
        fixture = self.run_tool("cli", "print-env").stdout
        for name in (*extra, "STUB_LOG", "CAGENTS_EVIDENCE_TASK"):
            self.assertNotIn(f"{name}=", fixture)
        self.assertNotIn(str(self.root / "real-home") + "/.local", fixture)
        self.assertIn(f"CARGO_HOME={self.root}/real-home/.cargo", fixture)
        for _ in range(100):
            if (self.logs / "runner-env").exists():
                break
            time.sleep(0.05)
        runner = (self.logs / "runner-env").read_text()
        self.assertIn("DBUS_SESSION_BUS_ADDRESS=stub\n", runner)
        self.assertIn("WAYLAND_DISPLAY=evidence\n", runner)
        self.assertIn(f"CARGO_HOME={self.root}/real-home/.cargo\n", runner)

    def test_relaunch_keeps_the_session_and_replays_only_the_built_products(self) -> None:
        self.built()
        self.run_tool("start", "after")
        original_pid = int((self.logs / "ui-pid").read_text())
        refused = self.run_tool("relaunch", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("close the app window", refused.stderr)
        os.kill(original_pid, 15)
        (self.task / "niri" / "session.rc").write_text("0")
        (self.logs / "ui-absent").touch()
        cargo_before = (self.logs / "cargo").read_text()
        identity = (self.task / "fixture" / "home" / ".claude").stat().st_ino
        output = self.logs / "relaunch-output"
        with output.open("w") as sink:
            process = subprocess.Popen([str(SCRIPT), "relaunch", "--chat-stages"], env=self.env,
                stdin=subprocess.DEVNULL, stdout=sink, stderr=sink)
        try:
            for _ in range(200):
                current = int((self.logs / "ui-pid").read_text())
                if current != original_pid and not (self.task / "session" / "replaying").exists():
                    break
                if process.poll() is not None:
                    self.fail(output.read_text())
                time.sleep(0.05)
            else:
                self.fail("relaunched UI did not appear: " + output.read_text())
            self.assertFalse(self.lock_is_free(self.native_lock))
            self.assertEqual((self.logs / "cargo").read_text(), cargo_before)
            self.assertEqual((self.task / "fixture" / "home" / ".claude").stat().st_ino, identity)
            self.assertEqual(len((self.logs / "runner").read_text().splitlines()), 2)
            self.assertIn(f"CAGENTS_CHAT_STAGE_OUTPUT={self.task}/logs/chat-stages/after-", (self.logs / "runner-env").read_text())
            self.assertEqual(stat.S_IMODE((self.task / "logs" / "chat-stages").stat().st_mode), 0o700)
            self.run_tool("stop")
            process.wait(timeout=20)
            self.assertTrue(self.lock_is_free(self.native_lock))
            self.assertFalse((self.task / "session" / "replaying").exists())
        finally:
            if process.poll() is None:
                self.run_tool("stop", check=False)
                process.terminate()
                process.wait(timeout=20)

    def test_a_sessions_builds_never_start_the_pruner(self) -> None:
        # Its runner's builds replay what `build` made. Their disk guard would
        # otherwise start the account's pruner, under the fixture's HOME,
        # while the app is being captured.
        self.built()
        self.run_tool("start", "after", extra={"CAGENTS_BUILD_FREE_FLOOR_GIB": "7"})
        for _ in range(100):
            if (self.logs / "runner-env").exists():
                break
            time.sleep(0.05)
        runner = (self.logs / "runner-env").read_text()
        self.assertIn("CAGENTS_BUILD_FREE_FLOOR_GIB=0\n", runner)
        # Only the runner's: the fixture's other commands keep their guard.
        self.assertNotIn("CAGENTS_BUILD_FREE_FLOOR_GIB=", self.run_tool("cli", "print-env").stdout)
        self.run_tool("stop")

    def killed_supervisor_processes(self, signal: int, extra: dict | None = None) -> None:
        self.built()
        self.run_tool("start", "after", extra=extra)
        for _ in range(100):
            if (self.logs / "runner").exists():
                break
            time.sleep(0.05)
        os.kill(int((self.task / "session" / "pid").read_text()), signal)
        for _ in range(100):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def fixture_process_count(self) -> int:
        runtime = f"XDG_RUNTIME_DIR={self.task}/rt".encode()
        count = 0
        for entry in Path("/proc").glob("[0-9]*/environ"):
            try:
                count += runtime in entry.read_bytes().split(b"\0")
            except OSError:
                pass
        return count

    def test_terminating_the_supervisor_ends_the_whole_session(self) -> None:
        self.killed_supervisor_processes(15)
        self.assertEqual(self.fixture_process_count(), 0)
        self.assertIn("none", self.run_tool("status").stdout)

    def test_a_killed_supervisor_takes_the_session_with_it_without_stop(self) -> None:
        # Nothing else enforces --limit once the supervisor is gone. This needs
        # the session's own scope, which an outer memory limit or no user manager
        # rules out.
        own_group = subprocess.run(
            [str(REPOSITORY / "scripts/memory-capped"), "cat", "/proc/self/cgroup"],
            env=self.env, capture_output=True, text=True, timeout=60).stdout
        if "cagents-capped-" not in own_group:
            self.skipTest("a session can't have a scope of its own here")
        self.killed_supervisor_processes(9)
        for _ in range(100):
            if self.fixture_process_count() == 0:
                break
            time.sleep(0.2)
        self.assertEqual(self.fixture_process_count(), 0)

    def test_stop_ends_what_a_killed_supervisor_left(self) -> None:
        # With a limit, the session's scope goes with its supervisor and leaves nothing.
        self.killed_supervisor_processes(9, extra={"CAGENTS_MEMORY_MAX": "0"})
        self.assertIn("lost", self.run_tool("status").stdout)
        self.assertIn("lost session", self.run_tool("stop").stdout)
        self.assertEqual(self.fixture_process_count(), 0)

    def test_a_portal_runs_inside_the_session_only_when_asked(self) -> None:
        self.built()
        self.run_tool("start", "after")
        self.run_tool("stop")
        self.run_tool("start", "after", "--portal", "gtk")
        self.run_tool("stop")
        starts = [line for line in (self.logs / "harness").read_text().splitlines()
                  if line.startswith("session start")]
        self.assertEqual(len(starts), 2, starts)
        self.assertNotIn("--portal", starts[0])
        self.assertIn("--portal gtk", starts[1])

    def test_an_unknown_portal_is_refused_before_the_lock(self) -> None:
        self.built()
        result = self.run_tool("start", "after", "--portal", "xdg", check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown portal backend: xdg; use gtk, kde, gnome or cosmic", result.stderr)
        self.assertTrue(self.lock_is_free(self.native_lock))

    def test_start_reports_a_busy_lock_without_starting(self) -> None:
        self.built()
        with open(self.native_lock, "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = self.run_tool("start", "after", "--wait-lock", "0", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("native lock stayed busy", result.stderr)
        self.assertIn(f"scripts/lock-holders {self.native_lock}", result.stderr)
        self.assertFalse((self.logs / "kwin").exists())

    def test_session_lifecycle_holds_the_lock_isolates_state_and_ends_its_processes(self) -> None:
        checkout = self.built()
        self.run_tool("start", "after", "--width", "900")
        self.assertFalse(self.lock_is_free(self.native_lock))
        self.assertNotIn(str(self.native_lock), (self.logs / "kwin-fds").read_text())

        environment = self.run_tool("cli", "print-env").stdout
        fixture = self.task / "fixture"
        self.assertIn(f"HOME={fixture}/home\n", environment)
        self.assertIn(f"XDG_RUNTIME_DIR={self.task}/rt\n", environment)
        self.assertNotIn("CLAUDE_CONFIG_DIR", environment)
        self.assertTrue((fixture / "bin" / "claude").is_file())
        self.assertFalse((fixture / "bin" / "claude").is_symlink())
        self.assertEqual(
            subprocess.run(["git", "-C", str(fixture / "project"), "rev-list", "--count", "HEAD"],
                           capture_output=True, text=True, env=self.env).stdout.strip(),
            "1",
        )
        for _ in range(100):
            if (self.logs / "runner").exists():
                break
            time.sleep(0.05)
        self.assertIn("--browserless", (self.logs / "runner").read_text())
        self.assertIn(f"XDG_DATA_HOME={fixture}/data", (self.logs / "runner-env").read_text())

        self.assertEqual(self.run_tool("nested", "printenv", "NESTED_MARKER").stdout.strip(), "nested")
        self.run_tool("drive", "observe")
        self.assertIn("--session evidence observe", (self.logs / "harness").read_text())

        path = self.run_tool("shot", "empty-state").stdout.strip()
        self.assertEqual(path, str(self.task / "shots" / "after-empty-state.webp"))
        head = subprocess.run(["git", "-C", str(checkout), "rev-parse", "HEAD"],
                              capture_output=True, text=True, env=self.env).stdout.strip()
        manifest = (self.task / "shots" / "manifest.tsv").read_text().strip().split("\t")
        self.assertEqual(manifest[0], "after-empty-state.webp")
        self.assertEqual(manifest[1], head)
        self.assertEqual(manifest[3:], ["900", "1500x900"])
        self.assertEqual(self.run_tool("shot", "bad/name", check=False).returncode, 2)
        self.run_tool("shot", "empty-state")
        self.assertEqual(len((self.task / "shots" / "manifest.tsv").read_text().splitlines()), 1)

        second = self.run_tool("start", "after", check=False)
        self.assertEqual(second.returncode, 1)
        self.assertIn("already running", second.stderr)

        leftover = subprocess.Popen(
            ["sleep", "300"],
            env={**self.env, "XDG_RUNTIME_DIR": str(self.task / "rt")},
        )
        unrelated = subprocess.Popen(
            ["sleep", "300"],
            env={**self.env, "XDG_RUNTIME_DIR": str(self.task / "rt-other")},
        )
        self.addCleanup(unrelated.wait)
        self.addCleanup(unrelated.kill)

        self.assertEqual(self.run_tool("stop").stdout.strip().splitlines()[-1], "stopped")
        self.assertTrue(self.lock_is_free(self.native_lock))
        self.assertEqual(leftover.wait(timeout=10), -15)
        self.assertIsNone(unrelated.poll())
        self.assertIn("daemon stop --confirm capability", (self.logs / "cli").read_text())
        self.assertEqual(self.run_tool("cli", "print-env", check=False).returncode, 1)

    def test_seed_restores_identical_data_for_each_side(self) -> None:
        self.built("after")
        self.built("before")
        self.run_tool("start", "before")
        fixture = self.task / "fixture"
        (fixture / "home" / "seeded").write_text("data")
        self.assertEqual(self.run_tool("save-seed", check=False).returncode, 1)
        self.run_tool("stop")
        self.run_tool("save-seed")

        self.run_tool("start", "after")
        (fixture / "home" / "changed-during-after").write_text("data")
        self.run_tool("stop")

        self.run_tool("start", "before", "--keep-state")
        self.assertTrue((fixture / "home" / "changed-during-after").exists())
        self.run_tool("stop")

        self.run_tool("start", "before")
        self.assertTrue((fixture / "home" / "seeded").exists())
        self.assertFalse((fixture / "home" / "changed-during-after").exists())

    def test_a_seeded_chat_keeps_the_provider_home_it_recorded(self) -> None:
        # A chat records its provider home's device and inode and refuses to
        # reopen once that directory is replaced, so restoring a seed must
        # keep the directories, not recreate them.
        self.built("after")
        self.built("before")
        self.run_tool("start", "after")
        fixture = self.task / "fixture"
        provider_home = fixture / "home" / ".claude"
        recorded = provider_home.stat()
        (provider_home / "session.jsonl").write_text("seeded\n")
        self.run_tool("stop")
        self.run_tool("save-seed")
        session_file = (provider_home / "session.jsonl").stat().st_ino

        self.run_tool("start", "before")
        (provider_home / "session.jsonl").write_text("changed by before\n")
        (provider_home / "left-by-before").write_text("data")
        self.run_tool("stop")
        result = self.run_tool("start", "after")
        restored = provider_home.stat()
        self.assertEqual((restored.st_dev, restored.st_ino), (recorded.st_dev, recorded.st_ino))
        self.assertEqual((provider_home / "session.jsonl").read_text(), "seeded\n")
        self.assertFalse((provider_home / "left-by-before").exists())
        self.assertNotIn("will not reopen", result.stderr)
        self.run_tool("stop")

        # A transcript keeps its file, which its location records.
        self.assertEqual((provider_home / "session.jsonl").stat().st_ino, session_file)

        # A fixture removed meanwhile can't keep them, and every start says so
        # until the seed is saved again.
        shutil.rmtree(fixture)
        for _ in range(2):
            result = self.run_tool("start", "before")
            self.assertIn("the provider home is not the one the seed was saved from", result.stderr)
            self.assertEqual((provider_home / "session.jsonl").read_text(), "seeded\n")
            self.run_tool("stop")
        self.run_tool("save-seed")
        self.assertNotIn("will not reopen", self.run_tool("start", "after").stderr)

    def test_a_seed_saved_before_its_home_was_recorded_is_said_not_to_reopen(self) -> None:
        self.built("after")
        self.run_tool("start", "after")
        self.run_tool("stop")
        self.run_tool("save-seed")
        (self.task / "fixture-seed.home").unlink()
        result = self.run_tool("start", "after")
        self.assertIn("chats in the seed will not reopen", result.stderr)

    def fixture_shell(self, script: str, check: bool = True) -> subprocess.CompletedProcess:
        """Runs a command as the app would, in the fixture's environment and project."""
        return self.run_tool("env", "sh", "-c", f'cd "$CAGENTS_EVIDENCE_FIXTURE/project" && {script}',
                             check=check)

    def switch_forge_on(self) -> Path:
        fixture = self.task / "fixture"
        # The fixture provides the directory, as a steps file's touch relies on.
        (fixture / "home" / ".config" / "fixture-forge").touch()
        return fixture

    def test_the_fixture_has_no_forge_or_network_until_switched_on(self) -> None:
        self.built()
        self.run_tool("start", "after")
        for command in ("gh --version", "gh auth status --json hosts",
                        "gh repo view --json nameWithOwner,url"):
            refused = self.fixture_shell(command, check=False)
            self.assertEqual(refused.returncode, 1, command)
            self.assertEqual(refused.stderr, "gh: the fixture has no forge access\n")
        offline = self.fixture_shell("curl -q --url https://avatars.githubusercontent.com/octo", check=False)
        self.assertEqual(offline.returncode, 7)
        self.assertIn("no network access", offline.stderr)
        # Nor can git: a project's github.com remote is only for the forge to name.
        fetch = self.fixture_shell("git ls-remote https://github.com/octo-org/app", check=False)
        self.assertNotEqual(fetch.returncode, 0)
        self.assertIn("transport 'https' not allowed", fetch.stderr)

        # Read on every call: a step can switch it on in a running session.
        self.switch_forge_on()
        self.assertRegex(self.fixture_shell("gh --version").stdout, r"^gh version \d+\.\d+\.\d+ ")
        hosts = json.loads(self.fixture_shell(
            "gh auth status --active --hostname github.com --json hosts").stdout)
        self.assertEqual(hosts, {"hosts": {"github.com": [{
            "state": "success", "active": True, "host": "github.com", "login": "fixture",
            "tokenSource": "keyring", "scopes": "gist, read:org, repo, workflow", "gitProtocol": "https",
        }]}})
        self.assertEqual(json.loads(self.fixture_shell("gh auth status --json hosts").stdout), hosts)
        # gh reports another host's absence in JSON, and succeeds.
        enterprise = self.fixture_shell("gh auth status --hostname ghe.example.invalid --json hosts")
        self.assertEqual(json.loads(enterprise.stdout), {"hosts": {}})

        selector = "-R github.com/octo-org/app"
        fields = "--json number,title,headRefName"
        for command in (f"gh pr list {selector} --state open --limit 30 {fields}",
                        f"gh pr list {selector} --state all --limit 100 {fields} --head main",
                        f"gh pr list {selector} --author @me --state open --search sort:updated-desc {fields}"):
            self.assertEqual(json.loads(self.fixture_shell(command).stdout), [], command)
        missing = self.fixture_shell(f"gh pr view 7 {selector} {fields}", check=False)
        self.assertEqual(missing.returncode, 1)
        self.assertIn("Could not resolve to a PullRequest with the number of 7.", missing.stderr)

        # The review-request poll, behind the headers --include asks for. Text
        # mode reads gh's CRLF header lines as LF.
        page = self.fixture_shell(
            "gh api graphql --include --hostname github.com "
            "-f 'query=query($owner:String!,$name:String!,$cursor:String){viewer{id login} "
            "repository(owner:$owner,name:$name){pullRequests(first:50,after:$cursor,states:OPEN)"
            "{nodes{number} pageInfo{hasNextPage endCursor}}}}' -f owner=octo-org -f name=app").stdout
        head, body = page.split("\n\n", 1)
        self.assertTrue(head.startswith("HTTP/2.0 200 OK\n"))
        self.assertNotIn("X-Ratelimit-Remaining: 0", head)
        data = json.loads(body)["data"]
        self.assertEqual(data["viewer"]["login"], "fixture")
        self.assertEqual(data["repository"]["pullRequests"],
                         {"nodes": [], "pageInfo": {"hasNextPage": False, "endCursor": None}})
        # The pull request browser and its search find nothing to list.
        browser = json.loads(self.fixture_shell(
            "gh api graphql --hostname github.com -f 'query=query($owner:String!,$name:String!,$cursor:String)"
            "{repository(owner:$owner,name:$name){pullRequests(first:50,after:$cursor,states:OPEN)"
            "{nodes{id} pageInfo{hasNextPage endCursor}}}}' -f owner=octo-org -f name=app").stdout)
        self.assertEqual(browser["data"], {"repository": {"pullRequests": data["repository"]["pullRequests"]}})
        search = json.loads(self.fixture_shell(
            "gh api graphql --hostname github.com -f 'query=query($searchText:String!,$cursor:String)"
            "{search(type:ISSUE,query:$searchText,first:50,after:$cursor){nodes{... on PullRequest{id}} "
            "pageInfo{hasNextPage endCursor}}}' -f 'searchText=repo:octo-org/app is:pr fix'").stdout)
        self.assertEqual(search["data"]["search"]["nodes"], [])
        releases = self.fixture_shell(
            "gh api --hostname github.com --method GET repos/o/r/releases?per_page=100&page=1")
        self.assertEqual(json.loads(releases.stdout), [])
        absent = self.fixture_shell("gh api --include --hostname github.com repos/o/r/compare/a...b",
                                    check=False)
        self.assertEqual(absent.returncode, 1)
        self.assertIn("HTTP/2.0 404 Not Found\n", absent.stdout)
        self.assertEqual(absent.stderr, "gh: Not Found (HTTP 404)\n")
        write = self.fixture_shell("gh pr create -R github.com/o/r --title t --body-file -", check=False)
        self.assertEqual(write.returncode, 1)
        self.assertIn("`gh pr create`", write.stderr)
        dangling = self.fixture_shell("gh pr list --json", check=False)
        self.assertEqual((dangling.returncode, dangling.stderr), (1, "flag needs an argument: --json\n"))

    def test_the_forge_names_the_repository_of_a_github_remote(self) -> None:
        self.built()
        self.run_tool("start", "after")
        self.switch_forge_on()
        view = "gh repo view --json nameWithOwner,url"
        none = self.fixture_shell(view, check=False)
        self.assertEqual((none.returncode, none.stderr), (1, "no git remotes found\n"))
        self.fixture_shell("git remote add origin https://gitlab.example.invalid/octo-org/app.git")
        elsewhere = self.fixture_shell(view, check=False)
        self.assertEqual(elsewhere.returncode, 1)
        self.assertIn("point to a known GitHub host", elsewhere.stderr)

        for url in ("https://github.com/Octo-Org/my.app", "https://github.com/Octo-Org/my.app.git",
                    "git@github.com:Octo-Org/my.app.git", "ssh://git@github.com/Octo-Org/my.app/"):
            self.fixture_shell(f"git remote set-url origin {url}")
            self.assertEqual(json.loads(self.fixture_shell(view).stdout), {
                "nameWithOwner": "Octo-Org/my.app", "url": "https://github.com/Octo-Org/my.app",
            }, url)
        # As gh does, an upstream remote names the base repository over origin.
        self.fixture_shell("git remote add upstream https://github.com/upstream-org/app")
        self.assertEqual(json.loads(self.fixture_shell(view).stdout)["nameWithOwner"], "upstream-org/app")
        # And the remote `gh repo set-default` marked comes before both.
        self.fixture_shell("git config remote.origin.gh-resolved base")
        self.assertEqual(json.loads(self.fixture_shell(view).stdout)["nameWithOwner"], "Octo-Org/my.app")

    def test_the_forge_seeds_owner_avatars_into_the_apps_cache(self) -> None:
        self.built()
        self.run_tool("start", "after")
        fixture = self.switch_forge_on()
        seed = fixture / "avatars" / "Octo-Org.image"
        seed.write_bytes(b"\x89PNG\r\n\x1a\nseeded")
        old = time.time() - 30 * 24 * 60 * 60
        os.utime(seed, (old, old))
        self.fixture_shell("git remote add origin https://github.com/Octo-Org/app")
        self.fixture_shell("gh repo view --json nameWithOwner,url")

        # The path avatar.rs reads under the development storage name, owner lower-cased.
        cached = fixture / "cache" / "cagents-development" / "github-avatars" / "github.com-octo-org.image"
        self.assertEqual(cached.read_bytes(), seed.read_bytes())
        # Under its seven-day freshness, so the app does not try to download it.
        self.assertLess(time.time() - cached.stat().st_mtime, 60)

        # A cache it can't write leaves the avatar out, not the repository.
        cached.unlink()
        cached.parent.chmod(0o555)
        try:
            seed.write_bytes(b"\x89PNG\r\n\x1a\nchanged")
            viewed = self.fixture_shell("gh repo view --json nameWithOwner,url")
            self.assertEqual(json.loads(viewed.stdout)["nameWithOwner"], "Octo-Org/app")
            self.assertEqual(viewed.stderr, "")
            self.assertEqual(list(cached.parent.iterdir()), [])
        finally:
            cached.parent.chmod(0o755)

    def test_the_forge_lists_seeded_pull_requests_of_each_repository(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is not installed")
        self.built()
        self.run_tool("start", "after")
        fixture = self.switch_forge_on()
        pull = {"isDraft": False, "baseRefName": "main", "headRepository": {"name": "app"},
                "headRepositoryOwner": {"login": "octo-org"}, "updatedAt": "2026-11-01T00:00:00Z"}
        (fixture / "home" / ".config" / "fixture-forge-prs.json").write_text(json.dumps([
            {**pull, "number": 7, "title": "Open", "state": "OPEN", "headRefName": "feature",
             "author": {"login": "fixture"}, "url": "https://github.com/octo-org/app/pull/7"},
            {**pull, "number": 8, "title": "Merged", "state": "MERGED", "headRefName": "done",
             "author": {"login": "someone"}, "url": "https://github.com/octo-org/app/pull/8"},
            {**pull, "number": 9, "title": "Elsewhere", "state": "OPEN", "headRefName": "feature",
             "url": "https://github.com/other/app/pull/9"},
        ]))
        selector = "-R github.com/Octo-Org/app"

        def numbers(arguments: str) -> list[int]:
            listed = json.loads(self.fixture_shell(f"gh pr list {selector} {arguments}").stdout)
            return [entry["number"] for entry in listed]

        self.assertEqual(numbers("--state open --json number"), [7])
        self.assertEqual(numbers("--state closed --json number"), [8])
        self.assertEqual(numbers("--state all --json number"), [7, 8])
        self.assertEqual(numbers("--state all --limit 1 --json number"), [7])
        self.assertEqual(numbers("--state all --head done --json number"), [8])
        self.assertEqual(numbers("--state all --author @me --json number"), [7])
        self.assertEqual(numbers("--state=all --head=done --json=number"), [8])
        # Only the requested fields, as gh prints them.
        self.assertEqual(json.loads(self.fixture_shell(
            f"gh pr view 7 {selector} --json number,headRefName").stdout),
            {"number": 7, "headRefName": "feature"})
        checks = self.fixture_shell(f"gh pr checks 7 {selector} --json name", check=False)
        self.assertEqual((checks.returncode, checks.stderr), (1, "no checks reported on the 'feature' branch\n"))
        self.assertEqual(self.fixture_shell(f"gh pr view 9 {selector} --json number", check=False).returncode, 1)
        # A malformed seed fails in gh, naming the file, rather than as bad JSON in the app.
        (fixture / "home" / ".config" / "fixture-forge-prs.json").write_text('{"number": 7}')
        malformed = self.fixture_shell(f"gh pr list {selector} --json number", check=False)
        self.assertEqual(malformed.returncode, 1)
        self.assertIn("fixture-forge-prs.json must be a JSON array", malformed.stderr)
        # So does one missing a field the app requires, or with a state gh never prints.
        complete = {**pull, "number": 7, "title": "Open", "state": "OPEN", "headRefName": "feature",
                    "url": "https://github.com/octo-org/app/pull/7"}
        for broken in ({k: v for k, v in complete.items() if k != "title"}, {**complete, "state": "open"},
                       {k: v for k, v in complete.items() if k != "headRepositoryOwner"}):
            (fixture / "home" / ".config" / "fixture-forge-prs.json").write_text(json.dumps([broken]))
            refused = self.fixture_shell(f"gh pr list {selector} --json number", check=False)
            self.assertEqual(refused.returncode, 1, broken)
            self.assertIn("title, state (OPEN, CLOSED or MERGED), headRefName, baseRefName, updatedAt, "
                          "headRepositoryOwner.login and headRepository.name", refused.stderr)

    def test_a_forgotten_session_releases_the_lock_at_its_limit(self) -> None:
        self.built()
        self.run_tool("start", "after", "--limit", "1")
        for _ in range(100):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))
        self.assertIn("none", self.run_tool("status").stdout)

    def test_a_failed_app_start_reports_and_releases(self) -> None:
        self.built()
        result = self.run_tool("start", "after", check=False, extra={"STUB_APP_FAILS": "1"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("app window did not appear", result.stderr)
        for _ in range(100):
            if self.lock_is_free(self.native_lock):
                break
            time.sleep(0.2)
        self.assertTrue(self.lock_is_free(self.native_lock))


REMOTE_SCRIPT = REPOSITORY / "scripts" / "native-evidence-remote"

# Stands in for ssh to the build host: a login there starts from its own
# environment, never this shell's, and runs the command its shell is given.
REMOTE_STUBS = {
    "ssh": """
        #!/bin/bash
        printf '%s\\n' "$*" >> "$STUB_LOG/ssh"
        while [ "$#" -gt 0 ]; do
            case $1 in
                -o | -p | -i | -l) shift 2 ;;
                -*) shift ;;
                *) break ;;
            esac
        done
        host=$1
        shift
        [ "$host" = buildhost ] || { echo "ssh: unknown host $host" >&2; exit 255; }
        # What travels to the host: its arguments, above, and its input.
        tee -a "$STUB_LOG/ssh-input" | env -i HOME="$STUB_REMOTE_HOME" PATH="$PATH" STUB_LOG="$STUB_LOG" \\
            GIT_CONFIG_GLOBAL=/dev/null sh -c "$*"
    """,
}

# The build host's native-evidence: records each command and what it ran with.
REMOTE_TOOL = """
    #!/bin/bash
    [ "$1" = --task ] && task=$2 && shift 2
    printf '%s\\t%s\\n' "$task" "$*" >> "$STUB_LOG/remote-tool"
    env > "$STUB_LOG/remote-env"
    # As the real one: a task directory holds only its own files.
    if [ -d "$task" ] && [ ! -e "$task/.native-evidence" ] && [ -n "$(ls -A "$task")" ]; then
        echo "native-evidence: not a native-evidence task directory, and not empty: $task" >&2
        exit 2
    fi
    mkdir -p "$task/shots"
    : > "$task/.native-evidence"
    case $1 in
        start) printf '%s\\n' "$2" > "$task/side"; echo "session ready: $2" ;;
        stop) [ ! -e "$task/stop-fails" ] || exit 1; rm -f "$task/side"; echo stopped ;;
        # More than one line, written apart, as the real status is: a reader
        # that stops at the first ends it with SIGPIPE.
        status) [ -e "$task/side" ] && echo "session: $(cat "$task/side"), pid 1, ready" || echo 'session: none'
                sleep 0.2; echo "native lock: free" ;;
        shot) printf 'webp' > "$task/shots/$(cat "$task/side")-$2.webp"
              printf '%s\\tREV\\n' "$(cat "$task/side")-$2.webp" >> "$task/shots/manifest.tsv" ;;
        build) printf '%s\\n' "$3" > "$task/built-$2"; ls "$3" > "$task/files-$2" ;;
        cli) printf '%s\\n' "$*" > "$task/cli" ;;
    esac
"""


class RemoteEvidence(unittest.TestCase):
    def setUp(self) -> None:
        state = Path.home() / ".local" / "state"
        state.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="ner-", dir=state))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.logs = self.root / "logs"
        self.logs.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in REMOTE_STUBS.items():
            write_executable(self.bin / name, body, self.logs)
        self.remote_home = self.root / "remote-home"
        self.remote_home.mkdir()
        self.task = self.root / "local" / "mytask"
        self.env = {
            "PATH": os.pathsep.join([str(self.bin), os.environ["PATH"]]),
            "HOME": str(self.root / "home"),
            "STUB_LOG": str(self.logs),
            "STUB_REMOTE_HOME": str(self.remote_home),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            # As in a cagents session: none of it may reach the host.
            "CAGENTS_AGENT_AUTHORITY": "inherited-token",
        }
        self.remote_repository = self.repository("remote-repo", {"README": "remote\n"})
        self.tool = self.repository("tool", {"scripts/native-evidence": REMOTE_TOOL}, executable=True)
        self.env.update({
            "CAGENTS_EVIDENCE_REMOTE_REPOSITORY": str(self.remote_repository),
            "CAGENTS_EVIDENCE_REMOTE_TOOL": str(self.tool),
        })

    def git(self, path: Path, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(path), "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
            check=True, env=self.env, capture_output=True, text=True,
        ).stdout.strip()

    def repository(self, name: str, files: dict[str, str], executable: bool = False) -> Path:
        path = self.root / name
        for relative, body in files.items():
            file = path / relative
            file.parent.mkdir(parents=True, exist_ok=True)
            if executable:
                write_executable(file, body, self.logs)
            else:
                file.write_text(body)
        subprocess.run(["git", "init", "-q", "--initial-branch=main", str(path)], check=True, env=self.env)
        self.git(path, "add", ".")
        self.git(path, "commit", "-qm", name)
        return path

    def run_tool(self, *args: str, check: bool = True, via: Path = SCRIPT,
                 remote: bool = True, extra=None, cwd=None) -> subprocess.CompletedProcess:
        prefix = ["--remote", "buildhost"] if remote else []
        result = subprocess.run(
            [str(via), *prefix, "--task", str(self.task), *args],
            env={**self.env, **(extra or {})}, stdin=subprocess.DEVNULL, cwd=cwd,
            capture_output=True, text=True, timeout=60,
        )
        if check and result.returncode:
            self.fail(f"{args} exited {result.returncode}:\n{result.stdout}\n{result.stderr}")
        return result

    @property
    def remote_task(self) -> Path:
        return self.remote_home / ".local" / "state" / "cev" / "mytask"

    def tool_calls(self) -> list[str]:
        return [line.split("\t", 1)[1] for line in (self.logs / "remote-tool").read_text().splitlines()]

    def test_a_hosted_shell_is_pointed_at_the_remote_route(self) -> None:
        result = self.run_tool("status", check=False, remote=False)
        self.assertEqual(result.returncode, 77)
        self.assertIn("--remote fw16", result.stderr)

    def test_a_hosted_shell_runs_it_on_the_host_without_its_authority(self) -> None:
        checkout = self.repository("after", {"src.rs": "fn main() {}\n"})
        self.run_tool("build", "after", str(checkout))
        environment = (self.logs / "remote-env").read_text()
        self.assertNotIn("CAGENTS_AGENT_AUTHORITY", environment)
        # Nor does anything sent carry it, which the stub's clean login
        # would otherwise hide.
        for sent in ("ssh", "ssh-input"):
            data = (self.logs / sent).read_bytes()
            self.assertNotIn(b"inherited-token", data, sent)
            self.assertNotIn(b"CAGENTS_AGENT_AUTHORITY", data, sent)
        self.assertNotIn("inherited-token", environment)
        built = (self.remote_task / "built-after").read_text().strip()
        self.assertEqual(built, str(self.remote_repository / ".worktrees" / "cev-mytask-after"))
        self.assertEqual(self.git(Path(built), "rev-parse", "HEAD"), self.git(checkout, "rev-parse", "HEAD"))
        # The host's login shell may not be Bash: every script travels on its input.
        commands = {line.split(" buildhost ", 1)[-1] for line in (self.logs / "ssh").read_text().splitlines()
                    if "receive-pack" not in line}
        self.assertEqual(commands, {"bash -s"})

    def test_uncommitted_and_untracked_files_are_built_without_touching_the_checkout(self) -> None:
        checkout = self.repository("after", {"src.rs": "fn main() {}\n"})
        head = self.git(checkout, "rev-parse", "HEAD")
        (checkout / "src.rs").write_text("fn main() { changed(); }\n")
        (checkout / "new.rs").write_text("fn new() {}\n")
        (checkout / ".gitignore").write_text("ignored\n")
        (checkout / "ignored").write_text("secret\n")
        status = self.git(checkout, "status", "--porcelain")
        result = self.run_tool("build", "after", str(checkout))
        # The copy of its index goes, and says nothing as it does.
        self.assertNotIn("unbound variable", result.stderr)
        remote = self.remote_repository / ".worktrees" / "cev-mytask-after"
        self.assertEqual((remote / "src.rs").read_text(), "fn main() { changed(); }\n")
        self.assertTrue((remote / "new.rs").exists())
        self.assertFalse((remote / "ignored").exists())
        self.assertEqual(self.git(checkout, "rev-parse", "HEAD"), head)
        self.assertEqual(self.git(checkout, "status", "--porcelain"), status)
        side, local, recorded_head, commit = (self.task / "sources.tsv").read_text().split()
        self.assertEqual((side, local, recorded_head), ("after", str(checkout), head))
        self.assertEqual(self.git(remote, "rev-parse", "HEAD"), commit)
        # A later copy removes files the earlier one had.
        (checkout / "new.rs").unlink()
        self.run_tool("build", "after", str(checkout))
        self.assertFalse((remote / "new.rs").exists())

    def test_a_revision_is_copied_as_it_is(self) -> None:
        self.git(self.tool, "tag", "base")
        self.run_tool("build", "before", "base", cwd=self.tool)
        remote = self.remote_repository / ".worktrees" / "cev-mytask-before"
        self.assertEqual(self.git(remote, "rev-parse", "HEAD"), self.git(self.tool, "rev-parse", "base"))

    def test_shots_come_back_with_their_manifest(self) -> None:
        checkout = self.repository("after", {"src.rs": "\n"})
        self.run_tool("build", "after", str(checkout))
        self.run_tool("start", "after")
        result = self.run_tool("shot", "home")
        self.assertEqual(result.stdout.strip(), str(self.task / "shots" / "after-home.webp"))
        self.assertEqual((self.task / "shots" / "after-home.webp").read_text(), "webp")
        self.assertIn("after-home.webp", (self.task / "shots" / "manifest.tsv").read_text())

    def test_cli_arguments_name_the_fixture_on_the_host(self) -> None:
        checkout = self.repository("after", {"src.rs": "\n"})
        self.run_tool("build", "after", str(checkout))
        self.run_tool("cli", "project", "open", "{fixture}/project", "it's quoted")
        self.assertEqual((self.remote_task / "cli").read_text().strip(),
                         f"cli project open {self.remote_task}/fixture/project it's quoted")

    def test_pair_runs_the_steps_on_both_sides_and_brings_back_every_shot(self) -> None:
        before = self.repository("before", {"src.rs": "old\n"})
        after = self.repository("after", {"src.rs": "new\n"})
        steps = self.root / "steps.sh"
        steps.write_text('evidence drive click "Open repository"\nevidence shot "home-$SIDE"\n')
        self.run_tool("pair", "--before", str(before), "--after", str(after),
                      "--steps", str(steps), "--width", "1200", "--height", "800", "--portal", "gtk")
        self.assertTrue((self.task / "shots" / "before-home-before.webp").exists())
        self.assertTrue((self.task / "shots" / "after-home-after.webp").exists())
        calls = self.tool_calls()
        self.assertIn("start before --width 1200 --height 800 --portal gtk", calls)
        self.assertIn("drive click Open repository", calls)
        self.assertEqual(calls.count("stop"), 2)

    def test_steps_name_the_fixture_on_the_host(self) -> None:
        before = self.repository("before", {"src.rs": "old\n"})
        steps = self.root / "steps.sh"
        steps.write_text("evidence cli project open '{fixture}/project'\n")
        self.run_tool("pair", "--before", str(before), "--after", str(before), "--steps", str(steps))
        self.assertIn(f"cli project open {self.remote_task}/fixture/project", self.tool_calls())

    def test_a_running_side_is_not_rebuilt_under_its_session(self) -> None:
        checkout = self.repository("after", {"src.rs": "one\n"})
        self.run_tool("build", "after", str(checkout))
        self.run_tool("start", "after")
        (checkout / "src.rs").write_text("two\n")
        result = self.run_tool("build", "after", str(checkout), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("the after side is running on buildhost; stop it", result.stderr)
        built = self.remote_repository / ".worktrees/cev-mytask-after/src.rs"
        self.assertEqual(built.read_text(), "one\n")

    def test_a_task_with_another_owner_on_the_host_is_left_alone(self) -> None:
        checkout = self.repository("after", {"src.rs": "one\n"})
        self.run_tool("build", "after", str(checkout))
        owner = self.remote_home / ".local/state/cev-owners/mytask"
        self.assertTrue(owner.read_text().endswith(f":{self.task}\n"))
        # Another agent's task directory of the same name, elsewhere.
        tool_ref = self.git(self.remote_repository, "rev-parse", "refs/native-evidence/mytask/tool")
        other = self.task
        self.task = self.root / "elsewhere" / "mytask"
        # Its own tool, so a push would show.
        self.git(self.tool, "commit", "-q", "--allow-empty", "-m", "another tool")
        (checkout / "src.rs").write_text("two\n")
        for command in (["build", "after", str(checkout)], ["start", "after"], ["clean"], ["status"]):
            with self.subTest(command=command[0]):
                result = self.run_tool(*command, check=False)
                self.assertEqual(result.returncode, 3, result.stderr)
                self.assertIn(f"task mytask on {os.uname().nodename} belongs to {os.uname().nodename}:{other}; "
                              "give this task directory another name", result.stderr)
        self.assertEqual((self.remote_repository / ".worktrees/cev-mytask-after/src.rs").read_text(), "one\n")
        # Nothing was sent: the owner's refs are as they were.
        self.assertEqual(self.git(self.remote_repository, "rev-parse", "refs/native-evidence/mytask/tool"), tool_ref)
        self.assertTrue(self.remote_task.exists())

    def test_only_a_build_takes_a_name_on_the_host(self) -> None:
        self.run_tool("status", check=False)
        self.run_tool("fetch")
        self.assertFalse((self.remote_home / ".local/state/cev-owners/mytask").exists())

    def test_a_task_the_host_runs_directly_is_left_alone(self) -> None:
        # Someone on the host runs native-evidence with a task of this name.
        (self.remote_task / "shots").mkdir(parents=True)
        result = self.run_tool("clean", check=False)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("is not a task this route started; give this task directory another name", result.stderr)
        self.assertTrue((self.remote_task / "shots").exists())
        self.assertFalse((self.remote_home / ".local/state/cev-owners").exists())

    def test_a_task_an_earlier_route_started_is_claimed_as_the_refusal_says(self) -> None:
        # Before owner records, the route left a task and no record, and a
        # host with only such tasks has no record directory either.
        checkout = self.repository("after", {"src.rs": "\n"})
        (self.remote_task / "shots").mkdir(parents=True)
        (self.remote_task / ".native-evidence").touch()
        refused = self.run_tool("build", "after", str(checkout), check=False)
        self.assertEqual(refused.returncode, 3, refused.stderr)
        self.assertFalse((self.remote_home / ".local/state/cev-owners").exists())
        claim = refused.stderr.rstrip("\n").split("claim it there with: ", 1)[1]
        # Run as printed, on the host, by the shell a person there would use.
        subprocess.run(["bash", "-c", claim], check=True, cwd=self.root)
        owner = f"{os.uname().nodename}:{self.task}"
        record = self.remote_home / ".local/state/cev-owners/mytask"
        self.assertEqual(record.read_text(), owner + "\n")
        self.run_tool("build", "after", str(checkout))
        self.run_tool("status")

    def test_the_hosts_own_task_directory_is_refused(self) -> None:
        self.task = self.remote_task
        result = self.run_tool("status", check=False)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("run scripts/native-evidence here without --remote", result.stderr)

    def test_clean_keeps_everything_when_the_session_does_not_stop(self) -> None:
        checkout = self.repository("after", {"src.rs": "\n"})
        self.run_tool("build", "after", str(checkout))
        self.run_tool("start", "after")
        (self.remote_task / "stop-fails").touch()
        result = self.run_tool("clean", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the session did not stop, so nothing was removed", result.stderr)
        self.assertTrue((self.remote_repository / ".worktrees/cev-mytask-after").exists())
        self.assertTrue(self.remote_task.exists())

    def test_clean_removes_the_task_from_the_host_and_keeps_the_shots_here(self) -> None:
        checkout = self.repository("after", {"src.rs": "\n"})
        self.run_tool("build", "after", str(checkout))
        self.run_tool("start", "after")
        self.run_tool("shot", "home")
        self.run_tool("clean")
        self.assertFalse(self.remote_task.exists())
        self.assertFalse((self.remote_home / ".local/state/cev-owners/mytask").exists())
        self.assertFalse((self.remote_repository / ".worktrees/cev-mytask-after").exists())
        self.assertFalse((self.remote_repository / ".worktrees/cev-mytask-tool").exists())
        self.assertEqual(self.git(self.remote_repository, "for-each-ref", "refs/native-evidence/"), "")
        self.assertTrue((self.task / "shots" / "after-home.webp").exists())

    def test_a_failing_step_still_stops_its_session(self) -> None:
        before = self.repository("before", {"src.rs": "old\n"})
        steps = self.root / "steps.sh"
        steps.write_text("evidence shot first\nfalse\nevidence shot never\n")
        result = self.run_tool("pair", "--before", str(before), "--after", str(before),
                               "--steps", str(steps), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("steps failed on the before side", result.stderr)
        self.assertTrue((self.task / "shots" / "before-first.webp").exists())
        self.assertFalse((self.task / "shots" / "before-never.webp").exists())
        self.assertEqual(self.tool_calls()[-1], "stop")

    def test_a_lost_connection_still_stops_the_session(self) -> None:
        before = self.repository("before", {"src.rs": "old\n"})
        steps = self.root / "steps.sh"
        # What the host's watcher sends once the connection has gone: TERM
        # to the work's process group, which the steps run in.
        steps.write_text("evidence shot first\n"
                         "kill -TERM -- \"-$(ps -o pgid= -p $$ | tr -d ' ')\"\n"
                         "sleep 5\nevidence shot never\n")
        result = self.run_tool("pair", "--before", str(before), "--after", str(before),
                               "--steps", str(steps), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.tool_calls()[-1], "stop")
        self.assertNotIn("shot never", self.tool_calls())

    def test_an_unknown_host_fails_plainly(self) -> None:
        result = self.run_tool("status", check=False, extra={"CAGENTS_EVIDENCE_REMOTE": "elsewhere"},
                               remote=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotEqual(result.returncode, 77)

    def test_a_task_name_the_host_cannot_use_is_refused_before_anything_runs(self) -> None:
        self.task = self.root / "local" / "a-name-much-too-long-for-sockets"
        result = self.run_tool("status", check=False)
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.logs / "ssh").exists())


if __name__ == "__main__":
    unittest.main()
