#!/usr/bin/env python3
"""The mise task interface: lanes stay feature-disjoint and arguments forward.

The structural cases read mise.toml. The behavioral cases run the real mise
against a copy of it whose scripts record how they were called, so no task
builds, tests or runs anything; they are skipped when mise is not installed.
"""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "mise.toml"
SCRIPT = re.compile(r"\bscripts/[A-Za-z0-9_.-]+")
LANES = {"cli": set(), "host": {"host"}, "ui": {"ui"}, "browser": {"browser-protocol-fixture"}, "editor": set(),
         "dictate": {"engine"}}
# The application is its own package; the UI lanes select it, and `ui` the
# shared library with it. No packages means the root's default member.
PACKAGES = {"cli": set(), "host": set(), "ui": {"cagents", "cagents-ui"}, "browser": {"cagents-ui"},
            "editor": {"ceditor"}, "dictate": {"cagents-dictate"}}


# Which lane task checks each lane of main's build check.
# The usage spec each lane task and each no-argument aggregate repeats.
ALL_TARGETS = ('arg "[cargo_args]..." var=#true double_dash="required" default="--all-targets" '
               'help="Cargo arguments, after --; they replace the default --all-targets"')
NO_ARGUMENTS = 'arg "[unexpected]..." var=#true hide=#true'
SENTINEL_LANES = {"cli": "cli", "host": "host", "ui": "ui", "app": "ui", "browser": "browser",
                  "dictate": "dictate"}


def sentinel_lanes():
    loader = importlib.machinery.SourceFileLoader("main_build_sentinel", str(ROOT / "scripts/main-build-sentinel"))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    return module.LANES


def sentinel_test_lanes():
    loader = importlib.machinery.SourceFileLoader("main_build_sentinel", str(ROOT / "scripts/main-build-sentinel"))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    return module.TEST_LANES


def load():
    return tomllib.loads(CONFIG.read_text())


def commands(task):
    run = task.get("run", [])
    return [run] if isinstance(run, str) else [entry for entry in run if isinstance(entry, str)]


def features(arguments):
    """The selected graph features, whichever package names them."""
    found = set()
    for flag, value in zip(arguments, arguments[1:]):
        if flag == "--features":
            found.update(feature.rpartition("/")[2] for feature in value.split(","))
    return found


def packages(arguments):
    return {value for flag, value in zip(arguments, arguments[1:]) if flag in {"-p", "--package"}}


