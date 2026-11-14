#!/usr/bin/env python3
"""The mise task interface: lanes stay feature-disjoint and arguments forward.

The structural cases read mise.toml. The behavioral cases run the real mise
against a copy of it whose scripts record how they were called, so no task
builds, tests or runs anything; they are skipped when mise is not installed.
"""

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
LANES = {"cli": set(), "host": {"host"}, "ui": {"ui"}, "browser": {"integrated-browser"}, "editor": set()}


def load():
    return tomllib.loads(CONFIG.read_text())


def commands(task):
    run = task.get("run", [])
    return [run] if isinstance(run, str) else [entry for entry in run if isinstance(entry, str)]


def features(arguments):
    found = set()
    for flag, value in zip(arguments, arguments[1:]):
        if flag == "--features":
            found.update(value.split(","))
    return found


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
                if lane in {"cli", "host", "ui"}:
                    self.assertIn("--no-default-features", arguments)

    def test_no_task_reunifies_host_and_ui(self):
        variables = self.config["vars"]
        for name, task in self.tasks.items():
            for command in commands(task):
                expanded = re.sub(r"\{\{\s*vars\.(\w+)\s*\}\}", lambda m: variables[m.group(1)], command)
                with self.subTest(task=name):
                    self.assertLessEqual(len(re.findall(r"\{\{\s*vars\.(?:cli|host|ui|browser|editor)\b", command)), 1)
                    selected = features(expanded.split())
                    self.assertFalse({"host", "ui"} <= selected, command)
                    self.assertFalse({"host", "integrated-browser"} <= selected, command)

    def test_every_compiling_task_goes_through_the_gate(self):
        for name, task in self.tasks.items():
            for command in commands(task):
                with self.subTest(task=name):
                    self.assertIsNone(re.search(r"(^|[;&|]\s*|\bexec\s+)cargo\s+(build|check|test|run)\b", command))

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
sys.exit(int(os.environ.get("MISE_TASKS_STATUS", "0")))
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
        self.mise = str(Path(shutil.which("mise")).resolve())
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

    def mise_run(self, *arguments, status=0):
        environment = self.environment | {"MISE_TASKS_STATUS": str(status)}
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
            ["cargo-gate", "check", "--locked", "--no-default-features", "--features", "host", "--all-targets"],
        ])

    def test_forwarded_arguments_replace_the_target_selection_and_keep_quoting(self):
        result = self.mise_run("test:ui", "--", "--lib", "app::chat ui", "--", "--exact")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [
            ["cargo-gate", "test", "--locked", "--no-default-features", "--features", "ui",
             "--lib", "app::chat ui", "--", "--exact"],
        ])

    def test_lane_arguments_without_the_separator_are_refused_before_anything_runs(self):
        # Requiring `--` is what keeps a later `--` for the test harness intact.
        result = self.mise_run("test:host", "--lib")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_browser_and_editor_default_to_their_one_binary(self):
        self.assertEqual(self.mise_run("check:browser").returncode, 0)
        self.assertEqual(self.mise_run("check:editor").returncode, 0)
        self.assertEqual(self.calls(), [
            ["cargo-gate", "check", "--locked", "--features", "integrated-browser", "--bin=cagents-ui"],
            ["cargo-gate", "check", "--locked", "--package", "ceditor", "--bin=ceditor"],
        ])

    def test_a_failure_keeps_its_exit_status(self):
        result = self.mise_run("check:cli", status=7)
        self.assertEqual(result.returncode, 7)

    def test_the_test_aggregate_runs_each_lane_in_order_and_stops_at_a_failure(self):
        self.assertEqual(self.mise_run("test").returncode, 0)
        self.assertEqual([call[5] if call[4] == "--features" else "cli" for call in self.calls()],
                         ["cli", "host", "ui"])
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
