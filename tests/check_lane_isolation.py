#!/usr/bin/python3
"""Exercise scripts/check-lane-isolation on a small offline workspace of path crates."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-lane-isolation"
# Stand-ins named like the real graphs' marker packages.
MARKERS = ("libcosmic", "libghostty-vt", "servo", "rusqlite")
VARS = {
    "cli": "--no-default-features",
    "host": "--no-default-features --features host",
    "ui": "--no-default-features --package cagents --package cagents-ui --features cagents/ui",
    "browser": "--no-default-features --package cagents-ui --features browser",
    "editor": "--package editor",
    "dictate": "--no-default-features --package dictate",
}


def real_tool(name):
    """The toolchain's own executable: a version manager's shim would read the
    fixture's mise.toml as its own configuration."""
    if shutil.which("rustup"):
        found = subprocess.run(["rustup", "which", name], capture_output=True, text=True, cwd=ROOT)
        if found.returncode == 0:
            return found.stdout.strip()
    return name


CARGO = real_tool("cargo")


class LaneIsolationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="lane-isolation-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for marker in MARKERS:
            self.package(f"markers/{marker}", marker)
        self.package(".", "cagents", features={
            "host": ["dep:libghostty-vt", "dep:rusqlite"],
            "ui": ["dep:libcosmic", "dep:rusqlite"],
        }, dependencies={
            "libghostty-vt": '{ path = "markers/libghostty-vt", optional = true }',
            "libcosmic": '{ path = "markers/libcosmic", optional = true }',
            "rusqlite": '{ path = "markers/rusqlite", optional = true }',
        }, workspace=True)
        self.package("crates/cagents-ui", "cagents-ui", features={"browser": ["dep:servo"]}, dependencies={
            "cagents": '{ path = "../..", default-features = false, features = ["ui"] }',
            "servo": '{ path = "../../markers/servo", optional = true }',
        })
        self.package("crates/editor", "editor", dependencies={
            "cagents": '{ path = "../..", default-features = false, features = ["ui"] }',
        })
        self.package("crates/dictate", "dictate")
        self.write_vars(VARS)

    def package(self, directory, name, features=None, dependencies=None, workspace=False):
        path = self.root / directory
        (path / "src").mkdir(parents=True, exist_ok=True)
        (path / "src/lib.rs").write_text("")
        manifest = ""
        if workspace:
            manifest += ('[workspace]\nmembers = [".", "crates/cagents-ui", "crates/editor", "crates/dictate"]\n'
                         'resolver = "3"\n\n')
        manifest += f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n'
        if features is not None:
            manifest += "\n[features]\n" + "".join(
                f"{feature} = {list(enabled)!r}\n".replace("'", '"') for feature, enabled in features.items())
        if dependencies:
            manifest += "\n[dependencies]\n" + "".join(
                f"{dependency} = {spec}\n" for dependency, spec in dependencies.items())
        (path / "Cargo.toml").write_text(manifest)

    def write_vars(self, variables):
        (self.root / "mise.toml").write_text("[vars]\n" + "".join(
            f'{lane} = "{selection}"\n' for lane, selection in variables.items()))

    def run_check(self):
        environment = dict(os.environ, CARGO_NET_OFFLINE="true", CARGO_TARGET_DIR=str(self.root / "target"),
                           RUSTC=real_tool("rustc"))
        return subprocess.run([str(SCRIPT), "--cargo", CARGO, "--root", str(self.root)], capture_output=True, text=True,
                              env=environment, timeout=120)

    def test_lanes_that_resolve_only_their_own_graphs_pass(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("6 lanes resolve only their own graphs", result.stdout)

    def test_a_ui_lane_that_resolves_ghostty_fails_and_says_through_what(self):
        # A crate split that reunifies the host's graph with the UI's.
        self.write_vars(dict(VARS, ui=VARS["ui"] + ",cagents/host"))
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the ui lane resolves Ghostty (libghostty-vt), through:", result.stderr)
        self.assertIn("libghostty-vt v0.1.0", result.stderr)
        self.assertNotIn("the host lane", result.stderr)

    def test_a_host_lane_that_resolves_the_ui_toolkit_fails(self):
        self.write_vars(dict(VARS, host="--no-default-features --features host,ui"))
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the host lane resolves libcosmic (libcosmic)", result.stderr)

    def test_the_browserless_editor_must_not_resolve_servo(self):
        self.write_vars(dict(VARS, editor="--package editor --package cagents-ui --features cagents-ui/browser"))
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the editor lane resolves Servo (servo)", result.stderr)

    def test_a_dev_dependency_counts_because_the_lane_compiles_tests(self):
        manifest = self.root / "crates/dictate/Cargo.toml"
        manifest.write_text(manifest.read_text() + '\n[dev-dependencies]\nlibcosmic = { path = "../../markers/libcosmic" }\n')
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the dictate lane resolves libcosmic (libcosmic)", result.stderr)

    def test_the_editor_must_not_build_on_the_application(self):
        manifest = self.root / "crates/editor/Cargo.toml"
        manifest.write_text(manifest.read_text() + 'cagents-ui = { path = "../cagents-ui" }\n')
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the editor lane resolves the application (cagents-ui)", result.stderr)

    def test_a_selection_cargo_cannot_resolve_fails_with_its_error(self):
        self.write_vars(dict(VARS, dictate="--package no-such-package"))
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("the dictate lane cargo tree failed", result.stderr)
        self.assertIn("no-such-package", result.stderr)

    def test_a_lane_missing_from_mise_is_refused(self):
        self.write_vars({lane: selection for lane, selection in VARS.items() if lane != "dictate"})
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("mise.toml has no lane dictate", result.stderr)


if __name__ == "__main__":
    unittest.main()
