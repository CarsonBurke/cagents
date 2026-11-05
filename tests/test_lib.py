#!/usr/bin/env python3
"""Fake Cargo/libtest contracts; every process and admission file is private."""

import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)
PROTECTED = ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")

# This program is both fake Cargo and its executable artifact. It never launches
# a compiler, provider or service, and observes only its synthetic environment.
TOOL = r'''#!/usr/bin/python3
import fcntl
import json
import os
from pathlib import Path
import signal
import sys
import time

root = Path(os.environ["TEST_ROOT"])
config = json.loads((root / "control.json").read_text())
cargo = Path(sys.argv[0]).name == "cargo"
phase = "compiler" if cargo else (
    "list-ignored" if "--ignored" in sys.argv else
    "list-all" if "--list" in sys.argv else "test")

def held():
    with open(os.environ["CAGENTS_BUILD_LOCK"], "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False

def record(**extra):
    event = dict(phase=phase, pid=os.getpid(), arguments=sys.argv[1:],
                 cwd=os.getcwd(), held=held(),
                 protected={name: os.environ[name] for name in
                            ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")})
    event.update(extra)
    with open(root / "events.jsonl", "a") as output:
        output.write(json.dumps(event) + "\n")

def interrupted(number, _frame):
    record(signal=number)
    (root / (phase + ".signalled")).touch()
    deadline = time.monotonic() + 4
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    raise SystemExit(128 + number)

for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
    signal.signal(number, interrupted)
record()
(root / (phase + ".ready")).touch()
if config.get("hold") == phase:
    deadline = time.monotonic() + 6
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    if not (root / "release").exists():
        raise SystemExit(95)

if cargo:
    artifact = dict(reason="compiler-artifact", manifest_path=str(root / "Cargo.toml"),
                    target=dict(name="cagents", kind=["lib"]),
                    profile=dict(test=True), executable=str(root / "libtest"))
    variant = config.get("artifact", "valid")
    if variant == "wrong-manifest":
        artifact["manifest_path"] = str(root / "other/Cargo.toml")
    elif variant == "wrong-name":
        artifact["target"]["name"] = "other"
    elif variant == "wrong-kind":
        artifact["target"]["kind"] = ["bin"]
    elif variant == "wrong-profile":
        artifact["profile"]["test"] = False
    elif variant == "relative":
        artifact["executable"] = "libtest"
    elif variant == "non-string":
        artifact["executable"] = 1
    elif variant == "missing-file":
        artifact["executable"] = str(root / "absent")
    elif variant == "directory":
        artifact["executable"] = str(root / "private-home")
    if config.get("diagnostics"):
        print("plain compiler output")
        print(json.dumps(dict(reason="compiler-message", message=dict(rendered="rendered warning\n"))))
        print("compiler stderr", file=sys.stderr)
    if variant != "missing":
        print(json.dumps(artifact))
    if variant == "duplicate":
        print(json.dumps(artifact))
    if variant == "malformed-json":
        print("[]")
    if variant == "truncated-json":
        print("{broken")
    raise SystemExit(config.get("build_exit", 0))

if phase.startswith("list"):
    record(stdin=sys.stdin.read())
    if config.get("replace") == phase:
        path = root / "libtest"
        replacement = root / "replacement"
        replacement.write_bytes(path.read_bytes())
        replacement.chmod(0o700)
        replacement.replace(path)
    key = "ignored" if phase == "list-ignored" else "available"
    defaults = ["module::one", "module::two", "other::three", "module::ignored"]
    names = config.get(key, ["module::ignored"] if key == "ignored" else defaults)
    for name in names:
        print(name + ": test")
    if config.get("malformed_listing") == phase:
        print("unparseable listing")
    if config.get("benchmark"):
        print("module::benchmark: benchmark")
    raise SystemExit(config.get(phase + "_exit", 0))

record(stdin=sys.stdin.read())
raise SystemExit(config.get("test_exit", 0))
'''


