#!/usr/bin/env python3
"""Exercise the real redraw shell runner with fake builds and no app launches."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class RedrawArtifacts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="redraw-artifacts-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.calls = self.root / "calls.jsonl"
        for name in ("run-redraw-latency", "build-ceditor-product"):
            self.write("scripts/" + name, (ROOT / "scripts" / name).read_text())
        self.write("fixtures/redraw-scheduling.schema.json", "{}\n")
        self.write(".cargo/config.toml", '[build]\ntarget = "x86_64-unknown-linux-gnu"\n')
        self.target = self.root / "configured target"
        self.products = self.target / "x86_64-unknown-linux-gnu/release"
        # These executable sentinels must never be launched, including cleanup.
        for name in ("cagents", "cagentsd", "cagents-ui"):
            self.write(f"configured target/release/{name}", "#!/bin/sh\nexit 88\n", executable=True)
        self.tool("cargo", """
if sys.argv[1] == 'metadata':
    print(json.dumps({'target_directory': str(root / 'configured target')}))
    sys.exit(0)
assert sys.argv[1] == 'build', sys.argv
name = sys.argv[sys.argv.index('--bin') + 1]
if os.environ.get('FAIL_BUILD') == name:
    print('mock build diagnostic', file=sys.stderr)
    sys.exit(101)
product = root / 'configured target/x86_64-unknown-linux-gnu/release' / name
product.parent.mkdir(parents=True, exist_ok=True)
product.write_text('#!' + sys.executable + '\\n' +
    'import json,os,pathlib,sys\\n' +
    'with pathlib.Path(os.environ["MOCK_CALLS"]).open("a") as f: f.write(json.dumps(["product",sys.argv])+"\\\\n")\\n')
product.chmod(0o755)
message = {'reason': 'compiler-artifact', 'target': {'name': name, 'kind': ['bin']},
           'executable': str(product)}
if os.environ.get('MISSING_ARTIFACT') != name:
    print(json.dumps(message))
if os.environ.get('AMBIGUOUS_ARTIFACT') == name:
    print(json.dumps(message))
""")
        self.tool("git", """
if sys.argv[1] == 'rev-parse': print('fixture-revision')
""")
        # Record the exact UI argument, but never execute the product.
        self.tool("timeout", """
assert sys.argv[1:5] == ['--signal=TERM', '--kill-after=5s', '120s',
    str(root / 'configured target/x86_64-unknown-linux-gnu/release/cagents-ui')], sys.argv
Path(os.environ['CAGENTS_LATENCY_OUTPUT']).write_text('{}\\n')
""")
        # jq is outside the artifact-routing contract; no performance acceptance
        # is inferred from these synthetic workload files.
        self.tool("jq", """
if sys.argv[1:3] == ['-er', '.target_directory']:
    print(json.load(sys.stdin)['target_directory'])
elif sys.argv[1] != '-e': print('{}')
""")
        self.environment = os.environ.copy()
        self.environment.pop("CARGO_BUILD_TARGET", None)
        self.environment.update(
            PATH=str(self.root / "tools") + os.pathsep + os.environ["PATH"],
            MOCK_ROOT=str(self.root), MOCK_CALLS=str(self.calls),
            XDG_RUNTIME_DIR=str(self.root / "desktop-runtime"),
            HOME=str(self.root / "home"),
        )

    def write(self, relative, text, executable=False):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        if executable:
            path.chmod(0o755)

    def tool(self, name, body):
        self.write("tools/" + name, f"#!{sys.executable}\n" + """
import json,os,sys
from pathlib import Path
root = Path(os.environ['MOCK_ROOT'])
with Path(os.environ['MOCK_CALLS']).open('a') as output:
    output.write(json.dumps([Path(sys.argv[0]).name, sys.argv[1:]]) + '\\n')
""" + body, executable=True)

    def run_runner(self, **overrides):
        return subprocess.run(
            ["sh", str(self.root / "scripts/run-redraw-latency"), str(self.root / "output")],
            cwd=self.root.parent, env=self.environment | overrides,
            capture_output=True, text=True, timeout=20,
        )

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def builds(self):
        return [args for tool, args in self.recorded() if tool == "cargo" and args[0] == "build"]

    def test_config_target_paths_and_matched_isolated_lanes(self):
        result = self.run_runner()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        expected = []
        for name, features in (("cagents", "development"), ("cagentsd", "development,host"),
                               ("cagents-ui", "development,latency-harness")):
            expected.append(["build", "--locked", "--release", "--no-default-features",
                             "--features", features, "--bin", name,
                             "--message-format=json-render-diagnostics"])
        self.assertEqual(self.builds(), expected)
        calls = self.recorded()
        self.assertFalse(any(tool == "cargo" and args[0] == "metadata" for tool, args in calls))
        self.assertEqual(sum(tool == "timeout" for tool, _ in calls), 5)
        cleanups = [args for tool, args in calls if tool == "product"]
        self.assertEqual(cleanups, [[str(self.products / "cagents"), "daemon", "stop-for-rebuild",
                                     "--including-subagents"]] * 5)

    def test_each_build_failure_preserves_status_and_stops_before_launch(self):
        for index, name in enumerate(("cagents", "cagentsd", "cagents-ui"), 1):
            with self.subTest(name=name):
                self.calls.unlink(missing_ok=True)
                shutil.rmtree(self.root / "output", ignore_errors=True)
                result = self.run_runner(FAIL_BUILD=name)
                self.assertEqual(result.returncode, 101, result.stdout + result.stderr)
                self.assertIn("mock build diagnostic", result.stderr)
                self.assertEqual(len(self.builds()), index)
                self.assertFalse(any(tool in ("timeout", "product") for tool, _ in self.recorded()))

    def test_missing_or_ambiguous_artifact_rejects_before_launch(self):
        for variable in ("MISSING_ARTIFACT", "AMBIGUOUS_ARTIFACT"):
            for index, name in enumerate(("cagents", "cagentsd", "cagents-ui"), 1):
                with self.subTest(variable=variable, name=name):
                    self.calls.unlink(missing_ok=True)
                    shutil.rmtree(self.root / "output", ignore_errors=True)
                    result = self.run_runner(**{variable: name})
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("exactly one executable", result.stderr)
                    self.assertEqual(len(self.builds()), index)
                    self.assertFalse(any(tool in ("timeout", "product") for tool, _ in self.recorded()))


if __name__ == "__main__":
    unittest.main()
