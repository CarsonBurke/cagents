#!/usr/bin/python3
"""HOST helper preparation contracts using fake Cargo; no compiler is launched."""
import json
import importlib.machinery
import importlib.util
import fcntl
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
HELPER = "CAGENTS_TEST_RETAINED_ACTOR_HELPER"


class HostTestHelper(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-host-helper-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "Cargo.toml").write_text("[package]\nname='cagents'\nversion='0.1.3'\n")
        self.product = self.root / "reported target with spaces" / "cagentsd"
        self.product.parent.mkdir()
        self.product.write_text("#!/bin/sh\nexit 97\n")
        self.product.chmod(0o700)
        self.events = self.root / "events.jsonl"
        self.cargo = self.root / "cargo"
        self.cargo.write_text("#!/usr/bin/python3\n" + '''
import json, os, sys
from pathlib import Path
root = Path(os.environ["FIXTURE_ROOT"])
arguments = sys.argv[1:]
if arguments and arguments[0].startswith("+"):
    arguments = arguments[1:]
with open(root / "events.jsonl", "a") as output:
    output.write(json.dumps({"args": sys.argv[1:], "helper": os.environ.get(
        "CAGENTS_TEST_RETAINED_ACTOR_HELPER")}) + "\\n")
if arguments[0] == "metadata":
    print(json.dumps({"workspace_root": str(root), "workspace_default_members": ["cagents-id"],
        "packages": [{"name": "cagents", "version": "0.1.3", "id": "cagents-id",
            "manifest_path": str(root / "Cargo.toml"), "targets": [{"name": "cagentsd",
            "kind": ["bin"]}], "features": {"default": ["full"], "full": [],
            "host": [], "test-alias": ["host"]}}]}))
elif arguments[0] == "build":
    mode = os.environ.get("FIXTURE_MODE")
    if mode == "blocking-build":
        import time
        (root / "build-pid").write_text(str(os.getpid()))
        time.sleep(30)
    if mode == "failure":
        print("fixture helper build failed", file=sys.stderr)
        sys.exit(101)
    if mode != "missing-artifact":
        print(json.dumps({"reason": "compiler-artifact", "target": {"name": "cagentsd",
            "kind": ["bin"]}, "executable": os.environ["FIXTURE_PRODUCT"]}))
elif arguments[0] in ("test", "t"):
    if os.environ.get("CAGENTS_TEST_RETAINED_ACTOR_HELPER") != os.environ["FIXTURE_PRODUCT"]:
        print("test did not receive exact helper", file=sys.stderr)
        sys.exit(78)
    sys.exit(int(os.environ.get("FIXTURE_TEST_STATUS", "0")))
''')
        self.cargo.chmod(0o700)
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith("CAGENTS_BUILD_") and name != HELPER}
        self.env.update(FIXTURE_ROOT=str(self.root), FIXTURE_PRODUCT=str(self.product), TMPDIR=str(self.root),
                        CAGENTS_BUILD_REMOTE="local", CAGENTS_BUILD_INSTALLED_GATE="",
                        CAGENTS_BUILD_INSTALL_SYNC="0", CAGENTS_BUILD_LOCK=str(self.root / "lock"),
                        CAGENTS_BUILD_CPU_PRESSURE="0", CAGENTS_BUILD_MEMORY_PRESSURE="0",
                        CAGENTS_BUILD_CPU_SETTLE_SECS="0", CAGENTS_BUILD_REGISTRY="",
                        CAGENTS_BUILD_FREE_FLOOR_GIB="0", CAGENTS_BUILD_SEED="0",
                        CAGENTS_BUILD_CARGO=str(self.cargo), CAGENTS_MEMORY_MAX="0")

    def invoke(self, arguments, *, run=False, gate=False, extra=None):
        if gate:
            command = [str(ROOT / "scripts/cargo-gate"), *arguments]
        else:
            command = [sys.executable, "-I", str(ROOT / "scripts/cargo-host-test-helper.py"),
                       *(["--run"] if run else []), str(self.cargo), *arguments]
        result = subprocess.run(command, cwd=self.root, env=self.env | (extra or {}),
                                capture_output=True, text=True, timeout=30)
        events = [json.loads(line) for line in self.events.read_text().splitlines()] \
            if self.events.exists() else []
        return result, events

    def test_plain_host_test_builds_exact_helper_then_runs_original_command(self):
        arguments = ["test", "--locked", "--no-default-features", "--features", "host",
                     "--all-targets", "filter", "--", "--test-threads=1"]
        result, events = self.invoke(arguments, run=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([event["args"][0] for event in events], ["metadata", "build", "test"])
        self.assertEqual(events[1]["args"], ["build", "--locked", "--no-default-features",
            "--features", "host", "--profile", "test", "--package", "cagents-id", "--bin",
            "cagentsd", "--message-format=json-render-diagnostics"])
        self.assertEqual(events[2], {"args": arguments, "helper": str(self.product)})
        self.assertEqual(result.stdout, "")

    def test_profile_target_config_and_attached_options_are_preserved(self):
        arguments = ["+stable", "test", "--lib", "--profile=dev-runtime", "--target-dir", "elsewhere",
                     "--target=x86_64-unknown-linux-gnu", "--config", "build.jobs=2",
                     "-Fhost", "-pcagents", "--locked", "filter"]
        result, events = self.invoke(arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, str(self.product) + "\n")
        self.assertEqual(events[1]["args"], ["+stable", "build", "--profile=dev-runtime",
            "--target-dir", "elsewhere", "--target=x86_64-unknown-linux-gnu", "--config",
            "build.jobs=2", "-Fhost", "--locked", "--package", "cagents-id", "--bin",
            "cagentsd", "--message-format=json-render-diagnostics"])

    def test_transitive_host_feature_is_detected(self):
        result, events = self.invoke(["test", "--features=test-alias"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, str(self.product) + "\n")
        self.assertEqual(events[-1]["args"][0], "build")

    def test_unrelated_lanes_packages_and_compile_only_commands_do_not_build_helper(self):
        for arguments in (["test"], ["test", "--features=ui"], ["check", "--features=host"],
                          ["test", "--features=host", "--no-run"],
                          ["test", "--features=host", "--doc"],
                          ["test", "--features=host", "--package=ceditor"],
                          ["test", "--all-features", "--workspace", "--exclude=cagents"]):
            with self.subTest(arguments=arguments):
                self.events.unlink(missing_ok=True)
                result, events = self.invoke(arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertTrue(all(event["args"][0] == "metadata" for event in events))

    def test_explicit_helper_is_preserved_without_building(self):
        result, events = self.invoke(["test", "--features=host"], run=True,
                                     extra={HELPER: str(self.product)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events, [{"args": ["test", "--features=host"], "helper": str(self.product)}])

    def test_failed_or_missing_helper_stops_before_test_execution(self):
        for mode, status, diagnostic in (("failure", 101, "fixture helper build failed"),
                                        ("missing-artifact", 1, "exactly one cagentsd executable"),
                                        ("directory", 1, "absolute executable file")):
            with self.subTest(mode=mode):
                self.events.unlink(missing_ok=True)
                extra = {"FIXTURE_MODE": mode}
                if mode == "directory":
                    extra["FIXTURE_PRODUCT"] = str(self.root)
                result, events = self.invoke(["test", "--features=host"], run=True, extra=extra)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertIn(diagnostic, result.stderr)
                self.assertEqual([event["args"][0] for event in events], ["metadata", "build"])

    def test_test_failure_status_is_preserved(self):
        result, _ = self.invoke(["test", "--features=host"], run=True,
                                extra={"FIXTURE_TEST_STATUS": "103"})
        self.assertEqual(result.returncode, 103, result.stderr)

    def test_actual_gate_passes_helper_to_test_after_preparation(self):
        result, events = self.invoke(["test", "--features=host", "--lib"], gate=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [event for event in events if event["args"][0] == "build"]
        tests = [event for event in events if event["args"][0] == "test"]
        self.assertEqual(len(builds), 1)
        self.assertEqual(len(tests), 2)  # Gate's compile half, then execution.
        self.assertTrue(all(event["helper"] == str(self.product) for event in tests))

    def test_cancelling_original_gate_reaps_helper_build_before_releasing_slot(self):
        build_pid = self.root / "build-pid"
        child = subprocess.Popen([str(ROOT / "scripts/cargo-gate"), "test", "--features=host",
                                  "--lib"], cwd=self.root,
                                 env=self.env | {"FIXTURE_MODE": "blocking-build"},
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
        compiler = None
        try:
            deadline = time.monotonic() + 10
            while not build_pid.exists() or not build_pid.read_text():
                self.assertIsNone(child.poll(), "gate exited before helper compilation")
                self.assertLess(time.monotonic(), deadline, "helper build did not start")
                time.sleep(0.02)
            compiler = int(build_pid.read_text())
            child.send_signal(signal.SIGTERM)
            self.assertEqual(child.wait(timeout=15), 143)
            self.assertFalse(Path(f"/proc/{compiler}").exists(), "helper compiler outlived its gate")
            with open(self.root / "lock", "a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            events = [json.loads(line) for line in self.events.read_text().splitlines()]
            self.assertFalse(any(event["args"][0] == "test" for event in events))
            self.assertEqual(list(self.root.glob("cagents-test-helper.*")), [])
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            if compiler is not None:
                try:
                    os.kill(compiler, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_actual_offload_agent_prepares_helper_inside_its_slot(self):
        loader = importlib.machinery.SourceFileLoader("offload", str(ROOT / "scripts/cargo-offload"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        offload = importlib.util.module_from_spec(spec)
        loader.exec_module(offload)
        slot = self.root / "slot-0"
        slot.mkdir()
        (slot / "src").symlink_to(self.root, target_is_directory=True)
        (slot / "claim").write_text(json.dumps({"cost": 1, "pid": None, "build": "", "key": ""}))
        temporary = self.root / "temporary"
        temporary.mkdir(mode=0o700)
        environment = self.env | {"CAGENTS_OFFLOAD_TEMPORARY_ROOT": str(temporary)}
        child = subprocess.Popen([sys.executable, "-I", "-c", offload.REMOTE_AGENT,
                                  "run", str(slot), ".", str(self.cargo), "test", "--features=host"],
                                 cwd=self.root, env=environment, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            child.stdin.write((json.dumps({"environment": {}, "host_test_helper":
                (ROOT / "scripts/cargo-host-test-helper.py").read_text(), "memory_cap":
                (ROOT / "scripts/memory-capped").read_text()}) + "\n").encode())
            child.stdin.flush()
            # The agent cancels when its connection's stdin closes. Keep that
            # pipe open until the fake Cargo finishes, as a real offload does.
            status = child.wait(timeout=30)
            output = child.stdout.read().decode()
            error = child.stderr.read().decode()
            self.assertEqual(status, 0, error)
            events = [json.loads(line) for line in self.events.read_text().splitlines()]
            self.assertEqual([event["args"][0] for event in events], ["metadata", "build", "test"])
            self.assertEqual(events[-1]["helper"], str(self.product))
            self.assertEqual(output, "")
        finally:
            child.stdin.close()
            if child.poll() is None:
                child.kill()
                child.wait()
            child.stdout.close()
            child.stderr.close()


if __name__ == "__main__":
    unittest.main()