class TestLib(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-test-lib-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("test-lib", "cargo-gate", "cargo-gate-command.py"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        if (ROOT / "scripts/seed-build-dir").exists():
            shutil.copy2(ROOT / "scripts/seed-build-dir", scripts / "seed-build-dir")
        (self.root / "Cargo.toml").write_text("# private fake Cargo manifest\n")
        (self.root / "bin").mkdir()
        for path in (self.root / "bin/cargo", self.root / "libtest"):
            path.write_text(TOOL)
            path.chmod(0o700)
        for name in ("private-home", "private-codex", "caller", "tmp"):
            (self.root / name).mkdir(mode=0o700)
        self.lock = self.root / "slot.lock"
        self.env = {
            "PATH": str(self.root / "bin") + ":/usr/bin:/bin",
            "HOME": str(self.root / "private-home"),
            "CODEX_HOME": str(self.root / "private-codex"),
            "CAGENTS_AGENT_AUTHORITY": "fixture-agent-authority",
            "CAGENTS_BUILD_LOCK": str(self.lock), "CAGENTS_BUILD_SLOTS": "1",
            "CAGENTS_BUILD_SEED": "0",
            "CARGO_BUILD_JOBS": "1", "TEST_ROOT": str(self.root),
            "TMPDIR": str(self.root / "tmp"), "LC_ALL": "C",
        }
        self.configure()
        self.children = []
        self.addCleanup(self.cleanup_processes)

    def configure(self, **config):
        (self.root / "control.json").write_text(json.dumps(config))

    def cleanup_processes(self):
        (self.root / "release").touch()
        for process in self.children:
            if process.poll() is None:
                # Each wrapper owns a fresh session containing fixture children
                # only. Retaining its unreaped Popen protects this group ID.
                os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=5)

    def start(self, *arguments, env=None, pass_fds=()):
        process = subprocess.Popen(
            [sys.executable, "-I", "-B", str(self.root / "scripts/test-lib"), *arguments],
            cwd=self.root / "caller", env=self.env if env is None else env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, start_new_session=True, pass_fds=pass_fds,
        )
        self.children.append(process)
        return process

    def invoke(self, *arguments, stdin="", env=None, pass_fds=()):
        process = self.start(*arguments, env=env, pass_fds=pass_fds)
        stdout, stderr = process.communicate(stdin, timeout=10)
        return subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)

    def events(self, phase=None):
        path = self.root / "events.jsonl"
        events = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        return [event for event in events if phase is None or event["phase"] == phase]

    def clear(self):
        for name in ("events.jsonl", "release"):
            (self.root / name).unlink(missing_ok=True)
        for path in self.root.glob("*.ready"):
            path.unlink()
        for path in self.root.glob("*.signalled"):
            path.unlink()

    def wait_for(self, condition, label, timeout=5):
        deadline = time.monotonic() + timeout
        while not condition():
            if time.monotonic() >= deadline:
                self.fail("timed out waiting for " + label)
            time.sleep(0.01)

    def held(self):
        with self.lock.open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            return False

    def assert_no_test(self):
        self.assertEqual(self.events("test"), [])

    def test_isolated_lane_arguments_cwd_profile_and_admission_release(self):
        for lane, profile in (("cli", None), ("host", "dev-runtime"), ("ui", "test")):
            with self.subTest(lane=lane):
                self.clear()
                extra = [] if profile is None else ["--profile", profile]
                result = self.invoke(lane, "module::one", "--test-threads", "2", *extra)
                self.assertEqual(result.returncode, 0, result.stderr)
                events = self.events()
                compiler = self.events("compiler")[0]
                expected = ["test", "--locked", "--no-default-features", "--manifest-path",
                            str(self.root / "Cargo.toml"), "--package", "cagents", "--lib",
                            "--no-run", "--message-format=json-render-diagnostics"]
                if lane != "cli":
                    expected += ["--features", lane]
                self.assertEqual(compiler["arguments"], expected + extra)
                self.assertTrue(compiler["held"], events)
                self.assertEqual({event["phase"] for event in events},
                                 {"compiler", "list-all", "list-ignored", "test"})
                for event in events:
                    self.assertEqual(event["cwd"], str(self.root))
                    self.assertEqual(event["protected"], {key: self.env[key] for key in PROTECTED})
                    if event["phase"] != "compiler":
                        self.assertFalse(event["held"], event)
                self.assertEqual(self.events("test")[0]["arguments"],
                                 ["--exact", "module::one", "--test-threads", "2"])
                self.assertFalse(self.held())

    def test_artifact_must_be_unique_matching_absolute_and_runnable(self):
        for variant in ("missing", "duplicate", "wrong-manifest", "wrong-name", "wrong-kind",
                        "wrong-profile", "relative", "non-string", "missing-file", "directory",
                        "unrunnable", "malformed-json", "truncated-json"):
            with self.subTest(variant=variant):
                self.clear()
                self.configure(artifact=variant)
                (self.root / "libtest").chmod(0o600 if variant == "unrunnable" else 0o700)
                result = self.invoke("cli", "module::one")
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertIn("test-lib:", result.stderr)
                self.assertEqual(len(self.events()), 1)
                self.assert_no_test()
                self.assertFalse(self.held())

    def test_missing_exact_name_is_refused_instead_of_running_zero_tests(self):
        for names in (("missing::name",), ("module::one", "missing::name")):
            with self.subTest(names=names):
                self.clear()
                result = self.invoke("cli", *names)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("exact tests not found: missing::name", result.stderr)
                self.assert_no_test()

    def test_invalid_selections_are_refused_before_compile(self):
        for arguments in (("cli",), ("cli", "module::one", "module::one"),
                          ("cli", "--", "--ignored"), ("cli", "--prefix=--bad::"),
                          ("cli", "--prefix", "module"), ("cli", "--prefix", "module*::"),
                          ("cli", "--prefix", "::"), ("cli", "bad\nname"),
                          ("cli", "module::one", "--test-threads", "0")):
            with self.subTest(arguments=arguments):
                result = self.invoke(*arguments)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.events(), [])

    def test_prefixes_deduplicate_ordinary_tests_and_report_ignored(self):
        self.configure(benchmark=True)
        result = self.invoke("ui", "module::one", "--prefix", "module::", "--prefix", "module::")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping ignored module::ignored", result.stderr)
        self.assertIn("running 2 exact ui tests outside build admission", result.stderr)
        self.assertEqual(self.events("test")[0]["arguments"],
                         ["--exact", "module::one", "module::two"])

    def test_interleaved_options_and_ignored_prefix_union_with_ordinary_exact(self):
        self.configure(available=["module::ignored", "other::three"])
        result = self.invoke("ui", "--profile", "dev-runtime", "--prefix", "module::",
                             "other::three", "--test-threads", "3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping ignored module::ignored", result.stderr)
        self.assertEqual(self.events("test")[0]["arguments"],
                         ["--exact", "other::three", "--test-threads", "3"])
        self.assertEqual(self.events("compiler")[0]["arguments"][-4:],
                         ["--features", "ui", "--profile", "dev-runtime"])

    def test_ignored_explicit_empty_prefix_and_ignored_only_are_refused(self):
        for names, config, message in (
            (("module::ignored",), {}, "explicit ignored tests"),
            (("--prefix", "absent::"), {}, "selects no tests"),
            (("--prefix", "module::"), {"available": ["module::ignored"]}, "no ordinary tests"),
        ):
            with self.subTest(names=names):
                self.clear()
                self.configure(**config)
                result = self.invoke("host", *names)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assert_no_test()

    def test_malformed_duplicate_inconsistent_and_failed_listings_never_run_tests(self):
        cases = (
            {"malformed_listing": "list-all"}, {"malformed_listing": "list-ignored"},
            {"available": ["module::one", "module::one"]},
            {"ignored": ["module::ignored", "module::ignored"]},
            {"ignored": ["missing::ignored"]}, {"available": [], "ignored": []},
            {"list-all_exit": 37}, {"list-ignored_exit": 38},
        )
        for config in cases:
            with self.subTest(config=config):
                self.clear()
                self.configure(**config)
                result = self.invoke("cli", "module::one")
                self.assertNotEqual(result.returncode, 0)
                for key in ("list-all_exit", "list-ignored_exit"):
                    if key in config:
                        self.assertEqual(result.returncode, config[key])
                self.assert_no_test()
                self.assertFalse(self.held())

    def test_compile_diagnostics_and_failure_status_are_preserved(self):
        self.configure(diagnostics=True, build_exit=41)
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 41, result.stderr)
        for text in ("plain compiler output", "rendered warning", "compiler stderr"):
            self.assertIn(text, result.stderr)
        self.assertEqual(len(self.events()), 1)
        self.assert_no_test()
        self.assertFalse(self.held())

    def test_protected_environment_stdin_and_final_exec_exit_are_preserved(self):
        self.configure(test_exit=43)
        process = self.start("cli", "module::one")
        stdout, stderr = process.communicate("private synthetic stdin\n", timeout=10)
        self.assertEqual(process.returncode, 43, stderr)
        self.assertEqual(stdout, "")
        final = self.events("test")
        self.assertEqual(final[0]["pid"], process.pid, "final harness must replace wrapper")
        self.assertEqual(final[-1]["stdin"], "private synthetic stdin\n")
        for phase in ("list-all", "list-ignored"):
            self.assertEqual(self.events(phase)[-1]["stdin"], "")
        for event in self.events():
            self.assertEqual(event["protected"], {key: self.env[key] for key in PROTECTED})

    def test_enclosing_admission_marker_is_refused_before_compile(self):
        result = self.invoke("cli", "module::one", env=self.env | {"CAGENTS_BUILD_SLOT_HELD": "123"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside cargo-gate", result.stderr)
        self.assertEqual(self.events(), [])

    def test_inherited_held_descriptor_and_hardlink_alias_are_refused(self):
        for suffix in ("", ".1"):
            with self.subTest(slot=suffix):
                path = Path(str(self.lock) + suffix)
                path.touch()
                alias = self.root / ("alias" + (suffix or "-base"))
                alias.hardlink_to(path)
                with alias.open("a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                    result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),))
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("inherited descriptor holds build admission", result.stderr)
                    self.assertEqual(self.events(), [])

    def test_unlocked_inherited_descriptor_does_not_falsely_refuse(self):
        with self.lock.open("a") as lock:
            result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.events("compiler")[0]["held"])
        self.assertFalse(self.events("test")[0]["held"])

    def test_relative_lock_is_resolved_from_the_gate_package_cwd(self):
        with self.lock.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),),
                                 env=self.env | {"CAGENTS_BUILD_LOCK": "slot.lock"})
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("inherited descriptor holds build admission", result.stderr)
        self.assertEqual(self.events(), [])

    def test_empty_or_unset_lock_uses_the_same_default_as_the_gate(self):
        # Remap both copied tools' default to a fixture-owned path: never
        # inspect or acquire the host's default admission for this regression.
        for name in ("test-lib", "cargo-gate"):
            path = self.root / "scripts" / name
            source = path.read_text()
            self.assertIn("/var/tmp/cagents-cargo.lock", source)
            path.write_text(source.replace("/var/tmp/cagents-cargo.lock", str(self.lock)))
        for value in ("", None):
            with self.subTest(value=value), self.lock.open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                environment = self.env.copy()
                if value is None:
                    environment.pop("CAGENTS_BUILD_LOCK")
                else:
                    environment["CAGENTS_BUILD_LOCK"] = value
                result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),),
                                     env=environment)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("inherited descriptor holds build admission", result.stderr)
            self.assertEqual(self.events(), [])

    def test_unrelated_holder_is_waited_for_instead_of_falsely_refused(self):
        with self.lock.open("a") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            process = self.start("cli", "module::one")
            # No descriptor is inherited: this is a legitimate independent
            # owner, not an enclosing lease that would deadlock the wrapper.
            time.sleep(0.15)
            self.assertIsNone(process.poll())
            self.assertEqual(self.events(), [])
            fcntl.flock(holder, fcntl.LOCK_UN)
            _stdout, stderr = process.communicate("", timeout=10)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertTrue(self.events("compiler")[0]["held"])

    def test_unlocked_inherited_fd_to_inode_held_by_unrelated_owner_is_not_refused(self):
        with self.lock.open("a") as unlocked, self.lock.open("a") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            process = self.start("cli", "module::one", pass_fds=(unlocked.fileno(),))
            time.sleep(0.15)
            self.assertIsNone(process.poll(), "unlocked inherited fd falsely treated as its owner's lock")
            self.assertEqual(self.events(), [])
            fcntl.flock(holder, fcntl.LOCK_UN)
            _stdout, stderr = process.communicate("", timeout=10)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertTrue(self.events("compiler")[0]["held"])
        self.assertFalse(self.events("test")[0]["held"])

    def test_executable_replacement_during_either_listing_is_refused(self):
        for phase in ("list-all", "list-ignored"):
            with self.subTest(phase=phase):
                self.clear()
                self.configure(replace=phase)
                result = self.invoke("cli", "module::one")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("test executable changed", result.stderr)
                self.assert_no_test()

    def test_direct_cancellation_waits_for_owned_compiler_and_listing_cleanup(self):
        for phase in ("compiler", "list-all", "list-ignored"):
            for number in SIGNALS:
                with self.subTest(phase=phase, signal=number):
                    self.clear()
                    self.configure(hold=phase)
                    process = self.start("cli", "module::one")
                    self.wait_for(lambda: (self.root / (phase + ".ready")).exists(), phase)
                    self.assertEqual(self.held(), phase == "compiler")
                    process.send_signal(number)
                    self.wait_for(lambda: (self.root / (phase + ".signalled")).exists(), "signal forwarding")
                    self.assertIsNone(process.poll(), "wrapper released a live owned child")
                    self.assertEqual(self.held(), phase == "compiler")
                    (self.root / "release").touch()
                    _stdout, stderr = process.communicate("", timeout=5)
                    self.assertEqual(process.returncode, 128 + number, stderr)
                    received = [event["signal"] for event in self.events(phase) if "signal" in event]
                    self.assertIn(signal.SIGTERM if phase == "compiler" else number, received)
                    self.assertFalse(self.held())
                    self.assert_no_test()

    def test_final_exec_has_ordinary_direct_signal_behavior(self):
        for number in SIGNALS:
            with self.subTest(signal=number):
                self.clear()
                self.configure(hold="test")
                process = self.start("cli", "module::one")
                self.wait_for(lambda: (self.root / "test.ready").exists(), "final harness")
                self.assertEqual(self.events("test")[0]["pid"], process.pid)
                process.send_signal(number)
                self.wait_for(lambda: (self.root / "test.signalled").exists(), "final signal")
                self.assertFalse(self.held())
                (self.root / "release").touch()
                _stdout, stderr = process.communicate("", timeout=5)
                self.assertEqual(process.returncode, 128 + number, stderr)
                self.assertIn(number, [event["signal"] for event in self.events("test") if "signal" in event])


if __name__ == "__main__":
    unittest.main()
