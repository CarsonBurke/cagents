#!/usr/bin/env python3
"""Exercise build offloading with a stand-in build host on this machine.

The stand-in `ssh` runs each remote command here under a private home, so
the build slots, the copied sources and the copied-back executables are all
in a temporary directory. Cargo builds only a tiny private crate.
"""

import importlib.machinery
import importlib.util
import ast
import base64
import contextlib
import fcntl
import json
import os
import pwd
import resource
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]


# Isolated Python (-I) leaves this directory off sys.path.
_custody_spec = importlib.util.spec_from_file_location("fixture_custody", ROOT / "tests/fixture_custody.py")
_custody = importlib.util.module_from_spec(_custody_spec)
_custody_spec.loader.exec_module(_custody)
FixtureDirectory = _custody.FixtureDirectory


def records(registry):
    """A build registry's records of running builds, without its decision
    lock or its records of waiting ones."""
    return [path for path in registry.iterdir() if path.name.startswith(("gate-", "offload-"))]


def load_offload():
    loader = importlib.machinery.SourceFileLoader("cargo_offload", str(ROOT / "scripts/cargo-offload"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


offload = load_offload()
# What these tests start must not price, or record itself among, the real
# builds on the host running them; a test about the registry names its own.
os.environ["CAGENTS_BUILD_REGISTRY"] = ""


def setUpModule():
    """Fails the suite if it leaves anything in the runtime directories'
    `cof`, where the build host's agent puts each slot's temporary files by
    default: the one this environment names, and the login session's, which
    the loopback gives the agent. A test's agent keeps them in its own
    directory through CAGENTS_OFFLOAD_TEMPORARY_ROOT."""
    roots = {Path(os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}", "cof"),
             Path(f"/run/user/{os.getuid()}", "cof")}

    def names(root):
        with contextlib.suppress(FileNotFoundError):
            return {path.name for path in root.iterdir()}
        return set()

    def real_slots(root):
        """The names this host's own slots take there: a build using a slot
        for the first time while the suite runs adds one."""
        temporary_directory = agent_function("temporary_directory")
        with patch.dict(os.environ, {"CAGENTS_OFFLOAD_TEMPORARY_ROOT": str(root)}):
            return {os.path.basename(temporary_directory(str(slot))) for slot in
                    Path("~/.cache/cagents-offload").expanduser().glob("*/slot-*")}

    before = {root: names(root) for root in roots}

    def unchanged():
        left = {}
        for root, known in before.items():
            if new := names(root) - known:
                if new := new - real_slots(root):
                    left[str(root)] = sorted(new)
        if left:
            raise AssertionError(f"the tests left temporary directories outside their own: {left}")
    unittest.addModuleCleanup(unchanged)


def write_figures(directory, *, available_gib=1024, cpu=0, memory=0):
    """A build host's memory and CPU figures, standing in for /proc. Unless a
    test is about them, an idle, roomy host: admission otherwise holds builds
    back by the real figures, which change as tests run."""
    directory = Path(directory)
    (directory / "pressure").mkdir(parents=True, exist_ok=True)
    (directory / "meminfo").write_text(f"MemAvailable: {available_gib * 2**20} kB\n")
    for resource_name, value in (("cpu", cpu), ("memory", memory)):
        (directory / "pressure" / resource_name).write_text(
            f"some avg10={value:.2f} avg60=0.00 avg300=0.00 total=0\n")


class Parsing(unittest.TestCase):
    def test_a_missing_memory_helper_refuses_before_admission(self):
        with patch.dict(os.environ, {"CAGENTS_BUILD_OFFLOAD_HOST": "fixture-host"}), \
                patch.object(Path, "read_text", side_effect=FileNotFoundError("missing helper")), \
                patch.object(offload, "say") as say:
            self.assertEqual(offload.main(["test"]), offload.REFUSED)
        self.assertIn("require memory-capped", say.call_args.args[0])

    def test_only_commands_that_execute_tests_need_a_memory_cap(self):
        for arguments, expected in ((["test"], True), (["t", "--doc"], True),
                                    (["bench"], True), (["test", "--no-run"], False),
                                    (["bench", "--no-run"], False), (["build"], False)):
            with self.subTest(arguments=arguments):
                self.assertEqual(offload.Plan(arguments).runs_tests, expected)

    def test_commands_that_compile_run_remotely(self):
        for arguments, subcommand in (
            (["build", "--locked", "--profile", "dev-runtime", "--bin", "cagents"], "build"),
            (["b", "--release"], "build"),
            (["check", "--locked", "--no-default-features", "--features", "ui"], "check"),
            (["clippy", "--all-targets", "--", "-D", "warnings"], "clippy"),
            (["test", "--locked", "--all-targets", "--", "--test-threads=1"], "test"),
            (["t", "-p", "cagents", "--", "/a/test/filter"], "test"),
            (["doc", "--no-deps"], "doc"),
            (["run", "--bin", "cagents"], "run"),
            (["bench"], "bench"),
            (["+nightly", "build"], "build"),
            (["fix", "--allow-dirty"], "fix"),
            (["test", "--no-run", "--message-format=json"], "test"),
        ):
            with self.subTest(arguments=arguments):
                self.assertEqual(offload.parse(arguments).subcommand, subcommand)

    def test_commands_that_compile_nothing_run_here(self):
        for arguments in (["metadata"], ["fmt"], ["tree"], ["clean"], ["+nightly", "fmt"],
                          ["--version"], []):
            with self.subTest(arguments=arguments):
                self.assertIsNone(offload.parse(arguments))

    def test_commands_that_may_compile_elsewhere_are_refused(self):
        for arguments in (["install", "ripgrep"], ["nextest", "run"], ["--locked", "build"],
                          ["doc", "--open", "--message-format=json"]):
            with self.subTest(arguments=arguments):
                with self.assertRaises(offload.Refusal) as refusal:
                    offload.parse(arguments)
                self.assertIn("CAGENTS_BUILD_REMOTE=local", str(refusal.exception))

    def test_run_passes_its_bare_and_trailing_arguments_to_the_program(self):
        plan = offload.parse(["run", "--bin", "x", "-p", "y", "first", "--flag", "--", "z"])
        self.assertEqual(plan.program, ["first", "--flag", "--", "z"])
        self.assertEqual(plan.arguments(lambda _o, v: v, []),
                         ["run", "--bin", "x", "-p", "y", "--message-format=json-render-diagnostics"])
        self.assertEqual(offload.parse(["run", "--", "a", "b"]).program, ["a", "b"])

    def test_paths_are_translated_and_test_arguments_kept(self):
        plan = offload.parse(["test", "--manifest-path=a/Cargo.toml", "--target-dir", "t",
                              "--config", "build.jobs=2", "--config", "c.toml", "--", "/x"])
        seen = []

        def translate(option, value):
            seen.append((option, value))
            return "R/" + value
        self.assertEqual(plan.arguments(translate, []),
                         ["test", "--manifest-path=R/a/Cargo.toml", "--target-dir", "R/t",
                          "--config", "R/build.jobs=2", "--config", "R/c.toml", "--", "/x"])
        self.assertIn(("--config", "build.jobs=2"), seen)

    def test_messages_this_side_reads_keep_the_callers_diagnostics(self):
        def words(*arguments):
            return offload.parse(list(arguments)).arguments(lambda _o, v: v, [])
        self.assertEqual(words("build"), ["build", "--message-format=json-render-diagnostics"])
        self.assertEqual(words("build", "--message-format", "short"),
                         ["build", "--message-format=json-render-diagnostics,json-diagnostic-short"])
        self.assertEqual(words("test", "--no-run", "--message-format=json"),
                         ["test", "--no-run", "--message-format=json"])
        self.assertEqual(words("check", "--message-format=short"),
                         ["check", "--message-format=short"])
        self.assertEqual(words("doc", "--open"), ["doc", "--message-format=json-render-diagnostics"])
        self.assertEqual(words("test", "--", "--nocapture"), ["test", "--", "--nocapture"])


class BuildIdentity(unittest.TestCase):
    def identity(self, *arguments, environment=None, directory="."):
        return offload.Offload("host", offload.parse(list(arguments)),
                               environment or {}).build_identity(directory)

    def test_a_build_is_the_same_whatever_tests_it_would_pick(self):
        self.assertEqual(self.identity("test", "--no-run", "--lib", "app::"),
                         self.identity("test", "--no-run", "--lib", "--", "chat::"))
        self.assertNotEqual(self.identity("test", "--no-run", "--lib"),
                            self.identity("test", "--no-run", "--lib", "--features", "ui"))
        self.assertNotEqual(self.identity("check"),
                            self.identity("check", environment={"RUSTFLAGS": "-Dwarnings"}))

    def test_a_build_is_the_same_however_it_is_named_or_reported(self):
        self.assertEqual(self.identity("b"), self.identity("build"))
        self.assertEqual(self.identity("check"),
                         self.identity("check", environment={"CARGO_TERM_COLOR": "always",
                                                             "RUST_BACKTRACE": "1"}))

    def test_a_build_from_another_directory_of_the_checkout_is_another_build(self):
        self.assertNotEqual(self.identity("check", directory="crates/a"),
                            self.identity("check", directory="crates/b"))

    def test_a_command_that_runs_tests_there_is_no_build_to_wait_for(self):
        self.assertEqual(self.identity("test", "--lib"), "")


class Rewrite(unittest.TestCase):
    def test_the_build_hosts_paths_become_this_hosts(self):
        mappings = [("/h/slot-0/src/target", "/c/target"), ("/h/slot-0/src", "/c")]
        message = {"executable": "/h/slot-0/src/target/debug/x", "src_path": "/h/slot-0/src/src/main.rs",
                   "filenames": ["/h/slot-0/src-other/y"], "fresh": True, "n": 1}
        self.assertEqual(offload.rewrite(message, mappings), {
            "executable": "/c/target/debug/x", "src_path": "/c/src/main.rs",
            "filenames": ["/h/slot-0/src-other/y"], "fresh": True, "n": 1})

    def test_search_paths_from_build_scripts_name_this_hosts_directories(self):
        mappings = [("/h/slot-0/src/target", "/c/target")]
        self.assertEqual(offload.rewrite({"linked_paths": ["native=/h/slot-0/src/target/x", "/usr/lib"]},
                                         mappings),
                         {"linked_paths": ["native=/c/target/x", "/usr/lib"]})
        self.assertEqual(offload.rewrite("a=b", mappings), "a=b")

    def test_path_package_ids_name_this_hosts_checkout(self):
        mappings = [("/h/slot-0/src", "/c")]
        self.assertEqual(offload.rewrite("path+file:///h/slot-0/src/crates/x#x@0.1.0", mappings),
                         "path+file:///c/crates/x#x@0.1.0")
        self.assertEqual(offload.rewrite("registry+https://github.com/x#y@1", mappings),
                         "registry+https://github.com/x#y@1")


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
            self.message(f"{self.target}/debug/deps/cagents-4567", ["lib"]),
            self.message(f"{self.target}/dev-runtime/cagents", ["bin"], reason="build-finished"),
            "warning: not a message",
            "{not json",
        ]
        self.assertEqual(offload.artifacts(messages, [self.target]), {self.target: [
            "dev-runtime/cagents-ui", "debug/examples/demo", "debug/deps/probe-0123",
            "debug/deps/cagents-4567",
        ]})

    def test_test_harnesses_in_a_separate_build_directory_are_listed_by_it(self):
        build = "/var/tmp/cargo-build/home/u/.cache/cagents-offload/r/slot-1/src/target"
        messages = [self.message(f"{self.target}/debug/probe", ["bin"]),
                    self.message(f"{build}/debug/build/probe/0123/out/probe-0123", ["test"])]
        self.assertEqual(offload.artifacts(messages, [self.target, build, self.target]), {
            self.target: ["debug/probe"], build: ["debug/build/probe/0123/out/probe-0123"]})

    def test_executables_outside_the_target_are_not_copied(self):
        messages = [self.message("/usr/bin/true", ["bin"]),
                    self.message(f"{self.target}/../../escape", ["bin"]),
                    self.message(f"{self.target}-other/debug/x", ["bin"])]
        self.assertEqual(offload.artifacts(messages, [self.target]), {})


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


def agent_function(name):
    """The build host's agent's top-level function `name`, on its own, with
    the modules its functions use."""
    import hashlib, stat
    tree = ast.parse(offload.REMOTE_AGENT)
    function = next(node for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name == name)
    scope = {"os": os, "hashlib": hashlib, "shutil": shutil, "stat": stat, "sys": sys}
    exec(compile(ast.Module([function], []), "agent", "exec"), scope)
    return scope[name]


class Resident(unittest.TestCase):
    def test_a_process_tree_holds_its_descendants_memory(self):
        with FixtureDirectory() as scope:
            scope.popen([sys.executable, "-c", "b = bytearray(64 << 20); import time; time.sleep(30)"])
            time.sleep(1)
            held = agent_function("resident")([os.getpid(), 999999999])
            self.assertGreater(held[os.getpid()], 64 << 20)
            self.assertEqual(held[999999999], 0)


def agent_scope():
    """The host agent's definitions, as its commands run them."""
    scope = {"__name__": "cargo-offload-agent"}
    with patch.object(sys, "argv", ["cargo-offload", "test", "~"]):
        exec(compile(offload.REMOTE_AGENT.split("\nif command == ")[0], "agent", "exec"), scope)
    return scope


class Busiest(unittest.TestCase):
    """What holds the CPUs, from two process tables standing in for /proc's
    a second apart. Their PIDs are beyond any the kernel gives, so nothing of
    theirs is in /proc: each command is the name it was sampled with."""

    TICKS = os.sysconf("SC_CLK_TCK")

    def process(self, pid, *, parent=1, session=None, terminal=0, own=0.0, reaped=0.0, age=30,
                name="spin"):
        return pid, self.scope["Process"](
            parent, pid if session is None else session, terminal, round(own * self.TICKS),
            round(reaped * self.TICKS), round((self.up - age - 0.5) * self.TICKS), name)

    def busiest(self, first, second, *, seconds=1.0, **options):
        """`busiest` over tables of CPU seconds, `seconds` apart by the clock
        it reads around its own sleep."""
        tables = iter([dict(first), dict(second)])
        clock = iter([0.0, 0.0, seconds, seconds])
        self.scope["process_table"] = lambda: next(tables)
        self.scope["time"] = type("Clock", (), {"monotonic": staticmethod(lambda: next(clock)),
                                                "sleep": staticmethod(lambda _: None)})
        return self.scope["busiest"](**options)

    def setUp(self):
        self.scope = agent_scope()
        # A host of two CPUs, whatever this one has.
        self.scope["host_cpus"] = lambda: 2
        self.up = float(Path("/proc/uptime").read_text().split()[0])

    def test_a_loop_of_short_lived_commands_is_named_by_what_it_reaps(self):
        loop, child = 5_000_001, 5_000_002
        text = self.busiest(
            [self.process(loop, own=1, reaped=5, name="sh"),
             self.process(child, parent=loop, session=loop, own=10)],
            # The child ended after 0.5 more seconds, another lived 0.4 of
            # them, and the loop itself used 0.1: only what the sample saw.
            [self.process(loop, own=1.1, reaped=5 + 10.5 + 0.4, name="sh")])
        self.assertRegex(text, rf"; besides builds, the CPUs go to session {loop} "
                               rf"\(sh, started 3\ds ago\): 1\.0 CPU in 1 process\. ")
        self.assertTrue(text.endswith(f". `ps -s {loop}` lists session {loop}'s processes, "
                                      f"and `pkill -s {loop}` ends them"), text)

    def test_what_ended_within_the_sample_is_taken_off_the_ancestor_that_outlived_it(self):
        # A shell runs make, which runs a compiler an hour in; both end.
        shell, make, compiler = 5_000_003, 5_000_004, 5_000_005
        text = self.busiest(
            [self.process(shell, name="sh"), self.process(make, parent=shell, session=shell, own=2),
             self.process(compiler, parent=make, session=shell, own=3600)],
            [self.process(shell, reaped=2 + 3600 + 0.5, name="sh")])
        self.assertIn(f"session {shell} (sh, started 30s ago): 0.5 CPUs in 1 process", text)
        # Nothing is said to use more than every CPU, however its ticks came.
        text = self.busiest([self.process(shell, name="sh")],
                            [self.process(shell, reaped=10**6, name="sh")])
        self.assertIn(": 2.0 CPUs in 1 process", text)

    def test_a_session_whose_leader_has_exited_says_so(self):
        session, orphan = 5_000_010, 5_000_011
        text = self.busiest([self.process(orphan, session=session, age=45, name="runaway")],
                            [self.process(orphan, session=session, own=1, age=45, name="runaway")])
        self.assertIn(f"session {session}, whose leader has exited: 1.0 CPU in 1 process, "
                      f"most in pid {orphan} (runaway, started 45s ago)", text)

    def test_a_command_reaches_the_terminal_without_its_control_characters(self):
        loop = 5_000_012
        name = "spin\x1b]0;owned\x07\x1b[2Jloop"
        text = self.busiest([self.process(loop, name=name)],
                            [self.process(loop, own=1, name=name)])
        self.assertIn("(spin ]0;owned [2Jloop, started", text)
        self.assertFalse(any(not char.isprintable() for char in text), repr(text))

    def test_the_running_builds_holding_the_cpus_is_said_outright(self):
        build, detached, other = 5_000_020, 5_000_021, 5_000_022
        first = [self.process(build), self.process(detached, parent=build), self.process(other)]
        second = [self.process(build, own=2), self.process(detached, parent=build, own=1),
                  self.process(other, own=0.1)]
        self.assertEqual(self.busiest(first, second, pids=[build]),
                         "nothing else uses much; the running builds hold them: "
                         "builds use 3.0 CPUs and everything else 0.1 CPUs")
        # One build compiling a crate on one CPU holds them as surely.
        self.assertEqual(self.busiest(first, [self.process(build, own=1.0)], pids=[build]),
                         "nothing else uses much; the running builds hold them: "
                         "builds use 1.0 CPU and everything else 0.0 CPUs")
        self.assertEqual(self.busiest(first, [self.process(other, own=0.1)]),
                         "nothing uses much at the moment: builds use 0.0 CPUs and everything else "
                         "0.1 CPUs, of 2 in all")
        # Pressure averages ten seconds, so it can outlast a burst: builds
        # leaving most CPUs idle don't hold them.
        self.scope["host_cpus"] = lambda: 16
        self.assertEqual(self.busiest(first, [self.process(build, own=3)], pids=[build]),
                         "nothing uses much at the moment: builds use 3.0 CPUs and everything else "
                         "0.0 CPUs, of 16 in all")

    def test_the_rate_is_over_the_time_the_sample_took(self):
        sessions = [5_000_030 + index for index in range(5)]
        first = [self.process(pid) for pid in sessions]
        # Four sessions use 2 CPU seconds each and one 0.6, over 2 seconds.
        second = [self.process(pid, own=2) for pid in sessions[:4]] + [self.process(sessions[4], own=0.6)]
        with patch.dict(os.environ, {"CAGENTS_OFFLOAD_BUSIEST_SHOWN": "a few",
                                     "CAGENTS_OFFLOAD_BUSIEST_LEAST": "nan"}):
            text = self.busiest(first, second, seconds=2.0, there=True)
        self.assertTrue(text.startswith("builds use 0.0 CPUs and everything else 4.3 CPUs; "), text)
        # Settings that are not numbers show the default three of at least
        # half a CPU.
        self.assertEqual(text.count(": 1.0 CPU in 1 process"), 3, text)
        self.assertIn("; and 2 more sessions using 1.3 CPUs. `ps -s ", text)
        self.assertRegex(text, r"`ps -s \d+` there lists session \d+'s processes, and `pkill -s \d+` ends them$")

    def test_no_session_a_person_works_in_is_offered_to_end_whole(self):
        def idle(name, **env):
            """A real process, for what busiest reads of it from /proc."""
            directory = Path(tempfile.mkdtemp(prefix="cagents-busiest-"))
            self.addCleanup(shutil.rmtree, directory, True)
            program = directory / name
            shutil.copy2(shutil.which("sleep"), program)
            process = subprocess.Popen([program, "60"], env=os.environ | env)
            self.addCleanup(lambda: (process.kill(), process.wait()))
            return process.pid

        terminal, own, kernel, starter, leaderless = 5_000_060, 5_000_061, 5_000_062, 5_000_063, 5_000_064
        desktop = idle("sleep", XDG_SESSION_TYPE="wayland")
        daemon = idle("cagentsd")
        cases = (
            ([self.process(terminal, terminal=34816, name="bash")], terminal, "a terminal's"),
            ([self.process(own, session=os.getsid(0))], os.getsid(0), "the one this wait runs in"),
            # What started this wait, such as the agent waiting, in a session of its own.
            ([(os.getpid(), self.scope["Process"](starter, os.getsid(0), 0, 0, 0, 0, "python3")),
              self.process(starter, session=starter, name="agent")], starter, "the one this wait runs in"),
            # A daemon whose leader has exited, started from the desktop.
            ([self.process(desktop, session=leaderless)], leaderless, "one started from the desktop"),
            # cagents' own daemon, whatever started it.
            ([self.process(daemon, session=leaderless)], leaderless, "cagents' own"),
        )
        for table, session, why in cases:
            with self.subTest(why):
                busy = [(pid, entry._replace(own=entry.own + self.TICKS)) for pid, entry in table]
                text = self.busiest(table, busy)
                self.assertIn(f". Session {session} is {why}: `ps -s {session}` lists its processes, "
                              f"to end only what you recognise", text)
                self.assertNotIn("pkill", text)
        # The kernel's threads are named, though no one can end them.
        text = self.busiest([self.process(kernel, parent=2, session=0, name="kswapd0")],
                            [self.process(kernel, parent=2, session=0, own=1.5, name="kswapd0")])
        self.assertEqual(text, "builds use 0.0 CPUs and everything else 1.5 CPUs; besides builds, "
                               "the CPUs go to the kernel's own threads: 1.5 CPUs")

    def test_a_session_using_under_half_a_cpu_is_not_named(self):
        quiet, builds = 5_000_040, 5_000_041
        text = self.busiest([self.process(quiet), self.process(builds)],
                            [self.process(quiet, own=0.4), self.process(builds, own=1)], pids=[builds])
        self.assertEqual(text, "nothing else uses much; the running builds hold them: "
                               "builds use 1.0 CPU and everything else 0.4 CPUs")
        many = [5_000_050 + index for index in range(10)]
        text = self.busiest([self.process(pid) for pid in many],
                            [self.process(pid, own=0.3) for pid in many])
        self.assertEqual(text, "builds use 0.0 CPUs and everything else 3.0 CPUs, "
                               "though no session outside them uses half a CPU")


# Sockets bind under these tests' directories, whose paths must stay short
# however long TMPDIR is.
SOCKET_ROOT = "/tmp"


class SharedConnection(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cof-", dir=SOCKET_ROOT)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.log = self.root / "ssh.log"
        tools = self.root / "tools"
        tools.mkdir()
        (tools / "ssh").write_text(f'#!/bin/sh\necho "$@" >> {self.log}\n')
        (tools / "ssh").chmod(0o755)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.environment = {"XDG_RUNTIME_DIR": str(self.runtime),
                            "PATH": f"{tools}:{os.environ['PATH']}"}
        old_path = os.environ["PATH"]
        os.environ["PATH"] = self.environment["PATH"]
        self.addCleanup(os.environ.__setitem__, "PATH", old_path)

    def build(self, environment=None):
        return offload.Offload("host", offload.parse(["check"]), environment or self.environment)

    def test_one_builds_steps_share_a_private_connection_that_ends_with_it(self):
        build = self.build()
        build.share_connection()
        command = " ".join(build.remote("exists", "x"))
        path = command.split("ControlPath=")[1].split()[0]
        self.assertTrue(path.startswith(f"{self.runtime}/cof-ssh-{os.getpid()}-"))
        self.assertLessEqual(len(path) + 17, 107)
        self.assertIn("ControlMaster=auto", command)
        other = self.build()
        other.share_connection()
        self.assertNotEqual(other.control, build.control)
        other.close_connection()
        directory = build.control
        build.close_connection()
        self.assertFalse(os.path.exists(directory))
        self.assertIn(f"ControlPath={path} -o ControlPersist=60 -O exit host", self.log.read_text())

    def test_a_runtime_directory_too_long_for_a_socket_is_passed_over(self):
        deep = self.runtime / ("d" * 90)
        deep.mkdir()
        build = self.build(self.environment | {"XDG_RUNTIME_DIR": str(deep)})
        build.share_connection()
        self.addCleanup(build.close_connection)
        self.assertIsNotNone(build.control)
        self.assertFalse(build.control.startswith(str(deep)))
        self.assertLessEqual(len(build.control), offload.CONTROL_PATH_LIMIT)
        self.assertEqual(list(deep.iterdir()), [])

    def test_without_a_short_enough_directory_each_step_connects_alone(self):
        limit = offload.CONTROL_PATH_LIMIT
        offload.CONTROL_PATH_LIMIT = 1
        self.addCleanup(setattr, offload, "CONTROL_PATH_LIMIT", limit)
        build = self.build()
        build.share_connection()
        self.assertIsNone(build.control)
        self.assertNotIn("ControlPath", " ".join(build.remote("exists", "x")))
        self.assertEqual(list(self.runtime.iterdir()), [])


class EnclosingCheckout(unittest.TestCase):
    def test_a_git_marker_in_any_ancestor_is_found(self):
        enclosing_checkout = agent_function("enclosing_checkout")
        with tempfile.TemporaryDirectory() as root:
            deep = Path(root, "a", "b", "c")
            deep.mkdir(parents=True)
            self.assertIsNone(enclosing_checkout(str(deep)))
            Path(root, "a", ".git").mkdir()
            self.assertEqual(enclosing_checkout(str(deep)), str(Path(root, "a", ".git")))
            # The directory itself is not its own ancestor.
            Path(root, "a", ".git").rmdir()
            Path(deep, ".git").write_text("gitdir: x")
            self.assertIsNone(enclosing_checkout(str(deep)))


class Clear(unittest.TestCase):
    def test_directories_left_without_permissions_are_removed(self):
        clear = agent_function("clear")
        with tempfile.TemporaryDirectory() as root:
            locked = Path(root, "t", "a", "target")
            (locked / "deep").mkdir(parents=True)
            (locked / "deep" / "file").write_text("")
            nested = locked / "deep"
            nested.chmod(0)
            locked.chmod(0)
            Path(root, "t").chmod(0)
            Path(root).chmod(0o755)
            clear(str(Path(root, "t")))
            self.assertFalse(Path(root, "t").exists())
            # Outside the tree, nothing changed.
            self.assertEqual(Path(root).stat().st_mode & 0o777, 0o755)


class TemporaryDirectory(unittest.TestCase):
    def scope(self):
        return agent_function("temporary_directory")

    def test_the_base_stays_within_the_fixtures_budget(self):
        temporary_directory = self.scope()
        with tempfile.TemporaryDirectory(dir="/var/tmp") as root:
            short = Path(root, "c")
            os.environ["CAGENTS_OFFLOAD_TEMPORARY_ROOT"] = str(short)
            self.addCleanup(os.environ.pop, "CAGENTS_OFFLOAD_TEMPORARY_ROOT")
            self.assertEqual(temporary_directory("/h/.cache/cagents-offload/ab/slot-2"),
                             str(short / "ab-2"))
            long = temporary_directory("/h/.cache/cagents-offload/a-very-long-repository/slot-2")
            self.assertLessEqual(len(long), max(28, len(str(short)) + 9))
            self.assertRegex(long, r"/[0-9a-f]{6}-2$")
            self.assertEqual(os.stat(short).st_mode & 0o777, 0o700)

    def test_another_users_directory_is_not_used(self):
        temporary_directory = self.scope()
        with tempfile.TemporaryDirectory() as root:
            shared = Path(root, "shared")
            shared.mkdir(mode=0o755)
            shared.chmod(0o755)
            os.environ["CAGENTS_OFFLOAD_TEMPORARY_ROOT"] = str(shared)
            self.addCleanup(os.environ.pop, "CAGENTS_OFFLOAD_TEMPORARY_ROOT")
            self.assertEqual(temporary_directory("/h/x/slot-0"), "/h/x/slot-0/tmp")


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

    def test_settings_naming_files_here_are_found_and_script_settings_are_not(self):
        self.assertEqual(offload.path_settings({
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER": "/t/cagentsd", "CAGENTS_LIVE_TESTS": "0",
            "CAGENTS_BUILD_LOCK": "/var/tmp/l", "TMPDIR": "/tmp/x", "CAGENTS_ZIG": "/z/zig",
            "CAGENTS_DEVELOPMENT_HOSTED_BY": "/run/user/1/cagents\n/run/user/1/nh-a/cagents-development-a",
            "CAGENTS_FIXTURE": "~/f", "CAGENTS_API_KEY_FILE": "/k",
            "CAGENTS_EVIDENCE_TASK": "/home/u/.local/state/cev/task",
            "CAGENTS_EVIDENCE_NATIVE_LOCK": "/var/tmp/native.lock"}),
            ["CAGENTS_FIXTURE", "CAGENTS_TEST_RETAINED_ACTOR_HELPER"])


class Sweep(unittest.TestCase):
    def test_only_unused_directories_of_ended_builds_are_removed(self):
        ended = subprocess.Popen(["true"])
        ended.wait()
        with tempfile.TemporaryDirectory(prefix="cof-", dir=SOCKET_ROOT) as root:
            names = {
                # An older gate's names, judged by age.
                "old": "cof-ssh-old", "fresh": "cof-ssh-fresh",
                # A build that ended a while ago, and one still waiting for
                # the host, however long, which never connected.
                "ended": f"cof-ssh-{ended.pid}-abc", "waiting": f"cof-ssh-{os.getpid()}-abc",
                # One that looks ended but used its directory just now, as a
                # build in another PID namespace does while it waits.
                "renewed": f"cof-ssh-{ended.pid}-ghi",
                # A connection that outlives its ended build for a while.
                "listening": f"cof-ssh-{ended.pid}-def",
            }
            paths = {key: Path(root, name) for key, name in names.items()}
            for path in paths.values():
                path.mkdir()
            Path(root, "unrelated").mkdir()
            listener = socket.socket(socket.AF_UNIX)
            self.addCleanup(listener.close)
            listener.bind(str(paths["listening"] / "m"))
            listener.listen()
            ancient = time.time() - 3600
            for key in ("old", "ended", "waiting", "listening"):
                os.utime(paths[key], (ancient, ancient))
            offload.sweep_connections(root)
            self.assertEqual(sorted(path.name for path in Path(root).iterdir()),
                             sorted([names["fresh"], names["waiting"], names["renewed"],
                                     names["listening"], "unrelated"]))


class ThisHost(unittest.TestCase):
    def test_where_builds_go_is_a_status_failure_cannot_give(self):
        script = str(ROOT / "scripts/cargo-offload")
        here = subprocess.run([script, "--host"], capture_output=True, text=True,
                              env=os.environ | {"CAGENTS_BUILD_REMOTE": "local"})
        self.assertEqual(here.returncode, offload.FALL_BACK)
        self.assertNotIn(here.returncode, (0, 1))

    @unittest.skipIf(offload.is_this_host("fw16"), "this is the default build host")
    def test_an_empty_build_host_setting_is_the_default_host(self):
        script = str(ROOT / "scripts/cargo-offload")
        default = subprocess.run([script, "--host"], capture_output=True, text=True,
                                 env=os.environ | {"CAGENTS_BUILD_REMOTE": ""})
        self.assertEqual((default.returncode, default.stdout), (0, "fw16\n"))


    def test_the_build_host_builds_in_its_own_slots_only_when_asked(self):
        script = str(ROOT / "scripts/cargo-offload")
        this = socket.gethostname()
        def where(**settings):
            result = subprocess.run([script, "--host"], capture_output=True, text=True,
                                    env=os.environ | {"CAGENTS_BUILD_REMOTE": this} | settings)
            return result.returncode, result.stdout
        self.assertEqual(where(), (offload.FALL_BACK, ""))
        self.assertEqual(where(CAGENTS_BUILD_REMOTE_LOOPBACK="0"), (offload.FALL_BACK, ""))
        self.assertEqual(where(CAGENTS_BUILD_REMOTE_LOOPBACK="1"), (0, f"{this}\tloopback\n"))
        # `local` still means this checkout's own build directory.
        self.assertEqual(where(CAGENTS_BUILD_REMOTE="local", CAGENTS_BUILD_REMOTE_LOOPBACK="1"),
                         (offload.FALL_BACK, ""))
        # Another host is reached as ever.
        self.assertEqual(where(CAGENTS_BUILD_REMOTE="192.0.2.1", CAGENTS_BUILD_REMOTE_LOOPBACK="1"),
                         (0, "192.0.2.1\n"))

    def test_a_loopback_runs_commands_and_copies_as_sshd_would(self):
        build = offload.Offload("fw16", None, {}, loopback=True)
        self.assertEqual(build.remote("exists", "/x")[:3],
                         [sys.executable, str(ROOT / "scripts/cargo-offload"), "--loopback-shell"])
        # There is no connection to share, or to end.
        build.share_connection()
        self.assertIsNone(build.control)
        script = str(ROOT / "scripts/cargo-offload")
        account = pwd.getpwuid(os.getuid())
        # A plain shell, whose configuration cannot add anything.
        caller = os.environ | {"CARGO_TARGET_DIR": "/caller/target", "CAGENTS_BUILD_LOCK": "/x",
                               "CAGENTS_BUILD_REMOTE_LOOPBACK_SHELL": "/bin/sh"}
        result = subprocess.run(
            [script, "--loopback-shell", "-o", "BatchMode=yes", "-o", "ControlPath=/x/m", "fw16",
             "pwd;", "env"],
            capture_output=True, text=True, env=caller, cwd="/", timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], account.pw_dir)
        self.assertIn(f"HOME={account.pw_dir}", lines)
        # Nothing of the caller's build reaches the slot.
        self.assertFalse([line for line in lines if line.startswith(("CARGO_TARGET_DIR=", "CAGENTS_BUILD_"))])
        # A control command acts on no connection, and runs nothing.
        ended = subprocess.run([script, "--loopback-shell", "-O", "exit", "fw16"],
                               capture_output=True, text=True, timeout=60)
        self.assertEqual((ended.returncode, ended.stdout), (0, ""))

    def test_a_loopback_that_is_killed_ends_its_command_as_a_dropped_connection_would(self):
        script = str(ROOT / "scripts/cargo-offload")
        with tempfile.TemporaryDirectory(prefix="cagents-loopback-") as root:
            ended = Path(root) / "ended"
            # Like the build host's agent: it ends what it started once its
            # input closes. The shell waits for it rather than exec it, as fish does.
            command = (f"python3 -c 'import sys; sys.stdin.read(); open(sys.argv[1], \"w\")' "
                       f"{ended}; true")
            loopback = subprocess.Popen(
                [script, "--loopback-shell", "fw16", command], stdin=subprocess.PIPE,
                env=os.environ | {"CAGENTS_BUILD_REMOTE_LOOPBACK_SHELL": "/bin/sh"})
            self.addCleanup(loopback.stdin.close)
            time.sleep(1)
            self.assertFalse(ended.exists())
            # This process still holds the loopback's input open, as a caller
            # whose stand-in for ssh was killed does.
            loopback.kill()
            loopback.wait(timeout=10)
            for _ in range(100):
                if ended.exists():
                    break
                time.sleep(0.1)
            self.assertTrue(ended.exists())

    def test_this_hosts_names_and_addresses_build_here(self):
        self.assertTrue(offload.is_this_host(socket.gethostname()))
        self.assertTrue(offload.is_this_host("localhost"))
        self.assertTrue(offload.is_this_host("127.0.0.1"))
        self.assertFalse(offload.is_this_host("192.0.2.1"))


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
for argument do
    if [ "$argument" = -G ]; then
        # Where the host is: an address that is not this machine's.
        printf 'hostname 192.0.2.1\\n'
        exit 0
    fi
done
while [ "$#" -gt 0 ]; do
    case $1 in
        -O) exit 0 ;;
        -o) shift 2 ;;
        -*) shift ;;
        *) break ;;
    esac
