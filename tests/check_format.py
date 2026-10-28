#!/usr/bin/env python3
"""Check routing and rustfmt's include! gap without Cargo/application builds.

CHECK_FORMAT_CARGO_FIXTURE=1 also exercises Cargo in a dependency-free fixture.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts/check-format"
INCLUDED = (
    "src/modules.rs",
    "src/terminal/worker/bounded_input_tests.rs",
    "src/host/model/views/placement_tests.rs",
    "tests/support/iced_tree.rs",
    "tests/support/repository_scale.rs",
)


class FormatCheck(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-format-check-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write("scripts/check-format", CHECKER.read_text())
        self.script = self.root / "scripts/check-format"
        self.script.chmod(0o755)

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run_checker(self, env=None):
        # Calling from outside the fixture verifies checkout-relative resolution.
        return subprocess.run(
            [str(self.script)], cwd=self.root.parent, env=env,
            capture_output=True, text=True, timeout=30,
        )

    def mock_tools(self, cargo_status=0, rustfmt_status=0):
        log = self.root / "calls.jsonl"
        for tool, status in (("cargo", cargo_status), ("rustfmt", rustfmt_status)):
            self.write(f"tools/{tool}", f"""#!{shutil.which('python3')}
import json
from pathlib import Path
import sys
with Path({str(log)!r}).open("a") as output:
    output.write(json.dumps([Path.cwd().as_posix(), Path(sys.argv[0]).name, *sys.argv[1:]]) + "\\n")
print({tool + ' diagnostic'!r}, file=sys.stderr)
sys.exit({status})
""")
            (self.root / "tools" / tool).chmod(0o755)
        env = os.environ | {"PATH": str(self.root / "tools") + os.pathsep + os.environ["PATH"]}
        return log, env

    def test_checks_both_routes_and_preserves_failures_and_diagnostics(self):
        for cargo_status, rustfmt_status in ((0, 0), (0, 1), (7, 0), (7, 2)):
            with self.subTest(cargo=cargo_status, rustfmt=rustfmt_status):
                log, env = self.mock_tools(cargo_status, rustfmt_status)
                log.unlink(missing_ok=True)
                result = self.run_checker(env)
                self.assertEqual(result.returncode, cargo_status or rustfmt_status)
                self.assertEqual(
                    [json.loads(line) for line in log.read_text().splitlines()],
                    [
                        [str(self.root), "cargo", "fmt", "--check"],
                        [str(self.root), "rustfmt", "--edition", "2024", "--check", *INCLUDED],
                    ],
                )
                self.assertIn("cargo diagnostic", result.stderr)
                self.assertIn("rustfmt diagnostic", result.stderr)

    @unittest.skipUnless(shutil.which("rustfmt"), "needs rustfmt")
    def test_real_rustfmt_traversal_and_nested_fragments_without_rewrites(self):
        # Model Cargo's root rustfmt call, without executing Cargo or metadata.
        self.write("tools/cargo", '#!/bin/sh\nexec rustfmt --edition 2024 --check src/lib.rs\n')
        (self.root / "tools/cargo").chmod(0o755)
        env = os.environ | {"PATH": str(self.root / "tools") + os.pathsep + os.environ["PATH"]}
        self.exercise_included_sources(env)

    @unittest.skipUnless(
        os.environ.get("CHECK_FORMAT_CARGO_FIXTURE") == "1"
        and shutil.which("cargo") and shutil.which("rustfmt"),
        "opt in with CHECK_FORMAT_CARGO_FIXTURE=1 for the tiny Cargo fixture",
    )
    def test_actual_cargo_root_omission_and_explicit_include_checks(self):
        self.exercise_included_sources(os.environ)

    def exercise_included_sources(self, env):
        self.write("Cargo.toml", """[workspace]
[package]
name = "format-fixture"
version = "0.0.0"
edition = "2024"
""")
        self.write("src/lib.rs", 'include!("modules.rs");\n')
        self.write("src/modules.rs", '#[cfg(feature = "ui")]\nmod gated;\n')
        for relative in INCLUDED[1:]:
            self.write(relative, "fn included_fragment() {}\n")
        # Include fragments remain invisible even when encountered in a module
        # that rustfmt can traverse; direct checking must cover every one.
        self.write("src/gated.rs", """pub fn gated() {}
include!("terminal/worker/bounded_input_tests.rs");
include!("host/model/views/placement_tests.rs");
include!("../tests/support/iced_tree.rs");
include!("../tests/support/repository_scale.rs");
""")
        clean = self.run_checker(env)
        self.assertEqual(clean.returncode, 0, clean.stdout + clean.stderr)
        for relative in ("src/gated.rs", *INCLUDED[1:]):
            with self.subTest(source=relative):
                path = self.root / relative
                original = path.read_text()
                path.write_text(original + "fn   malformed( ) { }\n")
                before = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
                cargo = subprocess.run(
                    ["cargo", "fmt", "--check"], cwd=self.root, env=env,
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(cargo.returncode, 0, cargo.stdout + cargo.stderr)
                checked = self.run_checker(env)
                self.assertNotEqual(checked.returncode, 0)
                self.assertIn(path.name, checked.stdout + checked.stderr)
                self.assertEqual(before, {path: path.read_bytes() for path in before})
                path.write_text(original)
                repaired = self.run_checker(env)
                self.assertEqual(repaired.returncode, 0, repaired.stdout + repaired.stderr)


if __name__ == "__main__":
    unittest.main()
