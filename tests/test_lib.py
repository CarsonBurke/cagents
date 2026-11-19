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
import termios
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)
PROTECTED = ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")
# Bounds only a hang: every wait it guards returns as soon as its event happens.
HANG_GUARD = 60

# This program is both fake Cargo and its executable artifact. It never launches
# a compiler, provider or service, and observes only its synthetic environment.
TOOL = r'''#!/usr/bin/python3
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

root = Path(os.environ["TEST_ROOT"])
config = json.loads((root / "control.json").read_text())
program = Path(sys.argv[0]).name
cargo = program == "cargo"
phase = ("metadata" if cargo and sys.argv[1:2] == ["metadata"] else
    "compiler" if cargo and "--no-run" in sys.argv else
    "cargo-run" if cargo else "disk" if program == "df" else
    "prune" if program == "prune-build-dirs" else
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
    event = dict(phase=phase, program=program, pid=os.getpid(), arguments=sys.argv[1:],
                 cwd=os.getcwd(), held=held(),
                 protected={name: os.environ[name] for name in
                            ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")})
    event.update(extra)
    with open(root / "events.jsonl", "a") as output:
        output.write(json.dumps(event) + "\n")

def interrupted(number, _frame):
    record(signal=number)
    (root / (phase + ".signalled")).touch()
    # Until released, or for `linger` seconds when a test wants it to end by itself.
    deadline = time.monotonic() + config.get("linger", config["hang_guard"])
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    raise SystemExit(128 + number)

if phase != "cargo-run":
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
        signal.signal(number, interrupted)
    record()
    (root / (phase + ".ready")).touch()
if config.get("hold") == phase:
    deadline = time.monotonic() + config["hang_guard"]
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    if not (root / "release").exists():
        raise SystemExit(95)

if phase == "metadata":
    if config.get("metadata_exit"):
        raise SystemExit(config["metadata_exit"])
    metadata = {}
    if not config.get("metadata_omit_target_directory"):
        metadata["target_directory"] = str(root / "owned-target")
    if not config.get("metadata_omit_build_directory"):
        metadata["build_directory"] = config.get("metadata_build_directory", str(root / "owned-build"))
    print(json.dumps(metadata, separators=(",", ":")))
    raise SystemExit(0)

if phase == "disk":
    free = config.get("free_gib", 4)
    if (root / "pruned").exists():
        free = config.get("after_prune_free_gib", free)
    print("Filesystem 1024-blocks Used Available Capacity Mounted on")
    print("fixture", config.get("size_gib", 100) * 1048576, 0,
          free * 1048576, "1%", "/fixture-owned")
    raise SystemExit(0)

if phase == "prune":
    (root / "pruned").touch()
    raise SystemExit(config.get("prune_exit", 0))

if phase == "compiler":
    if config.get("diagnostics"):
        print("plain compiler output")
        print("compiler stderr", file=sys.stderr)
    raise SystemExit(config.get("build_exit", 0))

if phase == "cargo-run":
    # Runs the tests as Cargo does, as observed on the build host: the
    # runner's relative path is resolved from Cargo's working directory, it
    # starts in the package's root with CARGO_MANIFEST_DIR set, and gets the
    # harness's path and the arguments after `--`, then `--quiet` for `-q`.
    # Cargo waits through an interrupt and exits with the runner's status.
    arguments = sys.argv[1:]
    split = arguments.index("--") if "--" in arguments else len(arguments)
    options, trailing = arguments[:split], arguments[split + 1:]
    settings = [value for option, value in zip(options, options[1:]) if option == "--config"]
    key = "target.'cfg(all())'.runner="
    runners = [json.loads(value[len(key):]) for value in settings if value.startswith(key)]
    package = options[options.index("--package") + 1]
    manifest = root / "crates/cagents-ui" if package == "cagents-ui" else root
    variant = config.get("artifact", "valid")
    environment = dict(os.environ, CARGO_PKG_NAME=package, CARGO_MANIFEST_DIR=str(manifest))
    environment.update(config.get("cargo_environment", {}))
    if variant == "wrong-package":
        environment["CARGO_PKG_NAME"] = "other"
    elif variant == "wrong-manifest":
        environment["CARGO_MANIFEST_DIR"] = str(root / "other")
    executable = {"relative": "libtest", "missing-file": str(root / "absent"),
                  "directory": str(root / "private-home")}.get(variant, str(root / "libtest"))
    trailing = [word for word in trailing if word not in config.get("cargo_drops", [])]
    if len(runners) != 1:
        raise SystemExit("fixture Cargo needs exactly one runner")
    program, *rest = runners[0]
    if "/" in program:
        program = os.path.join(os.getcwd(), program)
    signal.signal(signal.SIGINT, lambda _number, _frame: None)
    child = subprocess.Popen([program, *rest, executable, *trailing, *config.get("cargo_adds", [])],
                             cwd=manifest, env=environment)
    record(child=child.pid, runner=runners[0], trailing=trailing)
    status = child.wait()
    raise SystemExit(status if status >= 0 else 101)

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

record(stdin="" if config.get("no_stdin") else sys.stdin.read(), metadata={name: os.environ.get(name) for name in
       ("CAGENTS_BUILD_METADATA", "CAGENTS_BUILD_METADATA_SUPPLIED")})
raise SystemExit(config.get("test_exit", 0))
'''