done
# What the build host was asked to do, one line each, for tests that count.
[ -z "${STAND_IN_LOG-}" ] || printf '%s\n' "$*" >>"$STAND_IN_LOG"
[ "$1" != unreachable ] || exit 255
[ "$1" != silent ] || exec sleep 600
[ "$1" != broken ] || exit 1
if [ "$1" = marked ]; then
    # A host with a stray Git marker above its temporary directory.
    shift
    cd "$HOME"
    exec env -i CAGENTS_BUILD_REGISTRY= HOME="$HOME" PATH="$STAND_IN_HOST_PATH" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
        CAGENTS_OFFLOAD_FIGURES="$HOME/figures" CAGENTS_OFFLOAD_TEMPORARY_ROOT="$HOME/stray/cof" \
        sh -c "mkdir -p '$HOME/stray/.git'; $*"
fi
shift
cd "$HOME"
# Like sshd, start from a clean environment, the host's own search path and a
# session's own open-file limit: nothing of this one reaches the build host
# except what the command carries.
ulimit -Sn 256
# Its Cargo keeps intermediates outside the slot, as a build-dir setting in
# its configuration would, and inside the stand-in's home, which the test
# removes. A test may have it name every session that holds its CPUs.
exec env -i CAGENTS_BUILD_REGISTRY= HOME="$HOME" PATH="$STAND_IN_HOST_PATH" CARGO_HOME="$CARGO_HOME" \
    RUSTUP_HOME="$RUSTUP_HOME" CARGO_BUILD_BUILD_DIR="$HOME/build{workspace-root}/target" \
    CAGENTS_OFFLOAD_FIGURES="$HOME/figures" CAGENTS_OFFLOAD_TEMPORARY_ROOT="$HOME/cof" \
    CAGENTS_OFFLOAD_BUSIEST_SHOWN="${CAGENTS_OFFLOAD_BUSIEST_SHOWN-}" \
    CAGENTS_OFFLOAD_BUSIEST_LEAST="${CAGENTS_OFFLOAD_BUSIEST_LEAST-}" sh -c "$*"
