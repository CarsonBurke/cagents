#!/usr/bin/env python3
"""Exercise the release lane commands with mocked Cargo and the real artifact parser."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
HELPER_ENV = "CAGENTS_TEST_RETAINED_ACTOR_HELPER"


class ReleaseHostHelper(unittest.TestCase):
    def run_lanes(self, mode):
        # Run the actual command block, without provisioning a rootfs or
        # performing the later archive/metadata validation. Every Cargo and
        # packager command is a private executable fixture.
        source = (ROOT / "packaging/release-build").read_text()
        start = source.index("# Each command resolves its own feature graph;")
        end = source.index("\nmetric() {", start)
        with tempfile.TemporaryDirectory(prefix="cagents-release-helper-") as temporary:
            fixture = Path(temporary)
            (fixture / "scripts").mkdir()
            (fixture / "tools").mkdir()
            shutil.copyfile(ROOT / "scripts/build-ceditor-product",
                            fixture / "scripts/build-ceditor-product")
            helper = fixture / "reported target with spaces" / "cagentsd"
            helper.parent.mkdir()
            helper.write_text("#!/bin/sh\nexit 97\n")
            helper.chmod(0o700)
            events = fixture / "events.jsonl"
            cargo = fixture / "tools/cargo"
            cargo.write_text(f"#!{sys.executable}\n" + '''
import json, os, sys
arguments = sys.argv[1:]
with open(os.environ["FIXTURE_EVENTS"], "a") as output:
    output.write(json.dumps({"kind": "cargo", "args": arguments,
                            "helper": os.environ.get("CAGENTS_TEST_RETAINED_ACTOR_HELPER")}) + "\\n")
if arguments[0] == "build":
    mode = os.environ["FIXTURE_MODE"]
    if mode == "build-failure":
        print("fixture helper build failed", file=sys.stderr)
        sys.exit(101)
    if mode != "missing-artifact":
        print(json.dumps({"reason": "compiler-artifact", "target": {
            "name": "cagentsd", "kind": ["bin"]},
            "executable": os.environ["FIXTURE_HELPER"]}))
elif "--features" in arguments and arguments[arguments.index("--features") + 1] == "host":
    if os.environ.get("CAGENTS_TEST_RETAINED_ACTOR_HELPER") != os.environ["FIXTURE_HELPER"]:
        print("HOST tests did not receive the matched helper", file=sys.stderr)
        sys.exit(78)
''')
            cargo.chmod(0o700)
            for name in ("package-linux.sh", "package-ceditor-linux.sh"):
                package = fixture / "scripts" / name
                package.write_text(f"#!{sys.executable}\n" + '''
import json, os, sys
with open(os.environ["FIXTURE_EVENTS"], "a") as output:
    output.write(json.dumps({"kind": "package", "args": sys.argv[1:],
                            "helper": os.environ.get("CAGENTS_TEST_RETAINED_ACTOR_HELPER")}) + "\\n")
''')
                package.chmod(0o700)
            env = os.environ.copy()
            env.pop(HELPER_ENV, None)
            env.update(PATH=str(fixture / "tools") + os.pathsep + os.defpath,
                       FIXTURE_MODE=mode, FIXTURE_EVENTS=str(events),
                       FIXTURE_HELPER=str(helper))
            result = subprocess.run(
                ["/bin/bash", "-euo", "pipefail", "-c", source[start:end]],
                cwd=fixture, env=env, capture_output=True, text=True, timeout=10,
            )
            recorded = [json.loads(line) for line in events.read_text().splitlines()]
            return result, recorded, str(helper)

    def test_exact_helper_path_is_scoped_to_host_tests(self):
        result, events, helper = self.run_lanes("success")
        self.assertEqual(result.returncode, 0, result.stderr)
        cargo = [event for event in events if event["kind"] == "cargo"]
        self.assertEqual([event["args"][0] for event in cargo],
                         ["test", "build", "test", "test", "test", "test"])
        self.assertEqual(cargo[1]["args"], [
            "build", "--locked", "--release", "--no-default-features",
            "--features", "host", "--bin", "cagentsd",
            "--message-format=json-render-diagnostics",
        ])
        # Counts alone allow a missing feature to silently repeat the CLI
        # graph. Keep every independently qualified release boundary explicit.
        for lane, index, arguments in (
            ("CLI", 0, [
                "test", "--locked", "--release", "--no-default-features",
                "--all-targets",
            ]),
            ("HOST", 2, [
                "test", "--locked", "--release", "--no-default-features",
                "--features", "host", "--all-targets",
            ]),
            ("browserless UI", 3, [
                "test", "--locked", "--release", "--no-default-features",
                "--features", "ui", "--all-targets",
            ]),
            ("ceditor", 4, [
                "test", "--locked", "--release", "--package", "ceditor",
                "--all-targets",
            ]),
            ("integrated browser", 5, [
                "test", "--locked", "--release", "--features",
                "integrated-browser", "--all-targets",
            ]),
        ):
            with self.subTest(lane=lane):
                self.assertEqual(cargo[index]["args"], arguments)
        self.assertEqual(cargo[2]["helper"], helper)
        self.assertTrue(all(event["helper"] is None
                            for index, event in enumerate(cargo) if index != 2))
        packages = [event for event in events if event["kind"] == "package"]
        self.assertEqual(len(packages), 3)
        self.assertTrue(all(event["helper"] is None for event in packages))

    def test_failed_helper_build_stops_before_host_tests(self):
        result, events, _ = self.run_lanes("build-failure")
        self.assertEqual(result.returncode, 101, result.stderr)
        self.assertEqual([event["args"][0] for event in events], ["test", "build"])
        self.assertIn("fixture helper build failed", result.stderr)

    def test_success_without_helper_artifact_stops_before_host_tests(self):
        result, events, _ = self.run_lanes("missing-artifact")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([event["args"][0] for event in events], ["test", "build"])
        self.assertIn("exactly one executable for cagentsd (found 0)", result.stderr)


if __name__ == "__main__":
    unittest.main()