class TestLib(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-test-lib-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("test-lib", "cargo-gate", "cargo-gate-command.py", "build-host"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        for name in ("seed-build-dir", "build-disk-guard"):
            if (ROOT / "scripts" / name).exists():
                shutil.copy2(ROOT / "scripts" / name, scripts / name)
        (self.root / "Cargo.toml").write_text("# private fake Cargo manifest\n")
        # The lanes, as mise.toml defines them, and their check, recorded.
        shutil.copy2(ROOT / "mise.toml", self.root / "mise.toml")
        (scripts / "cargo-lane").write_text(
            "#!/usr/bin/python3\nimport json, os, sys\nfrom pathlib import Path\n"
            "root = Path(os.environ['TEST_ROOT'])\n"
            "with open(root / 'checked.jsonl', 'a') as log:\n"
            "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "sys.exit(json.loads((root / 'control.json').read_text()).get('check_exit', 0))\n")
        (scripts / "cargo-lane").chmod(0o755)
        (self.root / "bin").mkdir()
        for path in (self.root / "bin/cargo", self.root / "libtest"):
            path.write_text(TOOL)
            path.chmod(0o700)
        for name in ("private-home", "private-codex", "caller", "tmp"):
            (self.root / name).mkdir(mode=0o700)
        (self.root / "crates/cagents-ui").mkdir(parents=True)
        self.lock = self.root / "slot.lock"
        self.env = {
            "PATH": str(self.root / "bin") + ":/usr/bin:/bin",
            "HOME": str(self.root / "private-home"),
            "CODEX_HOME": str(self.root / "private-codex"),
            "CAGENTS_AGENT_AUTHORITY": "fixture-agent-authority",
            "CAGENTS_BUILD_LOCK": str(self.lock), "CAGENTS_BUILD_SLOTS": "1",
            "CAGENTS_BUILD_SEED": "0", "CAGENTS_BUILD_FREE_FLOOR_GIB": "0",
            "CARGO_BUILD_JOBS": "1", "TEST_ROOT": str(self.root),
            "CAGENTS_BUILD_INSTALLED_GATE": "", "CAGENTS_BUILD_REMOTE": "local",
            # Build in place: no systemd scope or shared jobserver.
            "CAGENTS_BUILD_SCOPE": "0",
            "TMPDIR": str(self.root / "tmp"), "LC_ALL": "C",
        }
        self.configure()
        self.children = []
        self.addCleanup(self.cleanup_processes)

    def configure(self, **config):
        (self.root / "control.json").write_text(json.dumps({"hang_guard": HANG_GUARD, **config}))

    def cleanup_processes(self):
        (self.root / "release").touch()
        for process in self.children:
            if process.poll() is None:
                # Each wrapper owns a fresh session containing fixture children
                # only. Retaining its unreaped Popen protects this group ID.
                os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=HANG_GUARD)

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
        stdout, stderr = process.communicate(stdin, timeout=HANG_GUARD)
        return subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)

    def events(self, phase=None):
        path = self.root / "events.jsonl"
        events = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        return [event for event in events if phase is None or event["phase"] == phase]

    def clear(self):
        for name in ("events.jsonl", "release", "pruned", "slot.lock.prune", "slot.lock.prune.done"):
            (self.root / name).unlink(missing_ok=True)
        for path in self.root.glob("*.ready"):
            path.unlink()
        for path in self.root.glob("*.signalled"):
            path.unlink()

    def wait_for(self, condition, label, timeout=HANG_GUARD):
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

    def disk_policy(self):
        # Neither a real disk query nor the real pruner beside the guard can
        # be used, and the pruner's schedule would add runs of its own.
        for path in (self.root / "bin/df", self.root / "scripts/prune-build-dirs"):
            path.write_text(TOOL)
            path.chmod(0o700)
        environment = self.env.copy()
        environment.pop("CAGENTS_BUILD_FREE_FLOOR_GIB")
        environment["CAGENTS_BUILD_PRUNE_INTERVAL_HOURS"] = "0"
        environment["CAGENTS_BUILD_PRUNE_STATE"] = str(self.root / "pruner")
        # A complete run just now: below the floor the guard starts no
        # background one, whose events would interleave with these.
        (self.root / "pruner.scheduled").touch()
        return environment

    def runner(self, lane):
        return ["scripts/test-lib", "--cargo-runner", lane]

    def assert_one_build_then_harness_under_cargo(self):
        events = self.events()
        self.assertEqual(len(self.events("metadata")), 1)
        compiler = self.events("compiler")
        self.assertEqual(len(compiler), 1)
        self.assertEqual(len(self.events("cargo-run")), 1)
        after = events[events.index(compiler[0]) + 1:]
        self.assertEqual({event["phase"] for event in after},
                         {"cargo-run", "list-all", "list-ignored", "test"})
        self.assertTrue(all(event["program"] == "libtest"
                            for event in after if event["phase"] != "cargo-run"), after)
        self.assertTrue(all(not event["held"] for event in after), after)
        # The runner Cargo started replaced itself with the harness.
        self.assertEqual(self.events("test")[0]["pid"], self.events("cargo-run")[0]["child"])

    def test_cargo_runs_the_tests_under_a_memory_limit(self):
        ran = self.root / "limited"
        limit = self.root / "scripts/memory-capped"
        limit.write_text("import json, os, sys\n"
                         f"open({str(ran)!r}, 'w').write(json.dumps(sys.argv[1:]))\n"
                         "os.execvp(sys.argv[4], sys.argv[4:])\n")
        limit.chmod(0o755)
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 0, result.stderr)
        # The whole Cargo command that runs the tests is capped, the runner and
        # harness within it, and what they leave running is stopped with them.
        limited = json.loads(ran.read_text())
        self.assertEqual(limited[:5], ["--slice", "app-cagents-test.slice", "--stop-leftovers",
                                       "cargo", "test"])
        self.assertEqual(limited[4:], self.events("cargo-run")[0]["arguments"])
        self.assert_one_build_then_harness_under_cargo()

    def test_isolated_lane_arguments_cwd_profile_and_admission_release(self):
        for lane, profile in (("cli", None), ("host", "dev-runtime"), ("shared-ui", "test"),
                              ("ui", None), ("browser", None)):
            with self.subTest(lane=lane):
                self.clear()
                extra = [] if profile is None else ["--profile", profile]
                result = self.invoke(lane, "module::one", "--test-threads", "2", *extra)
                self.assertEqual(result.returncode, 0, result.stderr)
                events = self.events()
                package = "cagents-ui" if lane in ("ui", "browser") else "cagents"
                options = ["--locked", "--no-default-features", "--manifest-path",
                           str(self.root / "Cargo.toml"), "--package", package, "--lib"]
                options += {"host": ["--features", "host"],
                            "shared-ui": ["--features", "ui"],
                            "browser": ["--features", "browser-protocol-fixture"]}.get(lane, [])
                options += ["--config", "target.'cfg(all())'.runner="
                            + json.dumps(self.runner(lane)), *extra]
                compiler = self.events("compiler")[0]
                self.assertEqual(compiler["arguments"], ["test", "--no-run", *options])
                self.assertTrue(compiler["held"], events)
                cargo = self.events("cargo-run")[0]
                self.assertEqual(cargo["arguments"],
                                 ["test", *options, "--", "--test-lib-selection", "module::one",
                                  "--test-threads", "2"])
                self.assertFalse(cargo["held"], events)
                metadata = self.events("metadata")[0]
                self.assertEqual(metadata["arguments"], ["metadata", "--format-version", "1", "--no-deps"])
                self.assertEqual(events[:2], [metadata, compiler])
                manifest = (self.root / "crates/cagents-ui" if lane in ("ui", "browser")
                            else self.root)
                for event in events:
                    # Cargo runs from the checkout; the tests from their package.
                    self.assertEqual(event["cwd"], str(manifest if event["program"] == "libtest"
                                                       else self.root), event)
                    self.assertEqual(event["protected"], {key: self.env[key] for key in PROTECTED})
                self.assertEqual(self.events("test")[0]["arguments"],
                                 ["--exact", "module::one", "--test-threads", "2"])
                self.assertIn(f"test-lib: running 1 exact {lane} tests", result.stderr)
                self.assert_one_build_then_harness_under_cargo()
                self.assertFalse(self.held())

    def test_disk_guard_refuses_before_compile_or_test_with_private_tools(self):
        environment = self.disk_policy()
        for size, free, floor, prune_exit in ((100, 4, 5, 0), (2000, 63, 64, 0), (100, 4, 5, 17)):
            with self.subTest(size=size, free=free, floor=floor, prune_exit=prune_exit):
                self.clear()
                self.configure(size_gib=size, free_gib=free, prune_exit=prune_exit)
                result = self.invoke("cli", "module::one", env=environment)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(f"below the {floor} GiB floor", result.stderr)
                events = self.events()
                self.assertEqual([event["phase"] for event in events], ["metadata", "disk", "prune", "disk"])
                self.assertTrue(all(not event["held"] for event in events), events)
                pruner = str(self.root / "pruner")
                self.assertEqual(self.events("prune")[0]["arguments"],
                                 ["--apply", "--max-idle", "7", "--lock", pruner + ".lock",
                                  "--log", pruner + ".tsv", "--stamp", pruner + ".scheduled",
                                  "--keep", str(self.root / "owned-build"),
                                  "--orphan-hours", "0", "--evidence-hours", "1", "--budget", "300", "--wait"])
                self.assertEqual(self.events("disk")[0]["arguments"], ["-Pk", "--", str(self.root)])
                self.assertFalse(Path(str(self.lock) + ".queue").exists(), "guard must refuse before admission")
                self.assert_no_test()
                self.assertFalse(self.held())

    def test_private_reclaim_can_allow_one_build_before_direct_tests(self):
        environment = self.disk_policy()
        self.configure(size_gib=100, free_gib=4, after_prune_free_gib=5)
        result = self.invoke("cli", "module::one", env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("reclaiming unused build directories", result.stderr)
        self.assertNotIn("refusing to build", result.stderr)
        self.assertEqual([event["phase"] for event in self.events()[:5]],
                         ["metadata", "disk", "prune", "disk", "compiler"])
        self.assertTrue(self.events("compiler")[0]["held"])
        self.assert_one_build_then_harness_under_cargo()

    def test_failed_or_pathless_metadata_bypasses_disk_guard(self):
        environment = self.disk_policy()
        for config in ({"metadata_exit": 37},
                       {"metadata_build_directory": "", "metadata_omit_target_directory": True},
                       {"metadata_omit_build_directory": True, "metadata_omit_target_directory": True}):
            with self.subTest(config=config):
                self.clear()
                # The fake disk would refuse this build if the guard ran.
                self.configure(size_gib=100, free_gib=0, **config)
                result = self.invoke("cli", "module::one", env=environment)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.events("disk"), [])
                self.assertEqual(self.events("prune"), [])
                self.assertEqual([event["phase"] for event in self.events()[:2]], ["metadata", "compiler"])
                self.assertTrue(self.events("compiler")[0]["held"])
                self.assert_one_build_then_harness_under_cargo()

    def test_disk_guard_uses_the_build_directory_else_the_target(self):
        environment = self.disk_policy()
        for name in ("owned-build", "owned-target"):
            (self.root / name).mkdir()
        for config, guarded in (({}, "owned-build"),
                                ({"metadata_build_directory": ""}, "owned-target"),
                                ({"metadata_omit_build_directory": True}, "owned-target")):
            with self.subTest(config=config):
                self.clear()
                # The fake disk refuses, so the path it was asked about is the guarded one.
                self.configure(size_gib=100, free_gib=0, **config)
                result = self.invoke("cli", "module::one", env=environment)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.events("disk")[0]["arguments"],
                                 ["-Pk", "--", str(self.root / guarded)])
                self.assertEqual(self.events("compiler"), [])

    def test_runner_refuses_another_package_or_an_unrunnable_harness(self):
        for variant in ("wrong-package", "wrong-manifest", "relative", "missing-file",
                        "directory", "unrunnable"):
            with self.subTest(variant=variant):
                self.clear()
                self.configure(artifact=variant)
                (self.root / "libtest").chmod(0o600 if variant == "unrunnable" else 0o700)
                result = self.invoke("cli", "module::one")
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("test-lib:", result.stderr)
                self.assertEqual([event["phase"] for event in self.events()],
                                 ["metadata", "compiler", "cargo-run"])
                self.assertFalse(self.held())

    def test_cargo_quiet_reaches_the_harness_and_other_arguments_are_refused(self):
        self.configure(cargo_adds=["--quiet"])
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.events("test")[0]["arguments"], ["--exact", "module::one", "--quiet"])
        self.clear()
        self.configure(cargo_adds=["--nocapture"])
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual([event["phase"] for event in self.events()],
                         ["metadata", "compiler", "cargo-run"])

    def test_a_runner_without_the_selection_marker_is_refused(self):
        # Cargo prefers a runner configured for the exact target, which would
        # pass the names to the harness itself: the marker makes that fail.
        self.configure(cargo_drops=["--test-lib-selection"])
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("--test-lib-selection", result.stderr)
        self.assert_no_test()

    def test_tests_get_the_callers_stamp_back_instead_of_the_gates(self):
        gate = {"CAGENTS_BUILD_METADATA": "gate-stamp", "CAGENTS_BUILD_METADATA_SUPPLIED": "1"}
        for caller, expected in ((None, None), ("caller.stamp-1", "caller.stamp-1"),
                                 ("not a stamp", None)):
            with self.subTest(caller=caller):
                self.clear()
                self.configure(cargo_environment=gate)
                environment = self.env.copy()
                if caller is not None:
                    environment["CAGENTS_BUILD_METADATA"] = caller
                result = self.invoke("cli", "module::one", env=environment)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.events("test")[-1]["metadata"],
                                 {"CAGENTS_BUILD_METADATA": expected,
                                  "CAGENTS_BUILD_METADATA_SUPPLIED": None})
        # A stamp the gate did not supply is the caller's, and stays.
        self.clear()
        self.configure()
        result = self.invoke("cli", "module::one",
                             env=self.env | {"CAGENTS_BUILD_METADATA": "own"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.events("test")[-1]["metadata"]["CAGENTS_BUILD_METADATA"], "own")

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
        self.assertIn("running 2 exact ui tests", result.stderr)
        self.assertEqual(self.events("test")[0]["arguments"],
                         ["--exact", "module::one", "module::two"])

    def test_interleaved_options_and_ignored_prefix_union_with_ordinary_exact(self):
        self.configure(available=["module::ignored", "other::three"])
        result = self.invoke("shared-ui", "--profile", "dev-runtime", "--prefix", "module::",
                             "other::three", "--test-threads", "3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping ignored module::ignored", result.stderr)
        self.assertEqual(self.events("test")[0]["arguments"],
                         ["--exact", "other::three", "--test-threads", "3"])
        self.assertEqual(self.events("compiler")[0]["arguments"][-2:], ["--profile", "dev-runtime"])
        self.assertEqual(self.events("cargo-run")[0]["trailing"],
                         ["--test-lib-selection", "other::three", "--prefix", "module::",
                          "--test-threads", "3"])

    def test_ignored_explicit_empty_prefix_and_ignored_only_are_refused(self):
        for names, config, message in (
            (("module::ignored",), {}, "explicit ignored tests"),
            (("--prefix", "absent::"), {}, "selects no tests"),
            (("--prefix", "module::"), {"available": ["module::ignored"]}, "no ordinary tests"),
            (("--prefix", "module::"), {"available": [], "ignored": []},
             "lane compiled no library tests"),
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

    def checks(self):
        log = self.root / "checked.jsonl"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def test_passing_tests_are_followed_by_their_lanes_check(self):
        # Built only under cfg(test), an item only tests use looks used; main's
        # build check, of every target, says otherwise.
        lanes = {"cli": ["--locked", "--no-default-features"],
                 "host": ["--locked", "--no-default-features", "--features", "host"],
                 "shared-ui": ["--locked", "--no-default-features", "--package", "cagents",
                               "--package", "cagents-ui", "--features", "cagents/ui"]}
        lanes["ui"] = lanes["shared-ui"]
        lanes["browser"] = ["--locked", "--no-default-features", "--package", "cagents-ui",
                            "--features", "browser-protocol-fixture"]
        for lane, selection in lanes.items():
            with self.subTest(lane=lane):
                (self.root / "checked.jsonl").unlink(missing_ok=True)
                result = self.invoke(lane, "module::one", "--profile", "dev-runtime")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.checks(), [["check", *selection, "--lib", "--bins", "--examples"]])
        # Its status is the task's.
        (self.root / "checked.jsonl").unlink()
        self.configure(check_exit=1)
        self.assertEqual(self.invoke("cli", "module::one").returncode, 1)
        self.assertEqual(len(self.checks()), 1)

    def test_compile_diagnostics_and_failure_status_are_preserved(self):
        self.configure(diagnostics=True, build_exit=41)
        result = self.invoke("cli", "module::one")
        self.assertEqual(result.returncode, 41, result.stderr)
        self.assertIn("plain compiler output", result.stdout)
        self.assertIn("compiler stderr", result.stderr)
        self.assertEqual([event["phase"] for event in self.events()], ["metadata", "compiler"])
        self.assert_no_test()
        self.assertEqual(self.checks(), [])
        self.assertFalse(self.held())

    def test_protected_environment_stdin_and_final_exec_exit_are_preserved(self):
        self.configure(test_exit=43)
        process = self.start("cli", "module::one")
        stdout, stderr = process.communicate("private synthetic stdin\n", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 43, stderr)
        self.assertEqual(stdout, "")
        final = self.events("test")
        self.assertEqual(final[0]["pid"], self.events("cargo-run")[0]["child"],
                         "the harness must replace Cargo's runner")
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

    def test_a_parent_flock_close_is_refused_before_compile(self):
        result = subprocess.run(
            ["flock", "--close", str(self.lock), sys.executable, "-I", "-B",
             str(self.root / "scripts/test-lib"), "cli", "module::one"],
            cwd=self.root / "caller", env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=HANG_GUARD,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("a parent flock holds build admission", result.stderr)
        self.assertEqual(self.events(), [])

    def under_flock(self, slot, *inner, pass_fds=()):
        return subprocess.run(
            ["flock", "--close", str(slot), *inner, sys.executable, "-I", "-B",
             str(self.root / "scripts/test-lib"), "cli", "module::one"],
            cwd=self.root / "caller", env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=HANG_GUARD, pass_fds=pass_fds,
        )

    def test_a_flock_further_up_is_refused_too(self):
        command = f'{sys.executable} -I -B "{self.root}/scripts/test-lib" cli module::one; exit $?'
        result = subprocess.run(
            ["flock", "--close", str(self.lock), "sh", "-c", f"sh -c '{command}'; exit $?"],
            cwd=self.root / "caller", env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=HANG_GUARD,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("a parent flock holds build admission", result.stderr)

    def test_a_flock_on_another_lock_is_not_refused(self):
        result = self.under_flock(self.root / "unrelated.lock")
        self.assertNotIn("a parent flock holds build admission", result.stderr)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_flock_with_the_slot_open_but_not_locked_is_not_refused(self):
        with self.lock.open("a") as opened:
            result = self.under_flock(self.root / "unrelated.lock", pass_fds=(opened.fileno(),))
        self.assertNotIn("a parent flock holds build admission", result.stderr)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unlocked_inherited_descriptor_does_not_falsely_refuse(self):
        with self.lock.open("a") as lock:
            result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.events("compiler")[0]["held"])
        self.assertFalse(self.events("test")[0]["held"])

    def test_numbered_slot_uses_a_literal_configured_filename(self):
        base = self.root / "slot[1]*?.lock"
        slot = Path(str(base) + ".1")
        slot.touch()
        alias = self.root / "literal-slot-alias"
        alias.hardlink_to(slot)
        with alias.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = self.invoke("cli", "module::one", pass_fds=(lock.fileno(),),
                                 env=self.env | {"CAGENTS_BUILD_LOCK": str(base)})
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("inherited descriptor holds build admission", result.stderr)
        self.assertEqual(self.events(), [])

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
            self.wait_for(lambda: Path(str(self.lock) + ".queue").exists(), "metadata then queued admission")
            self.assertIsNone(process.poll())
            self.assertEqual([event["phase"] for event in self.events()], ["metadata"])
            self.assertTrue(self.events("metadata")[0]["held"], "the independent owner still holds admission")
            fcntl.flock(holder, fcntl.LOCK_UN)
            _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertTrue(self.events("compiler")[0]["held"])

    def test_unlocked_inherited_fd_to_inode_held_by_unrelated_owner_is_not_refused(self):
        with self.lock.open("a") as unlocked, self.lock.open("a") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            process = self.start("cli", "module::one", pass_fds=(unlocked.fileno(),))
            self.wait_for(lambda: Path(str(self.lock) + ".queue").exists(), "metadata then queued admission")
            self.assertIsNone(process.poll(), "unlocked inherited fd falsely treated as its owner's lock")
            self.assertEqual([event["phase"] for event in self.events()], ["metadata"])
            self.assertTrue(self.events("metadata")[0]["held"], "the independent owner still holds admission")
            fcntl.flock(holder, fcntl.LOCK_UN)
            _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
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

    def test_cancellation_waits_for_the_owned_compiler(self):
        for number in SIGNALS:
            with self.subTest(signal=number):
                self.clear()
                self.configure(hold="compiler")
                process = self.start("cli", "module::one")
                self.wait_for(lambda: (self.root / "compiler.ready").exists(), "compiler")
                self.assertTrue(self.held())
                process.send_signal(number)
                self.wait_for(lambda: (self.root / "compiler.signalled").exists(), "signal forwarding")
                self.assertIsNone(process.poll(), "wrapper released a live owned child")
                self.assertTrue(self.held())
                (self.root / "release").touch()
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                # The gate ends as the signal it was sent, once its build has.
                self.assertEqual(process.returncode, -number, stderr)
                # The build host interrupts the build, whatever it was sent.
                received = [event["signal"] for event in self.events("compiler") if "signal" in event]
                self.assertEqual(received, [signal.SIGINT])
                self.assertFalse(self.held())
                self.assertEqual(self.events("cargo-run"), [])

    def test_a_signal_to_test_lib_reaches_a_listing_and_runs_no_tests(self):
        # Locally the gate becomes Cargo, which ignores an interrupt; test-lib
        # passes a signal sent to it alone to the whole command.
        for phase in ("list-all", "list-ignored"):
            for number in SIGNALS:
                with self.subTest(phase=phase, signal=number):
                    self.clear()
                    self.configure(hold=phase)
                    process = self.start("cli", "module::one")
                    self.wait_for(lambda: (self.root / (phase + ".ready")).exists(), phase)
                    process.send_signal(number)
                    self.wait_for(lambda: (self.root / (phase + ".signalled")).exists(), "the signal")
                    (self.root / "release").touch()
                    _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                    self.assertEqual(process.returncode,
                                     128 + number if number == signal.SIGINT else -number, stderr)
                    self.assertIn(number, [event["signal"] for event in self.events(phase)
                                           if "signal" in event])
                    self.assert_no_test()

    def test_a_signal_to_test_lib_reaches_the_harness(self):
        for number in SIGNALS:
            with self.subTest(signal=number):
                self.clear()
                self.configure(hold="test")
                process = self.start("cli", "module::one")
                self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
                process.send_signal(number)
                self.wait_for(lambda: (self.root / "test.signalled").exists(), "the signal")
                (self.root / "release").touch()
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                self.assertEqual(process.returncode,
                                 128 + number if number == signal.SIGINT else -number, stderr)
                self.assertIn(number, [event["signal"] for event in self.events("test")
                                       if "signal" in event])

    def test_test_lib_outlives_what_a_signal_left_running(self):
        # Cargo ends at once on SIGTERM while the harness still handles it,
        # then ends by itself, unreleased, well within the 5 seconds test-lib
        # waits for its group; returning sooner would leave it running.
        self.configure(hold="test", linger=1)
        process = self.start("cli", "module::one")
        self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
        harness = self.events("test")[0]["pid"]
        process.send_signal(signal.SIGTERM)
        self.wait_for(lambda: (self.root / "test.signalled").exists(), "the signal")
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, -signal.SIGTERM, stderr)
        with self.assertRaises(ProcessLookupError):
            os.kill(harness, 0)

    def in_terminal(self):
        """test-lib as the foreground job of a terminal of its own."""
        controller, terminal = os.openpty()
        self.addCleanup(os.close, controller)

        def take_terminal():
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            process = subprocess.Popen(
                [sys.executable, "-I", "-B", str(self.root / "scripts/test-lib"), "cli",
                 "module::one"],
                cwd=self.root / "caller", env=self.env, stdin=terminal, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, start_new_session=True,
                preexec_fn=take_terminal)
        finally:
            os.close(terminal)
        self.children.append(process)
        return process, controller

    def test_in_a_terminal_ctrl_c_and_a_direct_signal_each_reach_the_harness_once(self):
        for how in ("ctrl-c", "direct"):
            with self.subTest(how=how):
                self.clear()
                self.configure(hold="test")
                process, controller = self.in_terminal()
                self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
                if how == "ctrl-c":
                    os.write(controller, b"\x03")
                else:
                    process.send_signal(signal.SIGINT)
                self.wait_for(lambda: (self.root / "test.signalled").exists(), "the interrupt")
                time.sleep(0.2)
                (self.root / "release").touch()
                _stdout, stderr = process.communicate(timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 128 + signal.SIGINT, stderr)
                self.assertEqual([event["signal"] for event in self.events("test")
                                  if "signal" in event], [signal.SIGINT])

    def test_in_a_terminal_ctrl_z_stops_the_job_and_it_continues(self):
        self.configure(hold="test")
        process, controller = self.in_terminal()
        self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
        harness = self.events("test")[0]["pid"]
        state = lambda pid: Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        os.write(controller, b"\x1a")
        self.wait_for(lambda: state(process.pid) == "T", "test-lib to stop with its job")
        self.assertEqual(state(harness), "T")
        os.kill(process.pid, signal.SIGCONT)
        self.wait_for(lambda: state(harness) != "T", "the job to continue")
        (self.root / "release").touch()
        # The harness reads the terminal, which it has again: end its input.
        os.write(controller, b"\x04")
        _stdout, stderr = process.communicate(timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, stderr)

    def test_a_job_continued_in_the_background_leaves_the_shell_its_terminal(self):
        # A minimal job-control shell: it gives test-lib the terminal, takes
        # it back when Ctrl-Z stops the job, and continues it as `bg` or `fg`.
        shell = self.root / "shell.py"
        shell.write_text(r"""
import fcntl, json, os, signal, subprocess, sys, termios, time
fcntl.ioctl(0, termios.TIOCSCTTY, 0)
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
mode, report, guard, *command = sys.argv[1:]
job = subprocess.Popen(command, process_group=0)
os.tcsetpgrp(0, job.pid)
_pid, status = os.waitpid(job.pid, os.WUNTRACED)
stopped = os.WIFSTOPPED(status)
os.tcsetpgrp(0, os.getpgrp())
if mode == "fg":
    os.tcsetpgrp(0, job.pid)
os.killpg(job.pid, signal.SIGCONT)
if mode == "fg":
    # Continued in the foreground, test-lib hands the terminal on to its command.
    # The guard is shorter than the test's, so a handover that never happens
    # is reported by the `resumed` assertion rather than a wait timing out.
    deadline = time.monotonic() + float(guard)
    while os.tcgetpgrp(0) == job.pid and time.monotonic() < deadline:
        time.sleep(0.01)
else:
    # In the background it must leave the terminal alone; give it a chance not to.
    time.sleep(0.5)
resumed = os.tcgetpgrp(0)
open(sys.argv[2] + ".resumed", "w").close()
_pid, status = os.waitpid(job.pid, 0)
json.dump(dict(stopped=stopped, shell=os.getpgrp(), job=job.pid, resumed=resumed,
               ended=os.tcgetpgrp(0), status=os.waitstatus_to_exitcode(status)),
          open(report, "w"))
""")
        for mode in ("bg", "fg"):
            with self.subTest(mode=mode):
                self.clear()
                self.configure(hold="test", no_stdin=True)
                report = self.root / f"report-{mode}.json"
                controller, terminal = os.openpty()
                self.addCleanup(os.close, controller)
                try:
                    process = subprocess.Popen(
                        [sys.executable, "-I", "-B", str(shell), mode, str(report), str(HANG_GUARD / 2),
                         sys.executable, "-I", "-B", str(self.root / "scripts/test-lib"), "cli",
                         "module::one"],
                        cwd=self.root / "caller", env=self.env, stdin=terminal,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                        start_new_session=True)
                finally:
                    os.close(terminal)
                self.children.append(process)
                self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
                os.write(controller, b"\x1a")
                self.wait_for(lambda: Path(str(report) + ".resumed").exists(), "the job to continue")
                (self.root / "release").touch()
                _stdout, stderr = process.communicate(timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 0, stderr)
                result = json.loads(report.read_text())
                self.assertTrue(result["stopped"], result)
                self.assertEqual(result["status"], 0, stderr)
                self.assertEqual(result["resumed"], result["shell"] if mode == "bg" else
                                 self.events("cargo-run")[0]["pid"], result)
                # Ended, the job hands back only a terminal it held.
                self.assertEqual(result["ended"], result["shell"] if mode == "bg" else result["job"],
                                 result)

    def test_an_interrupt_during_a_listing_waits_for_it_and_runs_no_tests(self):
        # A terminal interrupts the whole foreground group. Cargo waits for
        # its runner, which waits for the listing it started.
        for phase in ("list-all", "list-ignored"):
            with self.subTest(phase=phase):
                self.clear()
                self.configure(hold=phase)
                process = self.start("cli", "module::one")
                self.wait_for(lambda: (self.root / (phase + ".ready")).exists(), phase)
                self.assertFalse(self.held())
                os.killpg(process.pid, signal.SIGINT)
                self.wait_for(lambda: (self.root / (phase + ".signalled")).exists(), "the interrupt")
                time.sleep(0.2)
                self.assertIsNone(process.poll(), "runner left before its listing ended")
                (self.root / "release").touch()
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 128 + signal.SIGINT, stderr)
                self.assertIn(signal.SIGINT, [event["signal"] for event in self.events(phase)
                                              if "signal" in event])
                self.assert_no_test()

    def test_the_harness_has_ordinary_signal_behavior(self):
        for number in SIGNALS:
            with self.subTest(signal=number):
                self.clear()
                self.configure(hold="test")
                process = self.start("cli", "module::one")
                self.wait_for(lambda: (self.root / "test.ready").exists(), "the harness")
                self.assertEqual(self.events("test")[0]["pid"], self.events("cargo-run")[0]["child"])
                os.killpg(process.pid, number)
                self.wait_for(lambda: (self.root / "test.signalled").exists(), "the signal")
                self.assertFalse(self.held())
                (self.root / "release").touch()
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                # Cargo exits with the harness's status after an interrupt;
                # another signal ends Cargo itself, as it would end real Cargo.
                self.assertEqual(process.returncode,
                                 128 + number if number == signal.SIGINT else -number, stderr)
                self.assertIn(number, [event["signal"] for event in self.events("test")
                                       if "signal" in event])

if __name__ == "__main__":
    unittest.main()