"""


@unittest.skipUnless(shutil.which("cargo") and shutil.which("rsync"), "needs cargo and rsync")
class StandInHost(unittest.TestCase):
    """A tiny crate built through the gate on a stand-in build host."""

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-offload-")
        self.addCleanup(self.scope.close)
        self.root = Path(self.scope.name)
        tools = self.root / "tools"
        tools.mkdir()
        # HOME changes below, so caller Python shims (such as mise's) may
        # no longer resolve their interpreter. Keep the test's own Python
        # first without changing which Cargo or rustup each side finds.
        python_tools = self.root / "python"
        python_tools.mkdir()
        (python_tools / "python3").symlink_to(Path(sys.executable).resolve())
        for name in ("cargo-gate", "cargo-gate-command.py", "cargo-gate-rustc-guard",
                     "cargo-offload", "memory-capped"):
            shutil.copy2(ROOT / "scripts" / name, tools / name)
        (tools / "ssh").write_text(STAND_IN_SSH)
        (tools / "ssh").chmod(0o755)
        self.gate = tools / "cargo-gate"
        self.host_home = self.root / "host"
        self.host_home.mkdir()
        write_figures(self.host_home / "figures")
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
        # A host's default toolchain may predate CARGO_BUILD_BUILD_DIR.
        shutil.copy2(ROOT / "rust-toolchain.toml", self.crate)
        self.scope.run(["git", "init", "-q"], cwd=self.crate, check=True)
        home = Path.home()
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("CAGENTS_BUILD_", "CARGO_BUILD_"))
        } | {
            "PATH": os.pathsep.join([str(python_tools), str(tools), os.environ["PATH"]]),
            # A build host's `cargo` is rustup's, as fw16's is, rather than
            # whatever wraps it first on this host's PATH: mise's shim, for
            # one, raises the open-file limit the build is meant to keep.
            "STAND_IN_HOST_PATH": os.pathsep.join([
                str(python_tools),
                os.path.join(os.environ.get("CARGO_HOME", str(home / ".cargo")), "bin"),
                os.environ["PATH"]]),
            "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"),
            "CAGENTS_BUILD_REMOTE": "stand-in",
            "CAGENTS_BUILD_REMOTE_SLOTS": "2",
            "CAGENTS_BUILD_SEED": "0",
            "CAGENTS_BUILD_FREE_FLOOR_GIB": "0",
            "CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB": "1",
            "CAGENTS_BUILD_REMOTE_RETRY_SECONDS": "1",
            "CAGENTS_BUILD_INSTALLED_GATE": "",
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(home / ".cargo")),
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(home / ".rustup")),
            "CARGO_BUILD_BUILD_DIR": str(self.root / "local-build"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"),
        }

    def gate_run(self, *arguments, env, timeout=120):
        return self.scope.run([str(self.gate), *arguments], cwd=self.crate, env=env,
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
        self.assertEqual(self.scope.run([executable], capture_output=True, text=True).stdout, "probe\n")
        # Nothing was built here.
        self.assertFalse((self.root / "local-build").exists())

    def assert_test_memory_cap(self, env):
        # Prove a real scope is available, without compiling or allocating.
        cap = ROOT / "scripts/memory-capped"
        report = 'cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'
        probe = self.scope.run([str(cap), "sh", "-c", report], capture_output=True, text=True,
                               env=os.environ | {"CAGENTS_MEMORY_MAX": "128M"}, timeout=60)
        if probe.returncode or probe.stdout.strip() != str(128 * 2**20):
            self.skipTest("needs an unconstrained user manager with working memory scopes")
        (self.crate / "src/lib.rs").write_text(r'''#[test]
fn bounded_allocation() {
    let group = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let group = group.lines().find_map(|line| line.strip_prefix("0::")).unwrap();
    let limit = std::fs::read_to_string(format!("/sys/fs/cgroup{group}/memory.max")).unwrap();
    assert_eq!(limit.trim(), "134217728", "offloaded tests lost their cap");
    // Even a broken cap cannot allocate without bound: stop at 512 MiB.
    let chunks: Vec<_> = (0..64).map(|_| vec![1u8; 8 * 1024 * 1024]).collect();
    std::hint::black_box(chunks);
}
''')
        env = env | {"CAGENTS_MEMORY_MAX": "128M", "CAGENTS_MEMORY_SWAP_MAX": "0"}
        built = self.gate_run("test", "--lib", "--no-run", env=env)
        self.assertEqual(built.returncode, 0, built.stderr)
        result = self.gate_run("test", "--lib", env=env)
        self.assertEqual(result.returncode, 137, result.stdout + result.stderr)
        self.assertIn("went over the limit", result.stderr)
        self.assertNotIn("offloaded tests lost their cap", result.stdout)

    def test_offloaded_tests_enforce_the_memory_cap(self):
        self.assert_test_memory_cap(self.remote_env())

    def test_loopback_tests_enforce_the_memory_cap(self):
        self.assert_test_memory_cap(self.loopback_env())

    def loopback_env(self):
        # The host's login shell, standing in for this one's: its home holds
        # the slots, and its own search path finds Cargo.
        shell = self.root / "tools/host-shell"
        settings = {
            "HOME": self.host_home, "PATH": self.env["STAND_IN_HOST_PATH"],
            "CARGO_HOME": self.env["CARGO_HOME"], "RUSTUP_HOME": self.env["RUSTUP_HOME"],
            "CARGO_BUILD_BUILD_DIR": f"{self.host_home}/build{{workspace-root}}/target",
            "CAGENTS_OFFLOAD_FIGURES": self.host_home / "figures",
            "CAGENTS_OFFLOAD_TEMPORARY_ROOT": self.host_home / "cof", "CAGENTS_BUILD_REGISTRY": "",
        }
        shell.write_text("#!/bin/sh\n"
                         f"cd {shlex.quote(str(self.host_home))}\n"
                         "exec env -i " + " ".join(f"{name}={shlex.quote(str(value))}"
                                                   for name, value in settings.items())
                         + ' sh "$@"\n')
        shell.chmod(0o755)
        # Nothing may reach the host through ssh.
        (self.root / "tools/ssh").unlink()
        env = self.env | {"CAGENTS_BUILD_REMOTE": socket.gethostname(),
                          "CAGENTS_BUILD_REMOTE_LOOPBACK": "1",
                          "CAGENTS_BUILD_REMOTE_LOOPBACK_SHELL": str(shell),
                          "CARGO_TARGET_DIR": str(self.root / "evidence/target-after")}
        return env

    def test_the_build_host_builds_in_its_own_slots_through_a_loopback(self):
        env = self.loopback_env()
        slot = self.host_home / ".cache/cagents-offload/crate/slot-0/src"
        result = self.gate_run("build", "--bin", "probe", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((slot / "target/debug/probe").is_file())
        # As native-evidence builds: one slot held for a script of builds.
        result = self.gate_run("exec", "--hold", "sh", "-c", "cargo build --release --bin probe",
                               env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((slot / "target/release/probe").is_file())
        for profile in ("debug", "release"):
            executable = self.root / "evidence/target-after" / profile / "probe"
            self.assertEqual(self.scope.run([executable], capture_output=True, text=True).stdout,
                             "probe\n")
        # Only the executables came back; the build output stays in the slot.
        self.assertEqual(sorted(path.name for path in (self.root / "evidence/target-after").rglob("*")
                                if path.is_file()), ["probe", "probe"])
        self.assertFalse((self.root / "local-build").exists())

    def test_a_worktree_builds_its_own_sources_in_a_slot_another_built_newer_ones_in(self):
        # Two worktrees of one repository share the slot. The second's copy
        # of a patched path dependency is older than the first's build there,
        # as a checkout made before the other's edit is: Cargo, judging path
        # dependencies by modification time, must still see it changed.
        def worktree(path, value, mtime=None):
            (path / "vendor/dep/src").mkdir(parents=True, exist_ok=True)
            (path / "src").mkdir(exist_ok=True)
            (path / "Cargo.toml").write_text(textwrap.dedent("""
                [package]
                name = "probe"
                version = "0.1.0"
                edition = "2021"
                [dependencies]
                dep = { path = "vendor/dep" }
                [workspace]
            """))
            (path / "vendor/dep/Cargo.toml").write_text(
                '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n')
            library = path / "vendor/dep/src/lib.rs"
            library.write_text(f"pub fn value() -> u32 {{ {value} }}\n")
            if mtime is not None:
                os.utime(library, (mtime, mtime))
            (path / "src/main.rs").write_text('fn main() { println!("{}", dep::value()); }\n')
            (path / ".gitignore").write_text("/target\n")
            if not (path / ".git").exists():
                self.scope.run(["git", "init", "-q"], cwd=path, check=True)

        worktree(self.crate, 11)
        other = self.root / "elsewhere" / "crate"
        other.mkdir(parents=True)
        worktree(other, 22, mtime=time.time() - 3600)
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE_SLOTS": "1"}
        for path, expected in ((self.crate, "11\n"), (other, "22\n"), (self.crate, "11\n")):
            result = self.scope.run([str(self.gate), "run", "--quiet"], cwd=path, env=env,
                                    capture_output=True, text=True, timeout=120)
            self.assertEqual((result.returncode, result.stdout), (0, expected), result.stderr)
        # An unchanged worktree's next build there compiles nothing.
        result = self.scope.run([str(self.gate), "build"], cwd=self.crate, env=env,
                                capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("Compiling", result.stderr)

    def test_a_slot_synced_by_modification_time_rebuilds_its_sources_once(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE_SLOTS": "1"}
        main = self.crate / "src/main.rs"
        original = main.read_text()
        main.write_text('fn main() { println!("other"); }\n')
        self.assertEqual(self.gate_run("build", env=env).returncode, 0)
        # A slot copied the old way: its copy now matches this checkout, with
        # this checkout's older time, but its build is of the other sources.
        # A gate from before copied this checkout's file with its own older
        # time into the slot and built nothing new; its lease wrote the
        # owner.
        slot = self.host_home / ".cache/cagents-offload/crate/slot-0"
        self.assertTrue((slot / "synced-by-content").is_file())
        (slot / "owner").write_text("older-gate\n")
        old = time.time() - 3600
        for copy in (main, slot / "src/src/main.rs"):
            copy.write_text(original)
            os.utime(copy, (old, old))
        result = self.gate_run("run", "--quiet", env=env)
        self.assertEqual((result.returncode, result.stdout), (0, "probe\n"), result.stderr)
        result = self.gate_run("build", env=env)
        self.assertNotIn("Compiling", result.stderr)

    def test_the_program_inherits_none_of_the_builds_processes(self):
        (self.crate / "src/main.rs").write_text(
            'fn main() { let id = std::process::id(); print!("[{}]", std::fs::read_to_string('
            'format!("/proc/{id}/task/{id}/children")).unwrap().trim()); }\n')
        result = self.gate_run("run", "--quiet", env=self.remote_env())
        self.assertEqual((result.returncode, result.stdout), (0, "[]"), result.stderr)

    def test_a_programs_own_status_is_never_taken_for_building_here(self):
        # 75 is EX_TEMPFAIL, and also how cargo-offload says this host builds.
        (self.crate / "src/main.rs").write_text("fn main() { std::process::exit(75); }\n")
        result = self.gate_run("run", "--quiet", env=self.remote_env())
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertFalse((self.root / "local-build").exists())
        self.assertEqual(list(Path(self.env["XDG_RUNTIME_DIR"]).glob("cof-ssh-*")), [])

    def test_the_build_goes_where_the_gate_found_it_should(self):
        # cargo-offload would decide afresh that this host builds, as a slow
        # name lookup can, and exit 75 having built nothing.
        tools = self.root / "tools"
        (tools / "cargo-offload").rename(tools / "cargo-offload.real")
        (tools / "cargo-offload").write_text(textwrap.dedent(f"""\
            #!/bin/sh
            [ "$1" != --host ] || {{ echo stand-in; exit 0; }}
            CAGENTS_BUILD_REMOTE=local exec '{tools / "cargo-offload.real"}' "$@"
        """))
        (tools / "cargo-offload").chmod(0o755)
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?}", std::env::var("CAGENTS_BUILD_OFFLOAD_HOST").ok()); }\n')
        result = self.gate_run("run", "--quiet", env=self.remote_env())
        self.assertEqual((result.returncode, result.stdout), (0, "None\n"), result.stderr)
        self.assertTrue((self.host_home / ".cache/cagents-offload/crate/slot-0/src/target").is_dir())
        self.assertFalse((self.root / "local-build").exists())

    def commit(self, message):
        self.scope.run(["git", "add", "-A"], cwd=self.crate, check=True)
        self.scope.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                        "commit", "-qm", message], cwd=self.crate, check=True)
        return self.scope.run(["git", "rev-parse", "--short", "HEAD"], cwd=self.crate,
                              capture_output=True, text=True, check=True).stdout.strip()

    def stamp_of_a_build(self, env):
        result = self.gate_run("run", "--quiet", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def test_each_commit_restamps_the_executable_here_and_there(self):
        # The stamp reaches Cargo with its marker, which tells build.rs that
        # the checkout need not be read, wherever the build runs.
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?} {:?}", option_env!("CAGENTS_BUILD_METADATA"), '
            'option_env!("CAGENTS_BUILD_METADATA_SUPPLIED")); }\n')
        first = self.commit("first")
        for name, env in (("there", self.remote_env()),
                          ("here", self.env | {"CAGENTS_BUILD_REMOTE": "local"})):
            with self.subTest(name):
                self.assertEqual(self.stamp_of_a_build(env), f'Some("local.{first}") Some("1")')
        (self.crate / "README").write_text("second\n")
        second = self.commit("second")
        (self.crate / "README").write_text("edited\n")
        for name, env in (("there", self.remote_env()),
                          ("here", self.env | {"CAGENTS_BUILD_REMOTE": "local"})):
            with self.subTest(name):
                self.assertEqual(self.stamp_of_a_build(env),
                                 f'Some("local.{second}.dirty") Some("1")')

    def test_a_stamp_a_gate_left_behind_is_worked_out_again(self):
        # A program a gated `run` started inherits that build's stamp, and a
        # build it starts must not keep it; a stamp the caller chose is kept.
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?}", option_env!("CAGENTS_BUILD_METADATA")); }\n')
        sha = self.commit("first")
        inherited = {"CAGENTS_BUILD_METADATA": "local.stale", "CAGENTS_BUILD_METADATA_SUPPLIED": "1"}
        chosen = {"CAGENTS_BUILD_METADATA": "local.chosen"}
        for name, env in (("there", self.remote_env()),
                          ("here", self.env | {"CAGENTS_BUILD_REMOTE": "local"})):
            with self.subTest(name):
                self.assertEqual(self.stamp_of_a_build(env | inherited), f'Some("local.{sha}")')
                self.assertEqual(self.stamp_of_a_build(env | chosen), 'Some("local.chosen")')

    def test_the_program_a_gate_runs_keeps_the_callers_stamp_not_the_gates(self):
        # Else a bare Cargo the program starts would build with this stamp
        # after the checkout has moved on.
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?} {:?}", std::env::var("CAGENTS_BUILD_METADATA").ok(), '
            'std::env::var("CAGENTS_BUILD_METADATA_SUPPLIED").ok()); }\n')
        self.commit("first")
        here = self.env | {"CAGENTS_BUILD_REMOTE": "local"}
        for name, env, expected in (
                ("there", self.remote_env(), "None None"),
                ("here", here, "None None"),
                ("here, chosen", here | {"CAGENTS_BUILD_METADATA": "local.chosen"},
                 'Some("local.chosen") None'),
                ("here, in a slot", here, "None None")):
            with self.subTest(name):
                arguments = (["exec", "cargo", "run", "--quiet"] if name.endswith("slot")
                             else ["run", "--quiet"])
                result = self.gate_run(*arguments, env=env)
                self.assertEqual((result.returncode, result.stdout.strip()), (0, expected),
                                 result.stderr)

    def test_a_runner_of_the_callers_own_is_left_alone(self):
        # Two `cfg(..)` runners would fail the command.
        (self.crate / "src/main.rs").write_text('fn main() { println!("ran"); }\n')
        (self.crate / ".cargo").mkdir()
        (self.crate / ".cargo/config.toml").write_text(
            "[target.'cfg(all())']\nrunner = [\"env\", \"OWN_RUNNER=1\"]\n")
        self.commit("first")
        result = self.gate_run("run", "--quiet", env=self.env | {"CAGENTS_BUILD_REMOTE": "local"})
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "ran"), result.stderr)

    def test_cargo_in_an_exec_command_here_is_stamped_too(self):
        # Otherwise a build directory used by both would flip build.rs
        # between reading the checkout and not, recompiling the library.
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?}", option_env!("CAGENTS_BUILD_METADATA")); }\n')
        sha = self.commit("first")
        env = self.env | {"CAGENTS_BUILD_REMOTE": "local"}
        for command in (["sh", "-c", "cargo run --quiet"],
                        ["sh", "-c", f"'{self.gate}' exec cargo run --quiet"]):
            with self.subTest(command=command):
                result = self.gate_run("exec", *command, env=env)
                self.assertEqual((result.returncode, result.stdout.strip()),
                                 (0, f'Some("local.{sha}")'), result.stderr)

    def test_a_stamp_here_is_the_built_workspaces_not_the_current_directorys(self):
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?}", option_env!("CAGENTS_BUILD_METADATA")); }\n')
        sha = self.commit("first")
        other = self.root / "other"
        other.mkdir()
        self.scope.run(["git", "init", "-q"], cwd=other, check=True)
        self.scope.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                        "commit", "-q", "--allow-empty", "-m", "other"], cwd=other, check=True)
        result = self.scope.run(
            [str(self.gate), "run", "--quiet", "--manifest-path", str(self.crate / "Cargo.toml")],
            cwd=other, env=self.env | {"CAGENTS_BUILD_REMOTE": "local"},
            capture_output=True, text=True, timeout=120)
        self.assertEqual((result.returncode, result.stdout.strip()),
                         (0, f'Some("local.{sha}")'), result.stderr)

    def test_an_inherited_stamp_is_dropped_where_none_can_be_worked_out(self):
        (self.crate / "src/main.rs").write_text(
            'fn main() { println!("{:?} {:?}", option_env!("CAGENTS_BUILD_METADATA"), '
            'option_env!("CAGENTS_BUILD_METADATA_SUPPLIED")); }\n')
        shutil.rmtree(self.crate / ".git")
        inherited = {"CAGENTS_BUILD_METADATA": "local.stale", "CAGENTS_BUILD_METADATA_SUPPLIED": "1"}
        result = self.gate_run("run", "--quiet",
                               env=self.env | {"CAGENTS_BUILD_REMOTE": "local"} | inherited)
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "None None"),
                         result.stderr)

    def test_a_gate_without_its_offload_helper_does_not_build_here(self):
        (self.root / "tools/cargo-offload").unlink()
        result = self.gate_run("build", env=self.remote_env())
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("cargo-offload is missing", result.stderr)
        self.assertIn("CAGENTS_BUILD_REMOTE=local", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_run_builds_there_and_runs_the_program_here(self):
        (self.crate / "src/main.rs").write_text(textwrap.dedent("""
            fn main() {
                let arguments: Vec<String> = std::env::args().skip(1).collect();
                println!("{} {} {}", arguments.join(","), env!("CARGO_PKG_NAME"),
                         std::env::var("CARGO_MANIFEST_DIR").unwrap());
                println!("{}", std::fs::read_to_string("/proc/self/exe").is_ok());
                std::process::exit(3);
            }
        """))
        result = self.gate_run("run", "--bin", "probe", "--", "a", "b c", env=self.remote_env())
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], f"a,b c probe {self.crate.resolve()}")
        self.assertTrue((self.crate / "target/debug/probe").is_file())
        self.assertFalse((self.root / "local-build").exists())

    def test_test_executables_and_their_messages_come_back_with_this_hosts_paths(self):
        (self.crate / "src/main.rs").write_text(
            'fn main() {}\n#[test]\nfn passes() { assert_eq!(1 + 1, 2); }\n')
        result = self.gate_run("test", "--no-run", "--message-format=json", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        messages = [json.loads(line) for line in result.stdout.splitlines()]
        executables = [message["executable"] for message in messages
                       if message.get("reason") == "compiler-artifact" and message.get("executable")]
        self.assertEqual(len(executables), 1, executables)
        # The harness is where Cargo here would have put it: in this host's
        # build directory, wherever the build host's Cargo keeps its own.
        self.assertTrue(executables[0].startswith(str(self.root / "local-build") + "/"),
                        executables[0])
        self.assertNotIn(str(self.host_home), result.stdout)
        self.assertTrue((self.host_home / "build").is_dir())
        run = self.scope.run([executables[0]], capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertIn("test passes ... ok", run.stdout)

    def test_timing_reports_come_back(self):
        result = self.gate_run("build", "--timings", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.crate / "target/cargo-timings/cargo-timing.html").is_file())

    def test_path_options_in_the_checkout_are_translated(self):
        result = self.gate_run("build", "--manifest-path", "./Cargo.toml", "--target-dir",
                               "target/alternate", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.crate / "target/alternate/debug/probe").is_file())
        result = self.gate_run("check", f"--target-dir={self.root}/elsewhere", env=self.remote_env())
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn("is outside this checkout", result.stderr)
        self.assertFalse((self.root / "elsewhere").exists())

    def test_a_file_a_setting_names_in_the_target_reaches_the_tests(self):
        (self.crate / "src/main.rs").write_text(textwrap.dedent("""
            fn main() {}
            #[test]
            fn helper() {
                let helper = std::env::var("CAGENTS_PROBE_HELPER").unwrap();
                let output = std::process::Command::new(&helper).output().unwrap();
                assert_eq!(String::from_utf8_lossy(&output.stdout), "helper\\n", "{helper}");
            }
        """))
        helper = self.crate / "target/debug/helper"
        helper.parent.mkdir(parents=True)
        helper.write_text("#!/bin/sh\necho helper\n")
        helper.chmod(0o755)
        env = self.remote_env() | {"CAGENTS_PROBE_HELPER": str(helper)}
        result = self.gate_run("test", env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("test helper ... ok", result.stdout)
        result = self.gate_run("test", env=env | {"CAGENTS_PROBE_HELPER": "/bin/true"})
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn("CAGENTS_PROBE_HELPER names /bin/true, outside this checkout", result.stderr)

    def test_fixes_made_there_are_applied_here(self):
        main = self.crate / "src/main.rs"
        main.write_text('fn main() {\n    let mut value = 1;\n    println!("{value}");\n}\n')
        git = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
        self.scope.run(git + ["add", "-A"], cwd=self.crate, check=True)
        self.scope.run(git + ["commit", "-qm", "probe"], cwd=self.crate, check=True)
        main.write_text(main.read_text() + "// uncommitted\n")
        result = self.gate_run("fix", "--bin", "probe", env=self.remote_env())
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn("uncommitted changes", result.stderr)
        self.scope.run(["git", "checkout", "--", "src/main.rs"], cwd=self.crate, check=True)
        result = self.gate_run("fix", "--bin", "probe", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("    let value = 1;", main.read_text())
        self.assertIn("applied fixes to 1 file", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_commands_that_compile_nothing_run_here_and_others_are_refused(self):
        result = self.gate_run("metadata", "--format-version", "1", "--no-deps", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["packages"][0]["name"], "probe")
        self.assertFalse((self.host_home / ".cache").exists())
        result = self.gate_run("install", "--path", ".", env=self.remote_env())
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn("`cargo install` may compile", result.stderr)

    def test_a_build_on_a_host_with_shims_names_their_data_directory(self):
        # A test there may move HOME, and a shim would then re-execute forever.
        shims = self.host_home / ".local/share/mise/shims"
        shims.mkdir(parents=True)
        main = self.crate / "src/main.rs"
        main.write_text('fn main() { println!("{}", env!("MISE_DATA_DIR")); }\n')
        result = self.gate_run("build", "--bin", "probe", env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        printed = self.scope.run([self.crate / "target/debug/probe"], capture_output=True, text=True).stdout
        self.assertEqual(printed.strip(), str(shims.parent))

    def test_cargo_in_an_exec_command_builds_there(self):
        result = self.gate_run(
            "exec", "sh", "-ec",
            "cargo build --bin probe; cargo metadata --format-version 1 >/dev/null; "
            "cargo --help | grep -q 'Rust.s package manager'; command -v cargo",
            env=self.remote_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.strip().endswith("/cargo"))
        self.assertNotIn(os.path.dirname(shutil.which("cargo")), result.stdout)
        self.assertTrue((self.crate / "target/debug/probe").is_file())
        self.assertTrue((self.host_home / ".cache/cagents-offload/crate/slot-0/src/target/debug/probe")
                        .is_file())
        self.assertFalse((self.root / "local-build").exists())
        # Its shim is gone afterwards.
        self.assertFalse(Path(result.stdout.strip()).exists())

    def lease_costs(self, log):
        """The expected cost of each slot the build host was asked to lease."""
        if not log.exists():
            return []
        requests = [json.loads(base64.b64decode(shlex.split(line)[-1]))[1]
                    for line in log.read_text().splitlines() if "cargo-offload" in line]
        return [request[6] for request in requests if request[:1] == ["lease"]]

    def leases(self, log):
        """How many slots the build host was asked to lease."""
        return len(self.lease_costs(log))

    def test_a_held_exec_builds_all_its_cargo_commands_in_one_lease(self):
        log = self.root / "requests"
        env = self.remote_env() | {"STAND_IN_LOG": str(log)}
        result = self.gate_run(
            "exec", "--hold", "sh", "-ec",
            "cargo build --bin probe; cargo build --release --bin probe; "
            "cargo metadata --format-version 1 >/dev/null; exit 7",
            env=env)
        self.assertEqual(result.returncode, 7, result.stderr)
        # Priced as the largest build it may run, a UI build.
        self.assertEqual(self.lease_costs(log), [str(9 * 2**30)], log.read_text())
        self.assertNotIn("waiting", result.stderr)
        self.assertTrue((self.crate / "target/debug/probe").is_file())
        self.assertTrue((self.crate / "target/release/probe").is_file())
        slots = self.host_home / ".cache/cagents-offload/crate"
        self.assertTrue((slots / "slot-0/src/target/release/probe").is_file())
        self.assertFalse((slots / "slot-1").exists())
        self.assertFalse((self.root / "local-build").exists())
        # The slot is free again, and nothing of the hold is left here.
        self.assertFalse(list((self.root / "runtime").glob("cagents-hold-*")))
        log.unlink()
        result = self.gate_run("build", "--bin", "probe", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.leases(log), 1)
        self.assertFalse((slots / "slot-1").exists())

    def test_a_build_leases_its_own_slot_once_a_hold_has_ended_or_is_not_its_checkouts(self):
        log = self.root / "requests"
        lock = self.root / "runtime" / "cagents-hold-ended" / "lock"
        lock.parent.mkdir(parents=True)
        lock.touch()
        base = "~/.cache/cagents-offload/crate"
        for top in (str(self.crate.resolve()), str(self.root / "elsewhere")):
            with self.subTest(top=top):
                holder = open(lock)
                if top != str(self.crate.resolve()):
                    # Held, but for another checkout.
                    fcntl.flock(holder, fcntl.LOCK_EX)
                held = json.dumps({"host": "stand-in", "base": base, "top": top,
                                   "slot": f"{base}/slot-1", "marker": None, "lock": str(lock)})
                env = self.remote_env() | {"STAND_IN_LOG": str(log),
                                           "CAGENTS_BUILD_OFFLOAD_HELD": held}
                result = self.gate_run("build", "--bin", "probe", env=env)
                holder.close()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.leases(log), 1)
                self.assertFalse((self.host_home / ".cache/cagents-offload/crate/slot-1").exists())
                log.unlink()

    def test_a_build_leases_its_own_slot_when_a_hold_names_no_slot(self):
        log = self.root / "requests"
        lock = self.root / "runtime" / "cagents-hold-held" / "lock"
        lock.parent.mkdir(parents=True)
        holder = open(lock, "w")
        self.addCleanup(holder.close)
        fcntl.flock(holder, fcntl.LOCK_EX)
        base = "~/.cache/cagents-offload/crate"
        for slot in ("/home/someone/slot-1/../other", "/home/someone/../slot-1",
                     "/home/someone/slot-1/src", f"{base}/slot-1", "slot-1"):
            with self.subTest(slot=slot):
                held = json.dumps({"host": "stand-in", "base": base,
                                   "top": str(self.crate.resolve()), "slot": slot,
                                   "marker": None, "lock": str(lock)})
                env = self.remote_env() | {"STAND_IN_LOG": str(log),
                                           "CAGENTS_BUILD_OFFLOAD_HELD": held}
                result = self.gate_run("build", "--bin", "probe", env=env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.leases(log), 1)
                log.unlink()

    def test_tests_in_a_hold_run_in_its_slot(self):
        # On a host of one slot, tests leasing a slot of their own would wait
        # for the hold around them.
        log = self.root / "requests"
        env = self.remote_env() | {"STAND_IN_LOG": str(log), "CAGENTS_BUILD_REMOTE_SLOTS": "1"}
        result = self.gate_run(
            "exec", "--hold", "sh", "-ec",
            "cargo build --bin probe; cargo test; cargo build --bin probe",
            env=env, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.leases(log), 1, log.read_text())

    def test_tests_in_a_hold_stop_under_a_stray_git_marker_and_builds_do_not(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "marked"}
        result = self.gate_run("exec", "--hold", "sh", "-c",
                               "cargo build --bin probe || exit 1; cargo test; echo \"tests $?\"",
                               env=env, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests 78", result.stdout)
        self.assertIn(f"cargo-gate: {self.host_home}/stray/.git on marked puts the tests inside a Git checkout;",
                      result.stderr)
        self.assertTrue((self.crate / "target/debug/probe").is_file())

    def test_cargo_in_an_exec_command_reaches_cargo_through_a_wrapper(self):
        # Like mise's shim, the wrapper runs the next `cargo` on PATH after its
        # own directory, which inside the command is the gate's shim.
        wrappers = self.root / "wrappers"
        wrappers.mkdir()
        (wrappers / "cargo").write_text(textwrap.dedent("""\
            #!/bin/sh
            here=$(dirname "$0")
            IFS=:
            for entry in $PATH; do
                [ "$entry" != "$here" ] && [ -x "$entry/cargo" ] && exec "$entry/cargo" "$@"
            done
            exit 127
        """))
        (wrappers / "cargo").chmod(0o755)
        env = self.remote_env()
        # Rustup's Cargo follows the wrapper, ahead of any wrapper of this
        # host's own, which would look for the next `cargo` the same way.
        env["PATH"] = os.pathsep.join(
            [str(wrappers), os.path.join(env["CARGO_HOME"], "bin"), env["PATH"]])
        result = self.gate_run("exec", "sh", "-ec", "cargo build --bin probe",
                               env=env, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.crate / "target/debug/probe").is_file())
        self.assertFalse((self.root / "local-build").exists())

    def test_cargo_in_an_exec_command_reached_by_path_does_not_compile_here(self):
        cargo = os.path.join(self.env["CARGO_HOME"], "bin", "cargo")
        for command in (f"'{cargo}' build --bin probe", '"$CARGO_PATH" build --bin probe'):
            with self.subTest(command=command):
                result = self.gate_run("exec", "sh", "-ec", command,
                                       env=self.remote_env() | {"CARGO_PATH": cargo}, timeout=60)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("would compile on this host", result.stderr)
                # Cargo made its directories here, and compiled nothing.
                self.assertEqual([path for path in self.root.rglob("probe*")
                                  if path.is_file() and "host" not in path.parts], [])
        # Cargo's own queries of rustc still answer.
        result = self.gate_run("exec", "sh", "-ec", f"'{cargo}' metadata --format-version 1 "
                               ">/dev/null && printf ok", env=self.remote_env(), timeout=60)
        self.assertEqual((result.returncode, result.stdout), (0, "ok"), result.stderr)

    def shell_program(self):
        # Exercise Offload.launch with a real Cargo-built program, which runs
        # the commands after the remote lease and its children have ended.
        (self.crate / "src/main.rs").write_text(textwrap.dedent("""
            fn main() {
                let status = std::process::Command::new("sh")
                    .args(std::env::args().skip(1)).status().unwrap();
                std::process::exit(status.code().unwrap_or(127));
            }
        """))

    def test_a_program_run_inside_exec_reenters_the_gate_for_named_cargo(self):
        self.shell_program()
        result = self.gate_run(
            "exec", "cargo", "run", "--quiet", "--", "-ec",
            "cargo build --bin probe; cargo metadata --format-version 1 >/dev/null",
            env=self.remote_env() | {"CAGENTS_BUILD_REMOTE_SLOTS": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def logging_rustc_wrapper(self):
        wrapper = self.root / "rustc-wrapper"
        wrapper.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >>"$WRAPPER_LOG"\nexec "$@"\n')
        wrapper.chmod(0o755)
        return {"RUSTC_WRAPPER": str(wrapper), "PROBE_WRAPPER": str(wrapper),
                "WRAPPER_LOG": str(self.root / "wrapper-log")}

    def test_a_program_run_inside_exec_cannot_compile_through_absolute_cargo(self):
        self.shell_program()
        # The program's $CARGO is the first `cargo` on this host's PATH. A
        # shim such as mise's, outside a directory where it manages Rust,
        # looks `cargo` up on PATH again, finds the gate's and builds on the
        # build host, so nothing reaches the guard. rustup's Cargo, first
        # here as on the build host, compiles here.
        cargo = os.path.join(self.env["CARGO_HOME"], "bin", "cargo")
        env = self.remote_env() | self.logging_rustc_wrapper()
        env["PATH"] = os.pathsep.join([os.path.dirname(cargo), env["PATH"]])
        result = self.gate_run(
            "exec", "cargo", "run", "--quiet", "--", "-ec",
            '"$RUSTC_WRAPPER" rustc -vV >/dev/null; printf %s "$CARGO"; "$CARGO" build --bin probe',
            env=env)
        self.assertEqual(result.stdout, cargo, result.stderr)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("would compile on this host", result.stderr)
        self.assertEqual([path for path in (self.root / "local-build").rglob("probe*")
                          if path.is_file()], [])
        # The guard still hands queries to the caller's saved wrapper.
        queries = (self.root / "wrapper-log").read_text()
        self.assertIn("-vV", queries)
        self.assertNotIn("--crate-name probe ", queries)

    def test_a_program_run_outside_exec_keeps_the_callers_environment(self):
        self.shell_program()
        result = self.gate_run(
            "run", "--quiet", "--", "-ec",
            'test "$RUSTC_WRAPPER" = "$PROBE_WRAPPER"; '
            'test -z "${CAGENTS_BUILD_EXEC_SHIMS-}"; '
            '"$RUSTC_WRAPPER" rustc -vV >/dev/null',
            env=self.remote_env() | self.logging_rustc_wrapper())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("-vV", (self.root / "wrapper-log").read_text())

    def test_documentation_opens_here(self):
        (self.crate / "src/lib.rs").write_text("//! Probe documentation.\n")
        opened = self.root / "opened"
        browser = self.root / "browser"
        browser.write_text(f"#!/bin/sh\nprintf '%s' \"$1\" >{opened}\n")
        browser.chmod(0o755)
        result = self.gate_run("doc", "--no-deps", "--open",
                               env=self.remote_env() | {"BROWSER": str(browser)})
        self.assertEqual(result.returncode, 0, result.stderr)
        deadline = time.monotonic() + 10
        while not opened.exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.1)
        page = Path(opened.read_text())
        self.assertTrue(page.is_file(), page)
        self.assertTrue(str(page).startswith(str(self.crate.resolve())), page)

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
        # The stand-in host's private temporary root; on a real host it is
        # /var/tmp/cof-<uid>.
        tmp = self.host_home / "cof"
        (self.crate / "build.rs").write_text(textwrap.dedent(f"""
            fn main() {{
                let dir = std::env::temp_dir();
                assert_eq!(dir.parent(), Some(std::path::Path::new({json.dumps(str(tmp))})));
                assert!(dir.to_str().unwrap().ends_with("-0"), "{{}}", dir.display());
                std::fs::write(dir.join("left-behind"), "").unwrap();
                println!("cargo:rerun-if-changed=build.rs");
            }}
        """))
        env = self.remote_env()
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        [slot_tmp] = tmp.iterdir()
        self.assertTrue((slot_tmp / "left-behind").exists())
        (self.crate / "src/main.rs").write_text('fn main() { println!("changed"); }\n')
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((slot_tmp / "left-behind").exists())

    def test_the_build_may_open_as_many_files_as_the_host_allows(self):
        # However low the caller's soft limit, and the stand-in session's
        # 256, the build gets the hard limit.
        (self.crate / "build.rs").write_text(textwrap.dedent("""
            fn main() {
                let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
                let line = limits.lines().find(|l| l.starts_with("Max open files")).unwrap();
                let fields: Vec<_> = line.split_whitespace().collect();
                assert_eq!(fields[3], fields[4], "{line}");
                println!("cargo:rerun-if-changed=build.rs");
            }
        """))
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        result = self.scope.run(
            [str(self.gate), "check"], cwd=self.crate, env=self.remote_env(),
            capture_output=True, text=True, timeout=120,
            preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (512, hard)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("building here", result.stderr)

    def test_a_failing_build_keeps_cargos_exit_status(self):
        (self.crate / "src/main.rs").write_text("fn main() { missing(); }\n")
        result = self.gate_run("check", env=self.remote_env())
        self.assertEqual(result.returncode, 101)
        self.assertIn("missing", result.stderr)

    def test_an_unreachable_host_is_waited_for_within_the_admission_budget(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "unreachable",
                                   "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "3"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual(result.stderr.count("unreachable is unreachable; waiting for it"), 1,
                         result.stderr)
        self.assertIn("set CAGENTS_BUILD_REMOTE=local to build here instead", result.stderr)
        self.assertIn("build admission timed out: unreachable is unreachable", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_a_host_that_comes_back_takes_the_build(self):
        # Unreachable for the first two tries.
        tries = self.root / "tries"
        ssh = self.root / "tools/ssh"
        ssh.write_text(ssh.read_text().replace(
            '[ "$1" != unreachable ] || exit 255',
            '[ "$1" != unreachable ] || exit 255\n'
            f'[ "$1" != flaky ] || {{ echo >>{tries}; [ $(wc -l <{tries}) -gt 2 ] || exit 255; }}'))
        result = self.gate_run("build", env=self.remote_env() | {"CAGENTS_BUILD_REMOTE": "flaky"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("flaky is unreachable; waiting for it", result.stderr)
        self.assertTrue((self.crate / "target/debug/probe").is_file())
        self.assertFalse((self.root / "local-build").exists())

    def test_a_host_that_does_not_answer_is_waited_for(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "silent",
                                   "CAGENTS_BUILD_REMOTE_ANSWER_SECONDS": "1",
                                   "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "4"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("silent did not answer within 1s; waiting for it", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_a_lease_that_fails_on_the_host_fails_the_command(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "broken"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 69, result.stderr)
        self.assertIn("broken could not lease a build slot (status 1); set CAGENTS_BUILD_REMOTE=local",
                      result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_tests_stop_under_a_stray_git_marker_and_other_commands_do_not(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "marked"}
        # Every slot is busy: the refusal must not wait for one.
        slots = self.host_home / ".cache/cagents-offload/crate"
        held = []
        for index in range(2):
            (slots / f"slot-{index}").mkdir(parents=True)
            held.append(open(slots / f"slot-{index}/lock", "a"))
            fcntl.flock(held[-1], fcntl.LOCK_EX)
        result = self.gate_run("test", env=env, timeout=30)
        for lock in held:
            lock.close()
        self.assertEqual(result.returncode, 78, result.stderr)
        self.assertIn(f"cargo-gate: {self.host_home}/stray/.git on marked puts the tests inside a Git checkout;",
                      result.stderr)
        self.assertNotIn("waiting", result.stderr)
        self.assertFalse((self.root / "local-build").exists())
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_local_builds_here_silently(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": "local"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "local-build").exists())

    def test_a_host_below_its_disk_floor_is_waited_for(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB": str(10**7),
                                   "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "3"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertRegex(result.stderr, r"stand-in has \d+ GiB free, below its 10000000 GiB floor; waiting")
        self.assertFalse((self.root / "local-build").exists())

    def test_the_build_host_itself_builds_here(self):
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE": socket.gethostname()}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "local-build").exists())

    def test_tests_running_there_keep_their_slot_from_the_next_build(self):
        # A plain `cargo test` runs its tests on the build host, from the
        # slot's target directory. Another build must not rewrite the test
        # executable, or anything else there, until the tests have ended.
        def worktree(path, text):
            (path / "src").mkdir(parents=True, exist_ok=True)
            (path / "tests").mkdir(exist_ok=True)
            for name in ("Cargo.toml", ".gitignore", "src/main.rs"):
                (path / name).write_text((self.crate / name).read_text())
            (path / "tests/slow.rs").write_text(textwrap.dedent("""
                #[test]
                fn slow() {
                    let exe = std::env::current_exe().unwrap();
                    let before = std::fs::read(&exe).unwrap();
                    std::fs::write(exe.with_file_name("started"), "").unwrap();
                    std::thread::sleep(std::time::Duration::from_secs(6));
                    assert!(std::fs::read(&exe).unwrap() == before, "rewritten under the test");
                    eprintln!("%s");
                }
            """) % text)
            if not (path / ".git").exists():
                self.scope.run(["git", "init", "-q"], cwd=path, check=True)
        worktree(self.crate, "first")
        other = self.root / "elsewhere" / "crate"
        worktree(other, "second")
        env = self.remote_env() | {"CAGENTS_BUILD_REMOTE_SLOTS": "1"}
        testing = self.scope.popen([str(self.gate), "test", "--test", "slow"], cwd=self.crate,
                                   env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True)
        deadline = time.monotonic() + 120
        # Beside the test executable, wherever the host's Cargo puts it.
        while not any(self.root.rglob("started")):
            self.assertIsNone(testing.poll(), "the tests ended before they were seen running")
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.1)
        building = self.scope.run([str(self.gate), "test", "--no-run", "--test", "slow"],
                                  cwd=other, env=env, capture_output=True, text=True,
                                  timeout=120)
        out, err = testing.communicate(timeout=60)
        self.assertEqual(testing.returncode, 0, err)
        self.assertEqual(building.returncode, 0, building.stderr)
        self.assertIn("waiting for", building.stderr)

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
        self.assertIn("waiting for a build slot on stand-in", result.stderr)
        self.assertIn("cargo-gate: build admission timed out", result.stderr)
        self.assertFalse((self.root / "local-build").exists())

    def test_the_build_host_admits_a_build_by_the_memory_it_is_expected_to_need(self):
        # Another build holds a slot there, priced as the largest; this one
        # is expected to need more memory than the host has beside it.
        other = self.host_home / ".cache/cagents-offload/crate/slot-1"
        other.mkdir(parents=True)
        lock = open(other / "lock", "a")
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX)
        env = self.remote_env() | {"CAGENTS_BUILD_COST_GIB": "100000",
                                   "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "3"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("waiting for memory on stand-in", result.stderr)
        self.assertNotIn("waiting for a build slot", result.stderr)
        # Alone there, the same build starts: nothing else is expected to
        # take the memory it sees.
        fcntl.flock(lock, fcntl.LOCK_UN)
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def test_a_wait_for_the_build_hosts_cpus_names_what_holds_them(self):
        other = self.host_home / ".cache/cagents-offload/crate/slot-1"
        other.mkdir(parents=True)
        # Another build's lease holds a slot: a process of its own, which
        # what this test starts is not below.
        lease = self.scope.popen(
            [sys.executable, "-c", "import fcntl, sys, time\nlock = open(sys.argv[1], 'a')\n"
             "fcntl.flock(lock, fcntl.LOCK_EX)\nprint(flush=True)\ntime.sleep(600)", str(other / "lock")],
            stdout=subprocess.PIPE)
        self.addCleanup(lambda: (lease.kill(), lease.wait(), lease.stdout.close()))
        lease.stdout.readline()
        write_figures(self.host_home / "figures", cpu=90)
        # A runaway loop on the build host, in a session of its own.
        runaway = self.scope.popen([f"runaway-{os.getpid()}", "-c", "while True: pass"],
                                   executable=sys.executable, start_new_session=True)
        self.addCleanup(lambda: (runaway.kill(), runaway.wait()))
        env = self.remote_env() | {"CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "4",
                                   "CAGENTS_OFFLOAD_BUSIEST_SHOWN": "1000",
                                   "CAGENTS_OFFLOAD_BUSIEST_LEAST": "0.05"}
        result = self.gate_run("check", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertRegex(result.stderr, r"waiting for CPU on stand-in: its CPU pressure is 90%, at or "
                                        r"above the 40% limit; builds use \d+\.\d CPUs? and everything "
                                        r"else \d+\.\d CPUs?; besides builds, the CPUs go to ")
        self.assertRegex(result.stderr, rf"session {runaway.pid} \(runaway-{os.getpid()} -c while True: pass, "
                                        rf"started \d+s ago\): \d+\.\d CPUs? in 1 process")
        self.assertRegex(result.stderr, r"`ps -s \d+` there lists session \d+'s processes, and "
                                        r"`pkill -s \d+` ends them|Session \d+ is ")

    def test_remote_commands_leave_no_core_dumps_unless_asked(self):
        (self.crate / "src/main.rs").write_text(textwrap.dedent("""
            fn main() {}
            #[test]
            fn limit() {
                let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
                let line = limits.lines().find(|line| line.starts_with("Max core file size")).unwrap();
                println!("core={}", line.split_whitespace().nth(4).unwrap());
            }
        """))
        soft, hard = resource.getrlimit(resource.RLIMIT_CORE)
        if hard == 0:
            self.skipTest("core dumps are disabled outright here")

        def allow_cores():
            resource.setrlimit(resource.RLIMIT_CORE, (hard, hard))

        for env, expected in ((self.remote_env(), "core=0"),
                              (self.remote_env() | {"CAGENTS_BUILD_REMOTE_CORE_DUMPS": "1"},
                               "core=" + ("unlimited" if hard == resource.RLIM_INFINITY else str(hard)))):
            result = self.scope.run([str(self.gate), "test", "--", "--nocapture"], cwd=self.crate,
                                    env=env, capture_output=True, text=True, timeout=120,
                                    preexec_fn=allow_cores)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(expected, result.stdout)

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
        gate = self.scope.popen([str(self.gate), "check"], cwd=self.crate, env=self.remote_env(),
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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


class StandInHostPython(unittest.TestCase):
    """Interpreter selection needs no Cargo build or real SSH connection."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-python-shim-")
        self.addCleanup(temporary.cleanup)
        caller_tools = Path(temporary.name)
        self.shim_calls = caller_tools / "invocations"
        shim = caller_tools / "python3"
        # Fail promptly instead of reproducing a shim's recursive exec loop.
        shim.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "${0%/*}/invocations"\nexit 86\n')
        shim.chmod(0o755)
        with patch.dict(os.environ, {"PATH": f"{caller_tools}:{os.environ['PATH']}"}):
            self.host = StandInHost()
            self.addCleanup(self.host.doCleanups)
            self.host.setUp()
        self.environment = self.host.remote_env() | {"CAGENTS_FIXTURE_CALLER_ONLY": "present"}
        self.probe = (
            "import json, os, pathlib, sys; "
            "print(json.dumps({'executable': str(pathlib.Path(sys.executable).resolve()), "
            "'home': os.environ['HOME'], "
            "'caller': os.environ.get('CAGENTS_FIXTURE_CALLER_ONLY')}))"
        )

    def assert_interpreter(self, result, *, caller):
        self.assertFalse(self.shim_calls.exists(), "the caller's python3 shim ran")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {
            "executable": str(Path(sys.executable).resolve()),
            "home": str(self.host.host_home),
            "caller": caller,
        })

    def test_copied_gate_ignores_callers_python_shim(self):
        result = self.host.gate_run("exec", "python3", "-c", self.probe,
                                    env=self.environment, timeout=10)
        self.assert_interpreter(result, caller="present")

    def test_clean_stand_in_host_ignores_callers_python_shim(self):
        result = self.host.scope.run(["ssh", "stand-in", "python3", "-c", shlex.quote(self.probe)],
                                cwd=self.host.crate, env=self.environment,
                                capture_output=True, text=True, timeout=10)
        self.assert_interpreter(result, caller=None)


