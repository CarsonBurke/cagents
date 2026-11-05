#!/usr/bin/env python3
"""Exercise build offloading with a stand-in build host on this machine.

The stand-in `ssh` runs each remote command here under a private home, so
the build slots, the copied sources and the copied-back executables are all
in a temporary directory. Cargo builds only a tiny private crate.
"""

import importlib.machinery
import importlib.util
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]


def load_offload():
    loader = importlib.machinery.SourceFileLoader("cargo_offload", str(ROOT / "scripts/cargo-offload"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


offload = load_offload()


class Routing(unittest.TestCase):
    def test_builds_checks_lints_tests_and_docs_run_remotely(self):
        for arguments, subcommand in (
            (["build", "--locked", "--profile", "dev-runtime", "--bin", "cagents"], "build"),
            (["b", "--release"], "build"),
            (["check", "--locked", "--no-default-features", "--features", "ui"], "check"),
            (["clippy", "--all-targets", "--", "-D", "warnings"], "clippy"),
            (["test", "--locked", "--all-targets", "--", "--test-threads=1"], "test"),
            (["t", "-p", "cagents", "--", "/a/test/filter"], "test"),
            (["doc", "--no-deps"], "doc"),
        ):
            with self.subTest(arguments=arguments):
                self.assertEqual(offload.route(arguments), subcommand)

    def test_programs_benchmarks_and_other_subcommands_run_here(self):
        for arguments in (["run", "--bin", "cagents"], ["bench"], ["metadata"], ["fmt"],
                          ["+nightly", "build"], ["tree"], []):
            with self.subTest(arguments=arguments):
                self.assertIsNone(offload.route(arguments))

    def test_paths_and_configuration_on_this_host_keep_a_command_here(self):
        for arguments in (
            ["build", "--manifest-path", "x/Cargo.toml"],
            ["check", "--target-dir=/var/tmp/t"],
            ["build", "--config", "build.jobs=2"],
            ["check", "-Zunstable-options"],
            ["test", "--features", "/etc/x"],
            ["build", "--artifact-dir", "out"],
        ):
            with self.subTest(arguments=arguments):
                self.assertIsNone(offload.route(arguments))

    def test_results_used_on_this_host_keep_a_command_here(self):
        for arguments in (["test", "--no-run"], ["doc", "--open"], ["doc", "--target", "x"],
                          ["clippy", "--fix", "--allow-dirty"], ["check", "--fix"],
                          ["build", "--timings"], ["test", "--timings=html"],
                          ["build", "--message-format=json"], ["build", "--message-format", "json"]):
            with self.subTest(arguments=arguments):
                self.assertIsNone(offload.route(arguments))
        # Only `build` parses messages itself.
        self.assertEqual(offload.route(["check", "--message-format=short"]), "check")


class Artifacts(unittest.TestCase):
    target = "/home/u/.cache/cagents-offload/r/slot-1/src/target"

    def message(self, executable, kind, reason="compiler-artifact"):
        return json.dumps({"reason": reason, "executable": executable, "target": {"kind": kind}})

    def test_reported_executables_are_listed_relative_to_the_target(self):
        messages = [
            self.message(None, ["lib"]),
            self.message(f"{self.target}/dev-runtime/build/x-1/build-script-build", ["custom-build"]),
            self.message(f"{self.target}/dev-runtime/cagents-ui", ["bin"]),
            self.message(f"{self.target}/dev-runtime/cagents-ui", ["bin"]),
            self.message(f"{self.target}/debug/examples/demo", ["example"]),
            self.message(f"{self.target}/debug/deps/probe-0123", ["test"]),
            self.message(f"{self.target}/dev-runtime/cagents", ["bin"], reason="build-finished"),
            "warning: not a message",
            "{not json",
        ]
        self.assertEqual(offload.artifacts(messages, self.target), [
            "dev-runtime/cagents-ui", "debug/examples/demo", "debug/deps/probe-0123",
        ])

    def test_executables_outside_the_target_are_not_copied(self):
        messages = [self.message("/usr/bin/true", ["bin"]),
                    self.message(f"{self.target}/../../escape", ["bin"]),
                    self.message(f"{self.target}-other/debug/x", ["bin"])]
        self.assertEqual(offload.artifacts(messages, self.target), [])


class RemoteCommand(unittest.TestCase):
    def test_the_agent_and_its_arguments_survive_any_login_shell(self):
        arguments = ["run", "it's", "a\\b", "$HOME", "x y", "\n"]
        words = offload.remote_command(arguments)
        # The encoded payload is base64, which no shell rewrites; the
        # bootstrap is single-quoted without backslashes or single quotes.
        self.assertRegex(words[-1], r"^[A-Za-z0-9+/=]+$")
        bootstrap = words[-2]
        self.assertTrue(bootstrap.startswith("'") and bootstrap.endswith("'"))
        self.assertNotIn("\\", bootstrap)
        self.assertNotIn("'", bootstrap[1:-1])
        echo = "import sys; print(repr(sys.argv[1:]))"
        command = " ".join(words).replace("REMOTE_AGENT", "")
        payload = json.loads(__import__("base64").b64decode(words[-1]))
        self.assertEqual(payload[1], arguments)
        for shell in ("sh", "bash", "fish"):
            if shutil.which(shell):
                with self.subTest(shell=shell):
                    probe = offload.remote_command(arguments)
                    probe[-1] = __import__("base64").b64encode(
                        json.dumps([echo, arguments]).encode()).decode()
                    result = subprocess.run([shell, "-c", " ".join(probe)],
                                            capture_output=True, text=True, check=True)
                    self.assertEqual(result.stdout.strip(), repr(arguments))


class Settings(unittest.TestCase):
    def test_only_build_affecting_settings_without_local_paths_are_forwarded(self):
        environment = {
            "RUST_BACKTRACE": "1", "RUSTFLAGS": "-Dwarnings", "CARGO_TERM_COLOR": "always",
            "CARGO_PROFILE_DEV_DEBUG": "0", "CAGENTS_LIVE_TESTS": "0",
            "CAGENTS_STATE_DIR": "/home/u/state", "CAGENTS_BUILD_SLOTS": "2",
            "CARGO_HOME": "/home/u/.cargo", "CARGO_TARGET_DIR": "/t", "CARGO_BUILD_JOBS": "12",
            "HOME": "/home/u", "PATH": "/usr/bin", "CAGENTS_SESSION_TOKEN": "t",
            "CAGENTS_LINEAR_API_KEY_WORK": "k", "CAGENTS_AGENT_PASSWORD": "p",
        }
        self.assertEqual(offload.forwarded_environment(environment), {
            "RUST_BACKTRACE": "1", "RUSTFLAGS": "-Dwarnings", "CARGO_TERM_COLOR": "always",
            "CARGO_PROFILE_DEV_DEBUG": "0", "CAGENTS_LIVE_TESTS": "0",
        })

    def test_a_setting_naming_a_file_here_keeps_the_build_here(self):
        self.assertEqual(offload.local_path_setting({
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER": "/t/cagentsd", "CAGENTS_LIVE_TESTS": "0",
            "CAGENTS_BUILD_LOCK": "/var/tmp/l", "TMPDIR": "/tmp/x"}),
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        self.assertIsNone(offload.local_path_setting({"CAGENTS_BUILD_LOCK": "/l", "TMPDIR": "/t"}))

    def test_profiles_are_kept_and_this_hosts_build_settings_are_not(self):
        with tempfile.NamedTemporaryFile("w", suffix=".toml") as config:
            config.write(textwrap.dedent("""
                [build]
                jobs = 12
                build-dir = "/var/tmp/cargo-build{workspace-root}/target"
                [profile.dev]
                debug = "line-tables-only"
                incremental = true
                [profile.dev.package."*"]
                debug = 0
            """))
            config.flush()
            text = offload.profile_config(config.name)
        self.assertNotIn("jobs", text)
        self.assertNotIn("build-dir", text)
        import tomllib
        self.assertEqual(tomllib.loads(text), {"profile": {"dev": {
            "debug": "line-tables-only", "incremental": True, "package": {"*": {"debug": 0}}}}})

    def test_a_missing_config_gives_no_profiles(self):
        self.assertEqual(offload.profile_config("/nonexistent/config.toml"), "")


STAND_IN_SSH = """#!/bin/sh
# Runs the remote command here, as ssh would on the build host.
while [ "$#" -gt 0 ]; do
    case $1 in
        -o) shift 2 ;;
        -*) shift ;;
        *) break ;;
    esac
done
[ "$1" != unreachable ] || exit 255
[ "$1" != silent ] || exec sleep 600
[ "$1" != broken ] || exit 1
shift
cd "$HOME"
# Like sshd, start from a clean environment: nothing of this one reaches the
# build host except what the command carries.
exec env -i HOME="$HOME" PATH="$PATH" CARGO_HOME="$CARGO_HOME" \
    RUSTUP_HOME="$RUSTUP_HOME" sh -c "$*"
"""


@unittest.skipUnless(shutil.which("cargo") and shutil.which("rsync"), "needs cargo and rsync")
class StandInHost(unittest.TestCase):
    """A tiny crate built through the gate on a stand-in build host."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-offload-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        tools = self.root / "tools"
        tools.mkdir()
        for name in ("cargo-gate", "cargo-gate-command.py", "cargo-offload"):
            shutil.copy2(ROOT / "scripts" / name, tools / name)
        (tools / "ssh").write_text(STAND_IN_SSH)
        (tools / "ssh").chmod(0o755)
        self.gate = tools / "cargo-gate"
        self.host_home = self.root / "host"
        self.host_home.mkdir()
        self.crate = self.root / "crate"
        (self.crate / "src").mkdir(parents=True)
        (self.crate / "Cargo.toml").write_text(textwrap.dedent("""
            [package]
            name = "probe"
            version = "0.1.0"
            edition = "2021"
            [workspace]
        """))
        (self.crate / "src/main.rs").write_text('fn main() { println!("probe"); }\n')
        (self.crate / ".gitignore").write_text("/target\n")
        subprocess.run(["git", "init", "-q"], cwd=self.crate, check=True)
        home = Path.home()
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("CAGENTS_BUILD_", "CARGO_BUILD_"))
        } | {
            "PATH": f"{tools}:{os.environ['PATH']}",
            "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"),
            "CAGENTS_BUILD_REMOTE": "stand-in",
            "CAGENTS_BUILD_REMOTE_SLOTS": "2",
            "CAGENTS_BUILD_SEED": "0",
            "CAGENTS_BUILD_FREE_FLOOR_GIB": "0",
            "CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB": "1",
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(home / ".cargo")),
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(home / ".rustup")),
            "CARGO_BUILD_BUILD_DIR": str(self.root / "local-build"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"),
        }

    def gate_run(self, *arguments, env, timeout=120):
        return subprocess.run([str(self.gate), *arguments], cwd=self.crate, env=env,
                              capture_output=True, text=True, timeout=timeout)

    def remote_env(self):
        # The stand-in host's home holds the slots; Cargo and rustup keep the
        # real ones, which the environment names.
        return self.env | {"HOME": str(self.host_home)}

    def test_a_build_runs_remotely_and_its_executable_comes_back(self):
        result = self.gate_run("build", "--bin", "probe", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("building here", result.stderr)
        slot = self.host_home / ".cache/cagents-offload/crate/slot-0/src"
        self.assertTrue((slot / "src/main.rs").is_file())
        self.assertTrue((slot / "target/debug/probe").is_file())
        executable = self.crate / "target/debug/probe"
        self.assertEqual(subprocess.run([executable], capture_output=True, text=True).stdout, "probe\n")
        # Nothing was built here.
        self.assertFalse((self.root / "local-build").exists())

    def test_documentation_comes_back(self):
        (self.crate / "src/lib.rs").write_text("//! Probe documentation.\n")
        result = self.gate_run("doc", "--no-deps", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.crate / "target/doc/probe/index.html").is_file())

    def test_credentials_stay_here(self):
        (self.crate / "build.rs").write_text(textwrap.dedent("""
            fn main() {
                println!("cargo:rerun-if-env-changed=CAGENTS_SESSION_TOKEN");
                if std::env::var_os("CAGENTS_SESSION_TOKEN").is_some() {
                    panic!("a credential reached the build host");
                }
            }
        """))
        env = self.remote_env() | {"CAGENTS_SESSION_TOKEN": "secret-value"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("building here", result.stderr)

    def test_removed_sources_leave_the_slot_and_build_output_stays(self):
        extra = self.crate / "src/extra.rs"
        extra.write_text("// untracked, not ignored\n")
        self.assertEqual(self.gate_run("check", env=self.remote_env()).returncode, 0)
        slot = self.host_home / ".cache/cagents-offload/crate/slot-0/src"
        self.assertTrue((slot / "src/extra.rs").exists())
        extra.unlink()
        self.assertEqual(self.gate_run("check", env=self.remote_env()).returncode, 0)
        self.assertFalse((slot / "src/extra.rs").exists())
        self.assertTrue((slot / "target").is_dir())

    def test_temporary_files_go_in_the_slot_and_do_not_outlive_the_next_command(self):
        # Short enough for the Unix sockets tests bind under it, and on disk.
        tmp = Path(f"/var/tmp/cagents-offload-{os.getuid()}/crate-0")
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (self.crate / "build.rs").write_text(textwrap.dedent(f"""
            fn main() {{
                let dir = std::env::temp_dir();
                assert_eq!(dir, std::path::Path::new({json.dumps(str(tmp))}));
                std::fs::write(dir.join("left-behind"), "").unwrap();
                println!("cargo:rerun-if-changed=build.rs");
            }}
        """))
        env = self.remote_env()
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((tmp / "left-behind").exists())
        (self.crate / "src/main.rs").write_text('fn main() { println!("changed"); }\n')
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((tmp / "left-behind").exists())

    def test_a_failing_build_keeps_cargos_exit_status(self):
        (self.crate / "src/main.rs").write_text("fn main() { missing(); }\n")
        result = self.gate_run("check", env=self.remote_env())
        self.assertEqual(result.returncode, 101)
        self.assertIn("missing", result.stderr)

    def test_an_unreachable_host_builds_here_after_saying_so(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "unreachable"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo-gate: building here: unreachable is unreachable\n", result.stderr)
        self.assertTrue((self.root / "local-build").exists())
        # The next build does not try again at once.
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stderr, r"building here: unreachable is unreachable \(tried \d+s ago\)")

    def test_a_host_that_does_not_answer_builds_here_and_is_tried_again(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "silent",
                                   "CAGENTS_BUILD_REMOTE_ANSWER_SECONDS": "1"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("building here: silent did not answer within 1s\n", result.stderr)
        self.assertFalse((self.root / "runtime/cagents-offload/silent.unreachable").exists())

    def test_a_lease_that_fails_on_the_host_does_not_mark_it_unreachable(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "broken"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("building here: broken could not lease a build slot (status 1)\n", result.stderr)
        self.assertFalse((self.root / "runtime/cagents-offload/broken.unreachable").exists())

    def test_local_builds_here_silently(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "local"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("building here", result.stderr)
        self.assertTrue((self.root / "local-build").exists())

    def test_a_host_below_its_disk_floor_builds_here(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB": str(10**7)}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stderr, r"building here: stand-in has \d+ GiB free, below its 10000000 GiB floor")

    def test_a_worktree_returns_to_the_slot_it_used_last(self):
        env = self.remote_env()
        slots = self.host_home / ".cache/cagents-offload/crate"
        self.assertEqual(self.gate_run("check", env=env).returncode, 0)
        (slots / "slot-0/owner").write_text("another checkout\n")
        (slots / "slot-1").mkdir(parents=True, exist_ok=True)
        (slots / "slot-1/owner").write_text(f"{os.uname().nodename}:{self.crate.resolve()}\n")
        self.assertEqual(self.gate_run("check", env=env).returncode, 0)
        self.assertTrue((slots / "slot-1/src/src/main.rs").exists())

    def test_the_admission_budget_bounds_the_wait_for_a_build_host_slot(self):
        slots = self.host_home / ".cache/cagents-offload/crate"
        held = []
        for index in range(2):
            (slots / f"slot-{index}").mkdir(parents=True)
            lock = open(slots / f"slot-{index}/lock", "a")
            self.addCleanup(lock.close)
            fcntl.flock(lock, fcntl.LOCK_EX)
            held.append(lock)
        env = self.remote_env() | {"CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "2"}
        started = time.monotonic()
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertLess(time.monotonic() - started, 30)
        self.assertIn("waiting for one of 2 build slots on stand-in", result.stderr)
        self.assertIn("cargo-gate: build admission timed out", result.stderr)
        self.assertNotIn("building here", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_the_admission_budget_does_not_time_the_build(self):
        (self.crate / "build.rs").write_text(
            "fn main() { std::thread::sleep(std::time::Duration::from_secs(3)); }\n")
        env = self.remote_env() | {"CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "1"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("timed out", result.stderr)

    def test_cancelling_the_gate_ends_the_remote_command(self):
        marker = self.root / "running"
        (self.crate / "build.rs").write_text(textwrap.dedent(f"""
            fn main() {{
                std::fs::write({json.dumps(str(marker))}, std::process::id().to_string()).unwrap();
                std::thread::sleep(std::time::Duration::from_secs(120));
            }}
        """))
        gate = subprocess.Popen([str(self.gate), "check"], cwd=self.crate, env=self.remote_env(),
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: gate.poll() is None and gate.kill())
        deadline = time.monotonic() + 120
        while not marker.exists() or not marker.read_text():
            self.assertLess(time.monotonic(), deadline, "build script never started")
            self.assertIsNone(gate.poll())
            time.sleep(0.1)
        script = int(marker.read_text())
        gate.send_signal(signal.SIGTERM)
        self.assertEqual(gate.wait(timeout=30), 128 + signal.SIGTERM)
        deadline = time.monotonic() + 15
        while Path(f"/proc/{script}").exists():
            state = Path(f"/proc/{script}/stat").read_text().rsplit(")", 1)[1].split()[0]
            if state == "Z":
                break
            self.assertLess(time.monotonic(), deadline, "the remote build script outlived the gate")
            time.sleep(0.1)


class RemoteRun(unittest.TestCase):
    """The build host's end of a command: it ends what it started when the
    connection to this host closes, outside any supervisor here."""

    def test_a_released_slot_waits_for_its_command_to_end(self):
        # A cancelled build closes the lease and the command's connections
        # together; the next build must not start in the slot while the
        # command's process group is still being ended.
        with tempfile.TemporaryDirectory() as base:
            lease = subprocess.Popen(
                ["python3", "-c", offload.REMOTE_AGENT, "lease", base, "1", "key", "0", ""],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            self.addCleanup(lease.stdout.close)
            self.addCleanup(lease.kill)
            self.assertRegex(lease.stdout.readline(), r"^slot 0 ")
            with open(Path(base, "slot-0", "running"), "a") as running:
                fcntl.flock(running, fcntl.LOCK_EX)
                lease.stdin.close()
                with self.assertRaises(subprocess.TimeoutExpired):
                    lease.wait(timeout=1)
            self.assertEqual(lease.wait(timeout=10), 0)

    def test_pruning_waits_for_the_slots_command_to_end(self):
        with tempfile.TemporaryDirectory() as base:
            Path(base, "src").mkdir()
            stale = Path(base, "src", "stale.rs")
            stale.write_text("")
            with open(Path(base, "running"), "a") as running:
                fcntl.flock(running, fcntl.LOCK_EX)
                prune = subprocess.Popen(["python3", "-c", offload.REMOTE_AGENT, "prune", base],
                                         stdin=subprocess.PIPE)
                self.addCleanup(prune.kill)
                prune.stdin.close()
                with self.assertRaises(subprocess.TimeoutExpired):
                    prune.wait(timeout=1)
                self.assertTrue(stale.exists())
            self.assertEqual(prune.wait(timeout=10), 0)
            self.assertFalse(stale.exists())

    def test_closing_the_connection_ends_the_command_and_its_children(self):
        with tempfile.TemporaryDirectory() as base:
            Path(base, "src").mkdir()
            marker = Path(base, "pid")
            agent = subprocess.Popen(
                ["python3", "-c", offload.REMOTE_AGENT, "run", base, ".",
                 "sh", "-c", f"sleep 120 & echo $! >{marker}; wait"],
                stdin=subprocess.PIPE, start_new_session=True)
            agent.stdin.write(b"{}\n")
            agent.stdin.flush()
            deadline = time.monotonic() + 10
            while not marker.exists() or not marker.read_text().strip():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.05)
            sleeper = int(marker.read_text())
            agent.stdin.close()
            self.assertEqual(agent.wait(timeout=15), 130)
            deadline = time.monotonic() + 5
            while Path(f"/proc/{sleeper}").exists() and Path(
                    f"/proc/{sleeper}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z":
                self.assertLess(time.monotonic(), deadline, "the command's child outlived it")
                time.sleep(0.05)

    def test_a_finished_command_keeps_its_status(self):
        with tempfile.TemporaryDirectory() as base:
            Path(base, "src").mkdir()
            # The connection stays open, as it does while the gate waits.
            agent = subprocess.Popen(
                ["python3", "-c", offload.REMOTE_AGENT, "run", base, ".",
                 "sh", "-c", 'test "$PROBE" = x && exit 7'],
                stdin=subprocess.PIPE)
            self.addCleanup(agent.stdin.close)
            agent.stdin.write(b'{"PROBE": "x"}\n')
            agent.stdin.flush()
            self.assertEqual(agent.wait(timeout=15), 7)


class Prune(unittest.TestCase):
    """The build host's removal of sources the checkout no longer has."""

    def test_files_not_listed_go_and_the_target_directory_stays(self):
        with tempfile.TemporaryDirectory() as base:
            source = Path(base, "src")
            for name in ("keep.rs", "a/keep.rs", "a/gone.rs", "b/gone.rs", "target/debug/x"):
                Path(source, name).parent.mkdir(parents=True, exist_ok=True)
                Path(source, name).write_text("")
            Path(source, "link").symlink_to("keep.rs")
            subprocess.run(["python3", "-c", offload.REMOTE_AGENT, "prune", base],
                           input=b"keep.rs\0a/keep.rs\0", check=True)
            left = sorted(str(path.relative_to(source)) for path in source.rglob("*"))
            self.assertEqual(left, ["a", "a/keep.rs", "keep.rs", "target", "target/debug",
                                    "target/debug/x"])


if __name__ == "__main__":
    unittest.main()
