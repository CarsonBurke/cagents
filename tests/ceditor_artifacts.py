#!/usr/bin/env python3
"""Run ceditor scripts with fixture Cargo artifacts; never compile or launch a UI."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CeditorArtifacts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="ceditor-artifacts-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.checkout = self.directory / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        (self.checkout / "packaging").mkdir()
        for name in ("run-ceditor-development", "install-ceditor", "build-ceditor-product"):
            source = ROOT / "scripts" / name
            if source.exists():
                shutil.copy2(source, self.checkout / "scripts" / name)
        for name in ("io.github.cagents.ceditor.svg", "io.github.cagents.ceditor.desktop"):
            shutil.copy2(ROOT / "packaging" / name, self.checkout / "packaging" / name)
        (self.checkout / ".cargo").mkdir()
        (self.checkout / ".cargo/config.toml").write_text(
            '[build]\ntarget = "x86_64-unknown-linux-gnu"\n'
        )
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.home = self.directory / "home"
        self.home.mkdir()
        # JSON escaping and shell quoting both matter for these artifact paths.
        self.target = self.directory / 'target with spaces "quote" \\backslash'
        self.prefix = self.directory / "installation"
        self.calls = self.directory / "cargo-calls.jsonl"
        self.launch = self.directory / "launched.json"
        self.environment = {
            "PATH": str(self.tools) + os.pathsep + os.defpath,
            "HOME": str(self.home),
            "XDG_DATA_HOME": str(self.home / "data"),
            "MOCK_CHECKOUT": str(self.checkout),
            "MOCK_TARGET": str(self.target),
            "MOCK_CALLS": str(self.calls),
            "MOCK_LAUNCH": str(self.launch),
            "MOCK_GENERATION": "first",
        }
        self.executable(self.tools / "cargo", f"#!{sys.executable}\n" + r'''
import json
import os
from pathlib import Path
import sys
import tomllib

args = sys.argv[1:]
with open(os.environ["MOCK_CALLS"], "a") as output:
    output.write(json.dumps(args) + "\n")
target = Path(os.environ["MOCK_TARGET"])
if args[0] == "metadata":
    print(json.dumps({"target_directory": str(target)}, separators=(",", ":")))
    sys.exit(0)
assert args[0] == "build", args
name = args[args.index("--bin") + 1] if "--bin" in args else "ceditor"
profile = "release" if "--release" in args else "dev-runtime"
configuration = tomllib.loads((Path(os.environ["MOCK_CHECKOUT"]) / ".cargo/config.toml").read_text())
triple = os.environ.get("CARGO_BUILD_TARGET", configuration["build"]["target"])
product = target / triple / profile / name
product.parent.mkdir(parents=True, exist_ok=True)
generation = os.environ["MOCK_GENERATION"]
product.write_text(
    "#!" + sys.executable + "\n"
    "import json, os, sys\n"
    "with open(os.environ['MOCK_LAUNCH'], 'w') as output:\n"
    "    json.dump({'executable': sys.argv[0], 'args': sys.argv[1:], "
    "'cwd': os.getcwd(), 'generation': " + repr(generation) + "}, output)\n"
)
product.chmod(0o755)
mode = os.environ.get("MOCK_ARTIFACT_MODE") if name == "ceditor" else None
print(json.dumps({"reason": "compiler-message", "message": {"rendered": "fixture compiler warning\n"}}))
print(json.dumps({"reason": "compiler-artifact", "target": {"name": name, "kind": ["lib"]}, "executable": None}))
if mode != "missing":
    record = {"reason": "compiler-artifact", "target": {"name": name, "kind": ["bin"]},
              "executable": str(product), "fresh": True}
    if mode == "relative":
        record["executable"] = "relative/ceditor"
    elif mode == "absent":
        record["executable"] = str(product.with_name("absent"))
    elif mode == "not-executable":
        product.chmod(0o644)
    print(json.dumps(record))
    if mode == "ambiguous":
        other = dict(record, executable=str(product.with_name("other-ceditor")))
        print(json.dumps(other))
print(json.dumps({"reason": "build-finished", "success": mode != "failed"}))
sys.exit(101 if mode == "failed" else 0)
''')
        # Desktop-cache updates remain fixture-only and never reach the desktop.
        for name in ("gtk-update-icon-cache", "update-desktop-database"):
            self.executable(self.tools / name, "#!/bin/sh\nexit 0\n")

    @staticmethod
    def executable(path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        path.chmod(0o755)

    def run_script(self, name, *arguments):
        return subprocess.run(
            ["/bin/sh", str(self.checkout / "scripts" / name), *arguments],
            cwd=self.directory, env=self.environment, text=True, capture_output=True, timeout=10,
        )

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)

    def cargo_calls(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_runner_uses_configured_target_artifact_and_preserves_arguments_and_cwd(self):
        self.target = self.directory / "plain-target"
        self.environment["MOCK_TARGET"] = str(self.target)
        result = self.run_script("run-ceditor-development", "--", "a file.rs", "--help")
        self.assert_success(result)
        launched = json.loads(self.launch.read_text())
        self.assertEqual(launched["executable"], str(self.target / "x86_64-unknown-linux-gnu/dev-runtime/ceditor"))
        self.assertEqual(launched["args"], ["a file.rs", "--help"])
        self.assertEqual(launched["cwd"], str(self.directory))
        self.assertEqual(len(self.cargo_calls()), 1)
        self.assertIn("fixture compiler warning", result.stderr)

    def test_runner_does_not_launch_stale_unqualified_artifact(self):
        stale = self.target / "dev-runtime/ceditor"
        self.executable(stale, "#!/bin/sh\nexit 97\n")
        self.assert_success(self.run_script("run-ceditor-development"))
        self.assertNotEqual(json.loads(self.launch.read_text())["executable"], str(stale))

    def test_review_alerts_keep_companion_build_lanes_separate(self):
        self.assert_success(self.run_script("run-ceditor-development", "--review-alerts", "--", "file.rs"))
        calls = self.cargo_calls()
        self.assertEqual(len(calls), 3)
        self.assertIn("development", calls[0])
        self.assertIn("development,host", calls[1])
        self.assertIn("--package", calls[2])
        self.assertIn("ceditor", calls[2])
        self.assertTrue(all("--locked" in call for call in calls))
        self.assertFalse(any("development,ui,host" in call for call in calls))

    def test_environment_target_is_honored(self):
        self.environment["CARGO_BUILD_TARGET"] = "custom-target"
        self.assert_success(self.run_script("run-ceditor-development"))
        self.assertIn("/custom-target/dev-runtime/", json.loads(self.launch.read_text())["executable"])

    def test_missing_python_fails_before_any_build_or_desktop_change(self):
        minimal = self.directory / "minimal-tools"
        minimal.mkdir()
        (minimal / "dirname").symlink_to(shutil.which("dirname"))
        self.environment["PATH"] = str(minimal)
        for name, arguments in (("run-ceditor-development", ()),
                                ("install-ceditor", ("--prefix", str(self.prefix)))):
            with self.subTest(script=name):
                result = self.run_script(name, *arguments)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("required command not found: python3", result.stderr)
                self.assertFalse(self.calls.exists())
                self.assertFalse(self.prefix.exists())
                self.assertFalse((self.home / "data").exists())
                self.assert_success(self.run_script(name, "--help"))

    def test_install_uses_all_exact_artifacts_and_keeps_previous_private_set(self):
        for name in ("ceditor", "cagents", "cagentsd"):
            self.executable(self.target / "release" / name, "stale unqualified artifact\n")
        public_cli = self.prefix / "bin/cagents"
        self.executable(public_cli, "unrelated installed CLI\n")
        self.assert_success(self.run_script("install-ceditor", "--prefix", str(self.prefix)))
        installed = self.prefix / "bin/ceditor"
        original = installed.resolve()
        for name in ("ceditor", "cagents", "cagentsd"):
            self.assertIn("'first'", original.with_name(name).read_text())
        self.environment["MOCK_GENERATION"] = "second"
        self.assert_success(self.run_script("install-ceditor", "--prefix", str(self.prefix)))
        self.assertNotEqual(installed.resolve(), original)
        self.assertTrue(original.exists())
        self.assertIn("'second'", installed.read_text())
        self.assertEqual(public_cli.read_text(), "unrelated installed CLI\n")
        self.assertEqual(len(self.cargo_calls()), 6)

    def test_failed_missing_or_ambiguous_artifact_never_launches_or_replaces_installation(self):
        self.assert_success(self.run_script("install-ceditor", "--prefix", str(self.prefix)))
        installed = self.prefix / "bin/ceditor"
        original = installed.resolve()
        releases = sorted((self.prefix / "lib/ceditor/releases").iterdir())
        for mode in ("failed", "missing", "ambiguous", "relative", "absent", "not-executable"):
            with self.subTest(mode=mode):
                self.environment["MOCK_ARTIFACT_MODE"] = mode
                runner = self.run_script("run-ceditor-development")
                self.assertNotEqual(runner.returncode, 0, runner.stderr)
                self.assertFalse(self.launch.exists())
                install = self.run_script("install-ceditor", "--prefix", str(self.prefix))
                self.assertNotEqual(install.returncode, 0, install.stderr)
                self.assertEqual(installed.resolve(), original)
                self.assertEqual(sorted((self.prefix / "lib/ceditor/releases").iterdir()), releases)


if __name__ == "__main__":
    unittest.main()