def incremental_session(slot, crate, *, age, size=64 * 1024, profile="debug"):
    """A crate's incremental directory in the slot's target, as rustc leaves
    it: a finalized session of `size` bytes and its lock file, last written
    `age` seconds ago."""
    directory = Path(slot, "src", "target", profile, "incremental", crate)
    session = directory / f"s-{crate}-0-0"
    session.mkdir(parents=True)
    (session / "query-cache.bin").write_bytes(os.urandom(size))
    (directory / f"s-{crate}-0.lock").touch()
    then = time.time() - age
    for path in (session / "query-cache.bin", session, directory / f"s-{crate}-0.lock"):
        os.utime(path, (then, then))
    return directory


def wait_until(condition, timeout=10):
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError("timed out")
        time.sleep(0.05)


class RemoteRun(unittest.TestCase):
    """The build host's end of a command: it ends what it started when the
    connection to this host closes, outside any supervisor here."""

    def agent(self, scope, *arguments, env=None, **options):
        """Starts the build host's agent in `scope`, as ssh would there. Its
        temporary directories go in the scope: by default they would go in
        the runtime directory, which the host's own builds share."""
        return scope.popen(
            [sys.executable, "-c", offload.REMOTE_AGENT, *arguments],
            env=os.environ | {"CAGENTS_OFFLOAD_TEMPORARY_ROOT": str(Path(scope.name, "cof"))}
            | (env or {}), **options)

    def test_the_remote_launch_runs_its_supplied_memory_wrapper(self):
        with FixtureDirectory() as scope:
            base = Path(scope.name, "slot-0")
            (base / "src").mkdir(parents=True)
            wrapper = "import os, sys; print(os.environ['CAGENTS_MEMORY_MAX'], sys.argv[1:]); sys.exit(37)"
            child = self.agent(scope, "run", str(base), ".", "sh", "-c", "exit 99",
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            child.stdin.write(json.dumps({"environment": {"CAGENTS_MEMORY_MAX": "128M"},
                                          "memory_cap": wrapper}).encode() + b"\n")
            child.stdin.flush()
            self.assertEqual(child.stdout.read().decode().strip(), "128M ['sh', '-c', 'exit 99']")
            self.assertEqual(child.wait(timeout=30), 37)
            child.stdin.close()
            child.stdout.close()

    def test_a_released_slot_waits_for_its_command_to_end(self):
        # A cancelled build closes the lease and the command's connections
        # together; the next build must not start in the slot while the
        # command's process group is still being ended.
        with FixtureDirectory() as scope:
            base = scope.name
            lease = self.agent(scope, "lease", base, "1", "key", "0", "",
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            self.assertRegex(offload.Lines(lease.stdout).next(10), r'^slot \{"index": 0, ')
            with open(Path(base, "slot-0", "running"), "a") as running:
                fcntl.flock(running, fcntl.LOCK_EX)
                lease.stdin.close()
                with self.assertRaises(subprocess.TimeoutExpired):
                    lease.wait(timeout=1)
            self.assertEqual(lease.wait(timeout=10), 0)

    def test_pruning_waits_for_the_slots_command_to_end(self):
        with FixtureDirectory() as scope:
            base = scope.name
            Path(base, "src").mkdir()
            stale = Path(base, "src", "stale.rs")
            stale.write_text("")
            with open(Path(base, "running"), "a") as running:
                fcntl.flock(running, fcntl.LOCK_EX)
                prune = self.agent(scope, "prune", base, stdin=subprocess.PIPE)
                prune.stdin.close()
                with self.assertRaises(subprocess.TimeoutExpired):
                    prune.wait(timeout=1)
                self.assertTrue(stale.exists())
            self.assertEqual(prune.wait(timeout=10), 0)
            self.assertFalse(stale.exists())

    def test_a_slot_is_handed_out_only_after_a_lingering_command_ends(self):
        # The previous lease's connection dropped while its command's did not.
        with FixtureDirectory() as scope:
            base = scope.name
            Path(base, "slot-0").mkdir()
            with open(Path(base, "slot-0", "running"), "a") as running:
                fcntl.flock(running, fcntl.LOCK_EX)
                lease = self.agent(scope, "lease", base, "1", "key", "0", "",
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
                lines = offload.Lines(lease.stdout)
                # It says it is waiting, so a long wait is not taken for a
                # host that does not answer.
                self.assertEqual(lines.next(10), "wait")
                with self.assertRaises(TimeoutError):
                    lines.next(1)
            line = lines.next(10)
            self.assertEqual(json.loads(line.removeprefix("slot ")),
                             {"index": 0, "path": os.path.realpath(Path(base, "slot-0"))})
            lease.stdin.close()
            self.assertEqual(lease.wait(timeout=10), 0)

    @contextlib.contextmanager
    def slots(self):
        """Settle leases and their descendants before removing the slots."""
        with FixtureDirectory() as scope:
            self.scope = scope
            yield scope.name

    def lease(self, base, *, slots=2, build="", slot_disk=0, figures=None, registry="", env=None,
              floor=0, budget=""):
        if figures is None:
            figures = Path(base, "figures")
            write_figures(figures)
        lease = self.agent(
            self.scope, "lease", base, str(slots), "key", str(floor), "", "", build, str(budget),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            env={"CAGENTS_BUILD_REGISTRY": str(registry),
                 "CAGENTS_OFFLOAD_SLOT_DISK_BYTES": str(slot_disk),
                 "CAGENTS_OFFLOAD_FIGURES": str(figures)} | (env or {}))
        return lease, offload.Lines(lease.stdout)

    def hold(self, base, index, claim=None):
        """Leases slot `index` as another build would."""
        slot = Path(base, f"slot-{index}")
        (slot / "src").mkdir(parents=True, exist_ok=True)
        (slot / "owner").write_text("key\n")
        if claim is not None:
            (slot / "claim").write_text(json.dumps(claim))
        lock = open(slot / "lock", "a")
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX)
        return lock

    def test_a_lease_trims_its_slot_before_the_build_and_keeps_what_a_rustc_holds(self):
        with self.slots() as base:
            slot = Path(base, "slot-0")
            (slot / "owner").parent.mkdir(parents=True)
            (slot / "owner").write_text("key\n")
            stale = incremental_session(slot, "stale-1", age=3600)
            compiling = incremental_session(slot, "compiling-2", age=7200)
            with open(next(compiling.glob("s-*.lock")), "rb") as rustc:
                fcntl.flock(rustc, fcntl.LOCK_EX)
                lease, lines = self.lease(base, budget=0)
                self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)
            self.assertFalse(stale.exists())
            self.assertTrue(compiling.exists())
            # What it moved out goes in the background.
            wait_until(lambda: not any((slot / "evicted").iterdir()))
            lease.stdin.close()
            self.assertEqual(lease.wait(timeout=10), 0)

    def test_below_twice_the_floor_a_lease_trims_the_slots_no_lease_holds(self):
        free = shutil.disk_usage(tempfile.gettempdir()).free // 2**30
        if free < 4:
            self.skipTest("too little free space to place a floor below it")
        # Between the floor and twice it, and with room.
        roomy = shutil.disk_usage(tempfile.gettempdir()).free >= \
            agent_scope()["pressure_floor"](tempfile.gettempdir(), 0)
        for floor, pressed in ((free * 2 // 3, True), (0, False)):
            if not pressed and not roomy:
                continue
            with self.subTest(pressed=pressed), self.slots() as base:
                idle = incremental_session(Path(base, "slot-0"), "idle-1", age=60)
                Path(base, "slot-0", "owner").write_text("other\n")
                mine = incremental_session(Path(base, "slot-1"), "mine-1", age=60)
                Path(base, "slot-1", "owner").write_text("key\n")
                lock = self.hold(base, 2)
                Path(base, "slot-2", "owner").write_text("other\n")
                leased = incremental_session(Path(base, "slot-2"), "leased-1", age=60)
                lease, lines = self.lease(base, slots=3, floor=floor, budget=0)
                self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 1)
                self.assertFalse(mine.exists())
                self.assertEqual(idle.exists(), not pressed)
                self.assertTrue(leased.exists())
                lease.stdin.close()
                self.assertEqual(lease.wait(timeout=10), 0)
                lock.close()

    def test_a_further_slot_is_made_while_the_disk_holds_one(self):
        with self.slots() as base:
            self.hold(base, 0, {"cost": 1, "pid": None})
            lease, lines = self.lease(base)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot ")),
                             {"index": 1, "path": os.path.realpath(Path(base, "slot-1"))})

    def test_no_further_slot_is_made_without_disk_for_one_or_beyond_the_most(self):
        for slots, slot_disk in ((2, 2**62), (1, 0)):
            with self.subTest(slots=slots, slot_disk=slot_disk), \
                    self.slots() as base:
                lock = self.hold(base, 0, {"cost": 1, "pid": None})
                lease, lines = self.lease(base, slots=slots, slot_disk=slot_disk)
                self.assertEqual(lines.next(10), "wait")
                with self.assertRaises(TimeoutError):
                    lines.next(1.5)
                self.assertFalse(Path(base, "slot-1").exists())
                fcntl.flock(lock, fcntl.LOCK_UN)
                self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)

    def test_slots_made_lately_count_as_full_until_they_fill(self):
        with self.slots() as base:
            stat = os.statvfs(base)
            slot_disk = int(stat.f_bavail * stat.f_frsize * 0.6)
            self.hold(base, 0, {"cost": 1, "pid": None})
            first, lines = self.lease(base, slots=8, slot_disk=slot_disk)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 1)
            # Slot 1 holds nothing yet, but will.
            second, lines = self.lease(base, slots=8, slot_disk=slot_disk)
            self.assertEqual(lines.next(10), "wait")
            with self.assertRaises(TimeoutError):
                lines.next(1.5)
            self.assertFalse(Path(base, "slot-2").exists())

    def test_the_same_build_waits_for_its_slot_rather_than_build_again_elsewhere(self):
        with self.slots() as base:
            lock = self.hold(base, 0, {"cost": 1, "pid": None, "build": "same", "key": "key"})
            lease, lines = self.lease(base, build="same")
            self.assertEqual(lines.next(10), "wait same")
            with self.assertRaises(TimeoutError):
                lines.next(1.5)
            self.assertFalse(Path(base, "slot-1").exists())
            # Another build goes ahead of it, into a further slot.
            other, others = self.lease(base, build="other")
            self.assertEqual(json.loads(others.next(10).removeprefix("slot "))["index"], 1)
            fcntl.flock(lock, fcntl.LOCK_UN)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)

    def test_a_build_that_waited_for_the_same_one_gets_back_in_line(self):
        with self.slots() as base:
            lock = self.hold(base, 0, {"cost": 1, "pid": None, "build": "same", "key": "key"})
            waiter, waits = self.lease(base, slots=1, build="same")
            self.assertEqual(waits.next(10), "wait same")
            # The head of the line meanwhile, as another build would be.
            line = open(Path(base, "queue"), "a")
            self.addCleanup(line.close)
            fcntl.flock(line, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(lock, fcntl.LOCK_UN)
            # The slot is free, and it still takes its turn.
            self.assertEqual(waits.next(10), "wait")
            with self.assertRaises(TimeoutError):
                waits.next(1.5)
            line.close()
            self.assertEqual(json.loads(waits.next(10).removeprefix("slot "))["index"], 0)

    def test_a_build_beside_another_waits_until_the_host_has_room(self):
        for figures, reason in (({"cpu": 90}, "wait cpu 90 40"),
                                ({"memory": 50}, "wait pressure 50 10"),
                                ({"available_gib": 1}, "wait memory 1.0 9.0 1")):
            with self.subTest(reason), self.slots() as base:
                stand_in = Path(base, "figures")
                write_figures(stand_in, **figures)
                self.hold(base, 0, {"cost": 1, "pid": None})
                _, lines = self.lease(base, figures=stand_in)
                line = lines.next(10)
                if reason.startswith("wait cpu "):
                    # Followed by what holds the CPUs; the other slot's
                    # holder here is the test, which spins on nothing.
                    self.assertRegex(line, rf"^{reason} -- (nothing (else )?uses much|builds use "
                                           rf"\d+\.\d CPUs? and everything else \d+\.\d CPUs?)")
                else:
                    self.assertEqual(line, reason)
                with self.assertRaises(TimeoutError):
                    lines.next(1.5)
                write_figures(stand_in)
                self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 1)

    def registered_elsewhere(self, registry, kib, record="0 {kib}\n0\n{lock}\n", name="gate-1"):
        """A build in another slot family on this host, as scripts/cargo-gate
        records one: its lock held, and its record naming that lock. By
        default its holder has not started, so all of its cost is to come."""
        registry.mkdir(exist_ok=True)
        lock_path = registry.parent / f"{name}.lock"
        lock = open(lock_path, "a")
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX)
        (registry / name).write_text(record.format(kib=kib, lock=lock_path))
        return lock

    def test_a_record_whose_holder_died_is_priced_at_the_largest_cost(self):
        with tempfile.TemporaryDirectory() as host, self.slots() as base:
            registry = Path(host, "registry")
            stand_in = Path(base, "figures")
            write_figures(stand_in, available_gib=12)
            dead = self.scope.popen(["true"])
            dead.wait()
            # 1 KiB recorded, but 9 GiB expected: too little left for 9 more.
            self.registered_elsewhere(registry, 1, f"{dead.pid} {{kib}}\n0\n{{lock}}\n")
            _, lines = self.lease(base, figures=stand_in, registry=registry)
            self.assertEqual(lines.next(10).split()[:2], ["wait", "memory"])
            with self.assertRaises(TimeoutError):
                lines.next(1.5)

    def test_a_build_in_another_slot_family_counts_as_running_beside_it(self):
        with tempfile.TemporaryDirectory() as host, self.slots() as base:
            registry = Path(host, "registry")
            stand_in = Path(base, "figures")
            write_figures(stand_in, available_gib=12)
            # Alone in these slots, but not on the host: the other family's
            # build is expected to need 4 GiB more, leaving too little for 9.
            lock = self.registered_elsewhere(registry, 4 * 2**20)
            _, lines = self.lease(base, figures=stand_in, registry=registry)
            self.assertEqual(lines.next(10).split()[:2], ["wait", "memory"])
            with self.assertRaises(TimeoutError):
                lines.next(1.5)
            # Once it ends, its record is stale and this one is alone.
            fcntl.flock(lock, fcntl.LOCK_UN)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)

    def test_a_leased_slot_is_recorded_for_other_families_until_released(self):
        with tempfile.TemporaryDirectory() as host, self.slots() as base:
            registry = Path(host, "registry")
            lease, lines = self.lease(base, registry=registry)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)
            [entry] = records(registry)
            first, _since, lock, late = entry.read_text().splitlines()
            self.assertEqual(lock, os.path.join(os.path.realpath(base), "slot-0", "lock"))
            self.assertEqual(first, f"0 {9 * 2**20}")
            self.assertEqual(late, "")
            lease.stdin.close()
            lease.wait(timeout=10)
            self.assertEqual(records(registry), [])

    def test_a_slot_reached_through_a_symbolic_link_is_recorded_once(self):
        with tempfile.TemporaryDirectory() as host, self.slots() as base:
            registry = Path(host, "registry")
            linked = Path(host, "linked")
            linked.symlink_to(base)
            lease, lines = self.lease(str(linked), registry=registry)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)
            [entry] = records(registry)
            self.assertEqual(entry.read_text().splitlines()[2],
                             os.path.join(os.path.realpath(base), "slot-0", "lock"))
            # As run records the build under the slot's real path.
            agent = offload.REMOTE_AGENT.split("\nif command == ")[0]
            self.scope.run(
                [sys.executable, "-c", agent + "\nimport sys\nregister(sys.argv[2], 2**30, os.getpid())",
                 "lease", os.path.join(os.path.realpath(base), "slot-0")],
                env=os.environ | {"CAGENTS_BUILD_REGISTRY": str(registry)}, check=True)
            self.assertEqual(records(registry), [entry])
            lease.stdin.close()
            lease.wait(timeout=10)
            self.assertEqual(records(registry), [])

    def uptime(self):
        return int(float(Path("/proc/uptime").read_text().split()[0]))

    def test_an_overdue_build_gives_way_to_a_gate_refused_longer_or_ramping_up(self):
        pid = os.getpid()
        started = int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19])
        for name, record in (
                ("wait-gate-older", "{pid} {started} {older} {now}\n"),
                ("gate-ramping", "{pid} {{kib}}\n{now}\n{{lock}}\noverdue\n")):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as host, \
                    self.slots() as base:
                registry = Path(host, "registry")
                stand_in = Path(base, "figures")
                write_figures(stand_in, available_gib=12)
                self.registered_elsewhere(registry, 4 * 2**20)
                now = self.uptime()
                record = record.format(pid=pid, started=started, older=now - 60, now=now)
                if name.startswith("wait-"):
                    (registry / name).write_text(record)
                else:
                    self.registered_elsewhere(registry, 1, record, name=name)
                _, lines = self.lease(base, figures=stand_in, registry=registry,
                                      env={"CAGENTS_BUILD_WAIT_PRIORITY_SECS": "1"})
                self.assertEqual(lines.next(10).split()[:2], ["wait", "memory"])
                with self.assertRaises(TimeoutError):
                    lines.next(3)

    def test_the_build_waiting_longest_starts_whatever_the_memory(self):
        with tempfile.TemporaryDirectory() as host, self.slots() as base:
            registry = Path(host, "registry")
            stand_in = Path(base, "figures")
            write_figures(stand_in, available_gib=12)
            self.registered_elsewhere(registry, 4 * 2**20)
            _, lines = self.lease(base, figures=stand_in, registry=registry,
                                  env={"CAGENTS_BUILD_WAIT_PRIORITY_SECS": "2"})
            self.assertEqual(lines.next(10).split()[:2], ["wait", "memory"])
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)
            entries = {path.name: path.read_text().splitlines() for path in records(registry)}
            [mine] = [lines for name, lines in entries.items() if name.startswith("offload-")]
            self.assertEqual(mine[3], "overdue")
            self.assertFalse(any(path.name.startswith("wait-") for path in registry.iterdir()))

    def test_a_lone_build_starts_however_busy_the_host_is(self):
        with self.slots() as base:
            stand_in = Path(base, "figures")
            write_figures(stand_in, available_gib=1, cpu=90, memory=50)
            _, lines = self.lease(base, figures=stand_in)
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 0)

    def test_the_same_build_from_another_checkout_is_not_waited_for(self):
        with self.slots() as base:
            self.hold(base, 0, {"cost": 1, "pid": None, "build": "same", "key": "elsewhere"})
            lease, lines = self.lease(base, build="same")
            self.assertEqual(json.loads(lines.next(10).removeprefix("slot "))["index"], 1)

    def run_agent(self, scope, base, command):
        Path(base, "src").mkdir(exist_ok=True)
        # Its standard input stays open: closing it cancels the command.
        agent = self.agent(scope, "run", base, ".", *command,
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        agent.stdin.write(b'{"environment": {}}\n')
        agent.stdin.flush()
        output = agent.stdout.read()
        self.assertEqual(agent.wait(timeout=30), 0)
        agent.stdin.close()
        agent.stdout.close()
        return output.decode()

    def phased_run(self, *, least=None, doctests=True):
        """Runs a command shaped as `cargo test` is in the agent: a compile, a
        test harness Cargo built into `deps`, then, with `doctests`, a compile
        again. Returns the slot's claimed costs during each step, the costs
        its host record named, and the build's own cost."""
        build_cost = 9 * 2**30
        with FixtureDirectory() as scope, tempfile.TemporaryDirectory() as host:
            root = scope.name
            base = Path(root, "slot-0")
            (base / "src").mkdir(parents=True)
            (base / "claim").write_text(json.dumps({"cost": build_cost, "pid": None,
                                                    "build": "", "key": "key"}))
            self.hold(root, 0)
            # Stand-ins named as a compiler and as a harness are: copies of
            # this Python, which, unlike a multi-call coreutils, keeps
            # working under another name.
            python = os.path.realpath(sys.executable)
            tools = Path(host, "toolchain", "bin")
            harnesses = Path(host, "target", "debug", "deps")
            for directory in (tools, harnesses):
                directory.mkdir(parents=True)
            shutil.copy(python, tools / "rustc")
            shutil.copy(python, harnesses / "cagents-0123456789abcdef")
            steps = Path(host, "steps")
            pause = "import sys, time; time.sleep(float(sys.argv[1]))"
            script = (f'"{tools}/rustc" -c "$0" 1.5; touch "{steps}.test"; '
                      f'"{harnesses}/cagents-0123456789abcdef" -c "$0" 2')
            if doctests:
                script += f'; touch "{steps}.doc"; "{tools}/rustc" -c "$0" 1.5'
            command = ["sh", "-ec", script, pause]
            registry = Path(host, "registry")
            settings = {"CAGENTS_OFFLOAD_PHASE_INTERVAL": "0.1",
                        "CAGENTS_BUILD_REGISTRY": str(registry)}
            if least is not None:
                settings["CAGENTS_OFFLOAD_TEST_LEAST_BYTES"] = str(least)
            agent = self.agent(scope, "run", str(base), ".", *command,
                               stdin=subprocess.PIPE, env=settings)
            # A relocatable Python finds its library by its own path, which
            # its copies do not share.
            request = {"environment": {"PYTHONHOME": sys.base_prefix}}
            agent.stdin.write(json.dumps(request).encode() + b"\n")
            agent.stdin.flush()
            seen = {"build": set(), "test": set(), "doc": set()}
            recorded = set()
            while agent.poll() is None:
                phase = ("doc" if Path(f"{steps}.doc").exists()
                         else "test" if Path(f"{steps}.test").exists() else "build")
                with contextlib.suppress(OSError, ValueError):
                    seen[phase].add(json.loads((base / "claim").read_text())["cost"])
                for record in records(registry) if registry.exists() else ():
                    with contextlib.suppress(OSError, IndexError, ValueError):
                        recorded.add(int(record.read_text().split()[1]) * 1024)
                time.sleep(0.05)
            self.assertEqual(agent.returncode, 0)
            agent.stdin.close()
            # Priced as a build again once it ends, whatever it did last: a
            # hold builds again in the slot under the same claim.
            self.assertEqual(json.loads((base / "claim").read_text())["cost"], build_cost)
            for record in records(registry):
                self.assertEqual(int(record.read_text().split()[1]) * 1024, build_cost)
            return seen, recorded, build_cost

    def test_a_command_that_ends_testing_leaves_its_slot_priced_as_a_build(self):
        seen, _, build_cost = self.phased_run(doctests=False)
        self.assertIn(2 * 2**30, seen["test"])

    def test_a_command_running_its_tests_is_priced_at_least_as_a_build_is(self):
        # A remote `cargo test` runs its harnesses in the slot. Once it only
        # tests, the compile's expected cost no longer applies: builds the
        # host admits meanwhile may use that memory. Compiling again, as
        # doctests do, prices it as a build again.
        seen, recorded, build_cost = self.phased_run()
        least = 2 * 2**30
        self.assertEqual(seen["build"], {build_cost})
        # Its harness holds far less than the least a build is priced at.
        self.assertIn(least, seen["test"])
        self.assertIn(build_cost, seen["doc"])
        # Other slot families on the host read the same price.
        self.assertEqual(recorded, {build_cost, least})

    def test_a_command_running_its_tests_is_priced_by_what_they_hold(self):
        # Without the floor, the price is what the command's processes hold:
        # the agent, the shell and the harness, measured, not a set figure.
        seen, recorded, build_cost = self.phased_run(least=1)
        tested = seen["test"] - {build_cost}
        self.assertTrue(tested, seen)
        for price in tested:
            self.assertGreater(price, 2**20)
            self.assertLess(price, 2**30)
        self.assertIn(build_cost, seen["doc"])
        self.assertTrue(tested & recorded, (tested, recorded))

    def test_a_command_in_a_lease_starts_from_the_leases_price(self):
        # A command before it under the same hold repriced the claim for its
        # tests and was killed before it could restore it.
        with FixtureDirectory() as scope:
            root = scope.name
            base = os.path.join(root, "slot-0")
            os.makedirs(os.path.join(base, "src"))
            Path(base, "claim").write_text(json.dumps(
                {"cost": 2 * 2**30, "leased": 9 * 2**30, "pid": None, "build": "", "key": "key"}))
            self.hold(root, 0)
            self.run_agent(scope, base, ["true"])
            claimed = json.loads(Path(base, "claim").read_text())
            self.assertEqual((claimed["cost"], claimed["leased"]), (9 * 2**30, 9 * 2**30))

    def test_a_build_shares_the_hosts_cpus_with_the_builds_running_there(self):
        cpus = len(os.sched_getaffinity(0))
        with FixtureDirectory() as scope:
            root = scope.name
            base = os.path.join(root, "slot-0")
            os.makedirs(base)
            self.hold(root, 0)
            echo = ["sh", "-c", "printf %s \"$CARGO_BUILD_JOBS\""]
            most = max(4, min(8, cpus // 2))
            self.assertEqual(self.run_agent(scope, base, echo), str(min(max(4, cpus), most)))
            for index in (1, 2, 3):
                self.hold(root, index)
            self.assertEqual(self.run_agent(scope, base, echo), str(min(max(4, cpus // 4), most)))

    def test_closing_the_connection_ends_the_command_and_its_children(self):
        self.assert_connection_cleanup(capped=False)

    def test_closing_the_connection_ends_a_memory_capped_command_and_its_children(self):
        self.assert_connection_cleanup(capped=True)

    def assert_connection_cleanup(self, *, capped):
        with FixtureDirectory() as scope:
            base = scope.name
            Path(base, "src").mkdir()
            marker = Path(base, "pid")
            agent = self.agent(scope, "run", base, ".",
                               "sh", "-c", f"sleep 120 & echo $! >{marker}; wait",
                               stdin=subprocess.PIPE, start_new_session=True)
            request = {"environment": {"CAGENTS_MEMORY_MAX": "128M", "CAGENTS_MEMORY_SWAP_MAX": "0"}}
            if capped:
                request["memory_cap"] = (ROOT / "scripts/memory-capped").read_text()
            agent.stdin.write(json.dumps(request).encode() + b"\n")
            agent.stdin.flush()
            deadline = time.monotonic() + 10
            while not marker.exists() or not marker.read_text().strip():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.05)
            sleeper = int(marker.read_text())
            agent.stdin.close()
            # This runner adopts the sleeper; reaping it as it dies, as init
            # would, lets the agent see the command's group end.
            deadline = time.monotonic() + 15
            while agent.poll() is None:
                self.assertLess(time.monotonic(), deadline, "the agent outlived its connection")
                with contextlib.suppress(ChildProcessError):
                    os.waitpid(sleeper, os.WNOHANG)
                time.sleep(0.05)
            self.assertEqual(agent.returncode, 130)
            deadline = time.monotonic() + 5
            while Path(f"/proc/{sleeper}").exists() and Path(
                    f"/proc/{sleeper}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z":
                self.assertLess(time.monotonic(), deadline, "the command's child outlived it")
                time.sleep(0.05)

    def test_a_finished_command_keeps_its_status(self):
        with FixtureDirectory() as scope:
            base = scope.name
            Path(base, "src").mkdir()
            # The connection stays open, as it does while the gate waits.
            agent = self.agent(scope, "run", base, ".", "sh", "-c", 'test "$PROBE" = x && exit 7',
                               stdin=subprocess.PIPE)
            agent.stdin.write(b'{"environment": {"PROBE": "x"}}\n')
            agent.stdin.flush()
            self.assertEqual(agent.wait(timeout=15), 7)

    def test_the_command_may_open_as_many_files_as_the_host_allows(self):
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        # An older gate's request still names a limit, which no longer
        # lowers it.
        for request in ({"environment": {}}, {"environment": {}, "open_files": 64}):
            with self.subTest(request=request), FixtureDirectory() as scope:
                base = scope.name
                Path(base, "src").mkdir()
                # The agent starts low, as an ssh session's 1024 does.
                agent = self.agent(
                    scope, "run", base, ".", "sh", "-c", "ulimit -Sn",
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (32, hard)))
                # The connection stays open, as it does while the gate waits.
                agent.stdin.write(json.dumps(request).encode() + b"\n")
                agent.stdin.flush()
                self.assertEqual(agent.wait(timeout=15), 0)
                reported = agent.stdout.read().decode().strip()
                self.assertEqual(reported, "unlimited" if hard == resource.RLIM_INFINITY
                                 else str(hard))


class Trim(unittest.TestCase):
    """The bound on a slot's build output, which its lease, or the guard for
    a slot no lease holds, applies under the slot's locks."""

    def setUp(self):
        self.agent = agent_scope()
        directory = tempfile.TemporaryDirectory(prefix="cagents-trim-")
        self.addCleanup(directory.cleanup)
        self.slot = Path(directory.name, "slot-0")
        self.slot.mkdir()

    def trim(self, budget, pressed=False):
        return self.agent["trim"](str(self.slot), budget, pressed)

    def session_size(self, crate):
        """What trim counts for `crate`: what its sessions hold, not the
        directories' own blocks, which some file systems give a size."""
        return sum(self.agent["tree_size"](str(session), set())
                   for session in crate.iterdir() if session.is_dir())

    def test_the_crates_compiled_most_recently_stay_within_the_budget(self):
        oldest = incremental_session(self.slot, "oldest-1", age=3 * 3600)
        older = incremental_session(self.slot, "older-1", age=2 * 3600, profile="dev-runtime")
        newer = incremental_session(self.slot, "newer-1", age=3600,
                                    profile="x86_64-unknown-linux-gnu/local")
        newest = incremental_session(self.slot, "newest-1", age=60)
        size = self.session_size(newest)
        moved = self.trim(2 * size)
        self.assertEqual([crate.exists() for crate in (oldest, older, newer, newest)],
                         [False, False, True, True])
        self.assertGreaterEqual(moved, 2 * size)
        # Moved out whole, in one rename each, then removed.
        self.assertEqual(len(list((self.slot / "evicted").iterdir())), 2)
        self.agent["remove_evicted"]([str(self.slot)], wait=True)
        self.assertEqual(list((self.slot / "evicted").iterdir()), [])
        # Under the budget, nothing goes.
        self.assertEqual(self.trim(2 * size), 0)
        self.assertTrue(newer.exists() and newest.exists())

    def test_below_twice_the_floor_a_quarter_of_the_budget_stays(self):
        crates = [incremental_session(self.slot, f"crate-{age}", age=age) for age in (10, 20, 30, 40)]
        size = self.session_size(crates[0])
        self.trim(4 * size, pressed=True)
        self.assertEqual([crate.exists() for crate in crates], [True, False, False, False])

    def test_a_crate_a_rustc_compiles_stays_whatever_the_budget(self):
        compiling = incremental_session(self.slot, "compiling-1", age=7200)
        idle = incremental_session(self.slot, "idle-1", age=60)
        with open(next(compiling.glob("s-*.lock")), "rb") as rustc:
            fcntl.flock(rustc, fcntl.LOCK_EX)
            self.trim(0)
        self.assertTrue(compiling.exists())
        self.assertFalse(idle.exists())

    def test_the_large_session_compiled_last_stays_though_it_crosses_the_budget(self):
        small = incremental_session(self.slot, "small-1", age=3600)
        large = incremental_session(self.slot, "large-1", age=60, size=256 * 1024)
        self.trim(self.session_size(small))
        self.assertEqual((large.exists(), small.exists()), (True, False))

    def test_each_finalized_session_is_measured_once(self):
        crate = incremental_session(self.slot, "crate-1", age=60)
        older = incremental_session(self.slot, "older-1", age=3600)
        size = self.session_size(crate)
        self.trim(2 * size - 1)
        self.assertTrue(older.exists())
        # Rustc never changes a finalized session; one it is writing is
        # measured again each time.
        session = next(path for path in crate.iterdir() if path.is_dir())
        (session / "more.o").write_bytes(os.urandom(64 * 1024))
        self.trim(2 * size - 1)
        self.assertTrue(older.exists())
        session.rename(crate / f"{session.name}-working")
        self.trim(2 * size - 1)
        self.assertTrue(crate.exists())
        self.assertFalse(older.exists())

    def test_an_unreadable_record_of_sizes_is_measured_again(self):
        crate = incremental_session(self.slot, "crate-1", age=60)
        older = incremental_session(self.slot, "older-1", age=3600)
        for record in ("not json", "[]", '{"src/target/debug/incremental/crate-1/s-crate-1-0-0": "x"}'):
            with self.subTest(record=record):
                (self.slot / "incremental-sizes").write_text(record)
                self.trim(self.session_size(crate) + 1)
                self.assertTrue(older.exists())

    def test_units_no_build_looked_at_for_days_go_every_few_hours_or_at_once_when_pressed(self):
        if os.statvfs(self.slot).f_flag & os.ST_NOATIME:
            self.skipTest("this file system keeps no access times")
        profile = self.slot / "src/target/debug"
        def written(path, days):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"output")
            then = time.time() - days * 86400
            os.utime(path, (then, then))
            return path
        def unit(package, crate, unit_hash, *, looked, built=4):
            """A unit laid out as Cargo leaves it; a build reads its
            fingerprint `looked` days ago, its outputs only when it is built."""
            written(profile / f".fingerprint/{package}-{unit_hash}/lib-{crate}", looked)
            return [profile / f".fingerprint/{package}-{unit_hash}",
                    written(profile / f"deps/lib{crate}-{unit_hash}.rlib", built),
                    written(profile / f"deps/{crate}-{unit_hash}.d", built),
                    written(profile / f"build/{package}-{unit_hash}/build-script-build",
                            built).parent]
        stale = unit("old-crate", "old_crate", "1a", looked=4)
        # A build dependency and a compiled build script: every build looks
        # at their fingerprints, though none reads them until they change.
        needed = unit("build-helper", "build_helper", "2b", looked=1)
        # Nothing dates what no fingerprint names.
        unnamed = written(profile / "deps/libloose-3c.rlib", 4)
        self.trim(2**40)
        self.assertEqual([path.exists() for path in stale], [False] * 4)
        self.assertEqual([path.exists() for path in needed], [True] * 4)
        self.assertTrue(unnamed.exists())
        # Not again for hours, unless the disk is low.
        later = unit("later-crate", "later_crate", "4d", looked=4)
        self.trim(2**40)
        self.assertTrue(all(path.exists() for path in later))
        self.trim(2**40, pressed=True)
        self.assertFalse(any(path.exists() for path in later))

    def test_a_removal_a_build_waits_for_does_not_wait_behind_an_idle_one(self):
        evicted = self.slot / "evicted"
        (evicted / "1-1-crate-1" / "s-1").mkdir(parents=True)
        (evicted / "1-1-crate-1" / "s-1" / "query-cache.bin").write_bytes(b"x" * 4096)
        (evicted / "2-1-libold-1.rlib").write_bytes(b"rlib")
        with open(self.slot / "evicted.lock", "a") as idle:
            fcntl.flock(idle, fcntl.LOCK_EX)
            # Another idle one passes over a slot whose remover is at work.
            self.agent["remove_evicted"]([str(self.slot)])
            time.sleep(0.5)
            self.assertEqual(len(list(evicted.iterdir())), 2)
            self.agent["remove_evicted"]([str(self.slot)], wait=True)
            self.assertEqual(list(evicted.iterdir()), [])

    def test_a_slot_held_shared_by_a_trim_is_not_taken_for_a_running_build(self):
        lock = self.slot / "lock"
        with open(lock, "a") as trimming:
            fcntl.flock(trimming, fcntl.LOCK_SH)
            self.assertFalse(self.agent["leased"](str(self.slot)))
        with open(lock, "a") as leasing:
            fcntl.flock(leasing, fcntl.LOCK_EX)
            self.assertTrue(self.agent["leased"](str(self.slot)))

    def test_the_guard_trims_only_the_slots_no_lease_holds(self):
        root = self.slot.parent / "offload"
        idle, leased = (root / "repository" / f"slot-{index}" for index in (0, 1))
        crates = [incremental_session(slot, "crate-1", age=60) for slot in (idle, leased)]
        offloading = [sys.executable, str(ROOT / "scripts/cargo-offload"), "--trim-slots"]
        environment = os.environ | {"CAGENTS_BUILD_OFFLOAD_SLOTS": str(root),
                                    "CAGENTS_BUILD_REMOTE_INCREMENTAL_GIB": "0"}
        with open(leased / "lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = subprocess.run([*offloading, "0", "--wait"], env=environment,
                                    capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([crate.exists() for crate in crates], [False, True])
        self.assertIn("out of 1 idle build slots", result.stdout)
        # It waited for the removal.
        self.assertEqual(list((idle / "evicted").iterdir()), [])
        for arguments in ([], ["x"], ["0", "--later"]):
            with self.subTest(arguments=arguments):
                result = subprocess.run([*offloading, *arguments], env=environment,
                                        capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 2)
                self.assertIn("usage", result.stderr)
        result = subprocess.run([*offloading, "0"], capture_output=True, text=True, timeout=30,
                                env=environment | {"CAGENTS_BUILD_REMOTE_INCREMENTAL_GIB": "-1"})
        self.assertEqual(result.returncode, 2)
        self.assertIn("must be a nonnegative integer", result.stderr)


class Prune(unittest.TestCase):
    """The build host's removal of sources the checkout no longer has."""

    def test_files_not_listed_go_and_the_target_directory_stays(self):
        with FixtureDirectory() as scope:
            base = scope.name
            source = Path(base, "src")
            for name in ("keep.rs", "a/keep.rs", "a/gone.rs", "b/gone.rs", "target/debug/x"):
                Path(source, name).parent.mkdir(parents=True, exist_ok=True)
                Path(source, name).write_text("")
            Path(source, "link").symlink_to("keep.rs")
            scope.run([sys.executable, "-c", offload.REMOTE_AGENT, "prune", base],
                           input=b"keep.rs\0a/keep.rs\0", check=True, timeout=15)
            left = sorted(str(path.relative_to(source)) for path in source.rglob("*"))
            self.assertEqual(left, ["a", "a/keep.rs", "keep.rs", "target", "target/debug",
                                    "target/debug/x"])


if __name__ == "__main__":
    unittest.main()