class Structure(unittest.TestCase):
    def setUp(self):
        self.config = load()
        self.tasks = self.config["tasks"]

    def test_every_task_is_described_for_discovery(self):
        for name, task in self.tasks.items():
            with self.subTest(task=name):
                self.assertTrue(task.get("description", "").strip())

    def test_every_script_a_task_runs_exists_and_is_executable(self):
        for name, task in self.tasks.items():
            for command in commands(task):
                for script in SCRIPT.findall(command):
                    with self.subTest(task=name, script=script):
                        path = ROOT / script
                        self.assertTrue(path.is_file() and os.access(path, os.X_OK), script)

    def test_each_lane_selects_its_own_implementation_graph(self):
        variables = self.config["vars"]
        for lane, expected in LANES.items():
            with self.subTest(lane=lane):
                arguments = variables[lane].split()
                self.assertIn("--locked", arguments)
                self.assertEqual(features(arguments), expected)
                self.assertEqual(packages(arguments), PACKAGES[lane])
                if lane in {"cli", "host", "ui", "dictate"}:
                    self.assertIn("--no-default-features", arguments)

    def test_the_check_lanes_cover_every_lane_of_mains_build_check(self):
        # What main's build check compiles, an author's lane checks first.
        lanes = sentinel_lanes()
        self.assertEqual(set(SENTINEL_LANES), set(lanes))
        variables = self.config["vars"]
        for sentinel, lane in SENTINEL_LANES.items():
            with self.subTest(sentinel=sentinel, lane=lane):
                arguments = variables[lane].split()
                self.assertIn("--no-default-features", arguments)
                self.assertLessEqual(packages(lanes[sentinel]) or {"cagents"}, packages(arguments) or {"cagents"})
                self.assertLessEqual(features(lanes[sentinel]), features(arguments))
                task = self.tasks[f"check:{lane}"]

    def test_mains_test_lanes_run_every_lanes_tests(self):
        # The verdict a release requires runs at least what each lane task tests.
        lanes = sentinel_test_lanes()
        variables = self.config["vars"]
        for lane in LANES:
            with self.subTest(lane=lane):
                arguments = variables[lane].split()
                selection = lanes[lane]
                self.assertEqual("--no-default-features" in selection, "--no-default-features" in arguments)
                self.assertLessEqual(packages(arguments) or {"cagents"}, packages(selection) or {"cagents"})
                self.assertLessEqual(features(arguments), features(selection))

    def test_lane_and_aggregate_tasks_repeat_one_usage_spec(self):
        for name in ("check:cli", "check:host", "check:ui", "check:browser", "test:cli", "test:host", "test:ui"):
            with self.subTest(task=name):
                self.assertEqual(self.tasks[name].get("usage"), ALL_TARGETS)
        for name in ("check", "test", "ci"):
            with self.subTest(task=name):
                self.assertEqual(self.tasks[name].get("usage"), NO_ARGUMENTS)

    def test_no_usage_spec_names_a_var(self):
        # mise 2025.9.9 renders vars in `run` but passes `usage` verbatim, so
        # a templated spec fails to parse as soon as the task gets arguments.
        for name, task in self.tasks.items():
            with self.subTest(task=name):
                self.assertNotIn("{{", task.get("usage", ""))

    def test_no_task_reunifies_host_and_ui(self):
        variables = self.config["vars"]
        for name, task in self.tasks.items():
            for command in commands(task):
                expanded = re.sub(r"\{\{\s*vars\.(\w+)\s*\}\}", lambda m: variables[m.group(1)], command)
                with self.subTest(task=name):
                    # One lane, however often the command names it.
                    self.assertLessEqual(
                        len(set(re.findall(r"\{\{\s*vars\.(cli|host|ui|browser|editor|dictate)\b", command))), 1)
                    selected = features(expanded.split())
                    self.assertFalse({"host", "ui"} <= selected, command)
                    self.assertFalse({"host", "integrated-browser"} <= selected, command)
                    self.assertFalse({"host", "browser-protocol-fixture"} <= selected, command)

    def test_every_compiling_task_goes_through_the_gate(self):
        for name, task in self.tasks.items():
            for command in commands(task):
                with self.subTest(task=name):
                    self.assertIsNone(re.search(r"(^|[;&|]\s*|\bexec\s+)cargo\s+(build|check|test|run)\b", command))

    def test_every_lane_holds_the_workspace_to_main_build_checks_warning_rule(self):
        # scripts/cargo-lane fails on the workspace's own warnings, as the
        # main build sentinel does, and runs the gate itself. A check lints:
        # Clippy type-checks as `cargo check` does and adds the workspace lints.
        for lane in LANES:
            for kind, subcommand in (("check", "clippy"), ("test", "test")):
                task = self.tasks.get(f"{kind}:{lane}")
                if task is None:
                    continue
                with self.subTest(task=f"{kind}:{lane}"):
                    self.assertRegex(commands(task)[0],
                                     rf"\bexec scripts/cargo-lane {subcommand} \{{\{{vars\.{lane}\}}\}}")
                    for command in commands(task):
                        # Each step execs it, so mise's TERM, sent to the step
                        # alone, reaches the gate; and stays in its own lane.
                        self.assertNotRegex(command, r"(?<!exec )scripts/cargo-lane")
                        self.assertEqual(set(re.findall(r"\bscripts/cargo-lane \w+ \{\{vars\.(\w+)\}\}", command)),
                                         {lane})

    def test_aggregates_cover_exactly_the_three_isolated_lanes(self):
        groups = [entry["tasks"] for entry in self.tasks["check"]["run"] if isinstance(entry, dict)]
        self.assertEqual(groups, [["check:cli", "check:host", "check:ui"]])
        self.assertEqual([entry["task"] for entry in self.tasks["test"]["run"] if isinstance(entry, dict)],
                         ["test:cli", "test:host", "test:ui"])

    def test_nothing_is_task_cached(self):
        # Cargo decides what is stale; native runs and mutable state are never artifacts.
        for name, task in self.tasks.items():
            with self.subTest(task=name):
                self.assertNotIn("sources", task)
                self.assertNotIn("outputs", task)

    def test_interactive_tasks_keep_the_terminal(self):
        for name in ("dev", "dev:browser", "install"):
            with self.subTest(task=name):
                self.assertIs(self.tasks[name].get("raw"), True)


# Python, so a task may also run a script through the interpreter.
RECORDER = """#!/usr/bin/python3
import json, os, sys
from pathlib import Path
with open(os.environ["MISE_TASKS_LOG"], "a") as log:
    log.write(json.dumps([Path(sys.argv[0]).name, *sys.argv[1:]]) + "\\n")
# A subcommand's own status, such as MISE_TASKS_STATUS_check, wins.
sys.exit(int(os.environ.get(f"MISE_TASKS_STATUS_{sys.argv[1] if len(sys.argv) > 1 else ''}",
                            os.environ.get("MISE_TASKS_STATUS", "0"))))
"""


@unittest.skipUnless(shutil.which("mise"), "mise is not installed")
class Behavior(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-mise-tasks-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        checkout = self.root / "checkout"
        (checkout / "scripts").mkdir(parents=True)
        shutil.copy(CONFIG, checkout / "mise.toml")
        shutil.copy(ROOT / "rust-toolchain.toml", checkout / "rust-toolchain.toml")
        scripts = {script for task in load()["tasks"].values() for command in commands(task)
                   for script in SCRIPT.findall(command)}
        for script in scripts:
            path = checkout / script
            path.write_text(RECORDER)
            path.chmod(0o755)
        # Tools a task runs by name record their calls too.
        tools = self.root / "bin"
        tools.mkdir()
        for name in ("rustup",):
            (tools / name).write_text(RECORDER)
            (tools / name).chmod(0o755)
        global_config = self.root / "global.toml"
        global_config.write_text("")
        self.checkout = checkout
        self.log = self.root / "calls.jsonl"
        # The real mise binary, not a shim, under private state that trusts only
        # this copy and installs nothing.
        found = shutil.which("mise")
        if found is None:
            self.skipTest("mise is not installed")
        self.mise = str(Path(found).resolve())
        self.environment = {
            key: value for key, value in os.environ.items() if not key.startswith("MISE_")
        } | {
            "PATH": f"{tools}{os.pathsep}{os.environ['PATH']}",
            "MISE_TASKS_LOG": str(self.log),
            "MISE_GLOBAL_CONFIG_FILE": str(global_config),
            "MISE_TRUSTED_CONFIG_PATHS": str(checkout),
            "MISE_DATA_DIR": str(self.root / "data"),
            "MISE_STATE_DIR": str(self.root / "state"),
            "MISE_CACHE_DIR": str(self.root / "cache"),
            "MISE_CONFIG_DIR": str(self.root / "config"),
            "MISE_DISABLE_TOOLS": "zig",
            "MISE_AUTO_INSTALL": "0",
            "MISE_YES": "1",
        }
        self.git("init", "-q")

    def git(self, *arguments):
        subprocess.run(["git", "-C", str(self.checkout), "-c", "user.name=Fixture",
                        "-c", "user.email=fixture@example.invalid", *arguments],
                       check=True, capture_output=True)

    def mise_run(self, *arguments, status=0, **statuses):
        environment = self.environment | {"MISE_TASKS_STATUS": str(status)} | {
            f"MISE_TASKS_STATUS_{subcommand}": str(code) for subcommand, code in statuses.items()}
        return subprocess.run([self.mise, "run", *arguments], cwd=self.checkout, env=environment,
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_tasks_are_discoverable(self):
        result = subprocess.run([self.mise, "tasks", "ls", "--json"], cwd=self.checkout, env=self.environment,
                                capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({task["name"] for task in json.loads(result.stdout)}, set(load()["tasks"]))

    def test_a_lane_defaults_to_all_targets(self):
        result = self.mise_run("check:host")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [
            ["cargo-lane", "clippy", "--locked", "--no-default-features", "--features", "host", "--all-targets"],
        ])

    def test_forwarded_arguments_replace_the_target_selection_and_keep_quoting(self):
        result = self.mise_run("test:ui", "--", "--lib", "app::chat ui", "--", "--exact")
        self.assertEqual(result.returncode, 0, result.stderr)
        lane = ["--locked", "--no-default-features", "--package", "cagents", "--package", "cagents-ui",
                "--features", "cagents/ui"]
        self.assertEqual(self.calls(), [
            ["cargo-lane", "test", *lane, "--lib", "app::chat ui", "--", "--exact"],
            # Narrowed, it then lints what its tests' build may have hidden.
            ["cargo-lane", "clippy", *lane, "--lib", "--bins", "--examples"],
        ])

    def test_a_narrowed_test_lane_checks_the_lanes_other_targets_once_its_tests_pass(self):
        # Built only under cfg(test), an item only tests use looks used; main's
        # build check, of every target, says otherwise.
        self.assertEqual(self.mise_run("test:host").returncode, 0)
        self.assertEqual(self.calls(), [
            ["cargo-lane", "test", "--locked", "--no-default-features", "--features", "host", "--all-targets"],
        ])
        self.log.unlink()
        # Failing tests are the news; the check waits for them.
        self.assertEqual(self.mise_run("test:cli", "--", "--lib", status=4).returncode, 4)
        self.assertEqual([call[1] for call in self.calls()], ["test"])
        self.log.unlink()
        self.assertEqual(self.mise_run("test:cli", "--", "--test", "cli", clippy=1).returncode, 1)
        self.assertEqual(self.calls(), [
            ["cargo-lane", "test", "--locked", "--no-default-features", "--test", "cli"],
            ["cargo-lane", "clippy", "--locked", "--no-default-features", "--lib", "--bins", "--examples"],
        ])

    def test_lane_arguments_without_the_separator_are_refused_before_anything_runs(self):
        # Requiring `--` is what keeps a later `--` for the test harness intact.
        result = self.mise_run("test:host", "--lib")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_browser_checks_all_targets_and_editor_its_one_binary(self):
        # Tests gated on the browserless lane must still compile without it.
        self.assertEqual(self.mise_run("check:browser").returncode, 0)
        self.assertEqual(self.mise_run("check:editor").returncode, 0)
        self.assertEqual(self.calls(), [
            ["cargo-lane", "clippy", "--locked", "--no-default-features", "--package", "cagents-ui",
             "--features", "browser-protocol-fixture", "--all-targets"],
            ["cargo-lane", "clippy", "--locked", "--package", "ceditor", "--bin=ceditor"],
        ])

    def test_a_failure_keeps_its_exit_status(self):
        result = self.mise_run("check:cli", status=7)
        self.assertEqual(result.returncode, 7)

    def test_the_test_aggregate_runs_each_lane_in_order_and_stops_at_a_failure(self):
        self.assertEqual(self.mise_run("test").returncode, 0)
        self.assertEqual([sorted(features(call)) for call in self.calls()], [[], ["host"], ["ui"]])
        self.log.unlink()
        self.assertEqual(self.mise_run("test", status=3).returncode, 3)
        self.assertEqual(len(self.calls()), 1)

    def test_the_check_aggregate_checks_every_lane_once(self):
        self.assertEqual(self.mise_run("check").returncode, 0)
        lanes = sorted(tuple(features(call)) for call in self.calls())
        self.assertEqual(lanes, [(), ("host",), ("ui",)])
        self.assertTrue(all(call[-1] == "--all-targets" for call in self.calls()))

    def test_runner_arguments_are_forwarded_after_the_lane(self):
        self.assertEqual(self.mise_run("dev", "--", "--debug", "--", "--ui-arg").returncode, 0)
        self.assertEqual(self.mise_run("dev:browser", "--", "--instance", "a b").returncode, 0)
        self.assertEqual(self.calls(), [
            ["run-development", "--browserless", "--debug", "--", "--ui-arg"],
            ["run-development", "--instance", "a b"],
        ])

    def test_script_contracts_run_every_job_or_the_one_named(self):
        self.assertEqual(self.mise_run("test:scripts").returncode, 0)
        self.assertEqual(self.mise_run("test:scripts", "native evidence", "2/3").returncode, 0)
        recorded = [call[1:] for call in self.calls()]
        self.assertEqual(recorded, [["verify"], ["run"], ["verify"], ["run", "native evidence", "2/3"]])

    def test_aggregates_refuse_arguments_instead_of_running_every_lane_whole(self):
        for task in ("check", "test", "ci"):
            with self.subTest(task=task):
                result = self.mise_run(task, "--", "--lib", "x")
                self.assertEqual(result.returncode, 2)
                self.assertIn("takes no arguments", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_the_ci_aggregate_runs_formatting_protocol_and_every_suite(self):
        self.git("commit", "-q", "--allow-empty", "-m", "base")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        result = self.mise_run("ci")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(call[0] for call in self.calls()), sorted([
            "check-format", "check-protocol-contract", "check-older-clients", "check-update-sources",
            "script-contracts", "script-contracts",
        ]))

    def test_setup_installs_the_toolchain_rust_toolchain_toml_names_then_runs_the_doctor(self):
        toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]
        result = self.mise_run("setup")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [
            ["rustup", "toolchain", "install", "--no-self-update", toolchain["channel"],
             "--profile", toolchain["profile"], *(f"--component={c}" for c in toolchain["components"])],
            ["doctor"],
        ])

    def test_the_protocol_check_defaults_to_the_merge_base_and_refuses_without_one(self):
        result = self.mise_run("check:protocol")
        self.assertEqual(result.returncode, 2)
        self.assertIn("pass --base REF", result.stderr)
        self.assertEqual(self.calls(), [])
        self.git("commit", "-q", "--allow-empty", "-m", "base")
        base = subprocess.run(["git", "-C", str(self.checkout), "rev-parse", "HEAD"],
                              capture_output=True, text=True, check=True).stdout.strip()
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("commit", "-q", "--allow-empty", "-m", "change")
        result = self.mise_run("check:protocol")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[0], ["check-protocol-contract", "--base-ref", base])

    def test_the_protocol_check_compares_with_the_chosen_base(self):
        result = self.mise_run("check:protocol", "--base", "abc123")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [
            ["check-protocol-contract", "--base-ref", "abc123"],
            ["check-older-clients"],
            ["check-update-sources"],
        ])


if __name__ == "__main__":
    unittest.main(verbosity=2)
