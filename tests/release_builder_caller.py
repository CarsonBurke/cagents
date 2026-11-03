#!/usr/bin/env python3
"""Caller refusal without invoking a real release, namespace or lifecycle.

Real caller markers are never read, copied, assigned, replaced or unset. The
full-script refusal test inherits them naturally and skips if none is present;
only an owned fake PATH changes in its child. Exhaustive marker cases execute
an alpha-renamed guard-only model using harmless TEST_RELEASE_* variables.
No test executes the unmarked full release path or establishes User authority.
"""

import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
BUILDER = ROOT / "scripts/build-release-linux"
MARKERS = ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN")
MODEL_NAMES = ("TEST_RELEASE_CALLER_AUTHORITY", "TEST_RELEASE_CALLER_TOKEN", "TEST_RELEASE_CALLER_SESSION")
REFUSAL = ("build-release-linux: full release builds require your own terminal; "
           "hosted sessions cannot run installation or daemon lifecycle checks\n")


class ReleaseBuilderCaller(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="release-caller-contract-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.fixture = Path(temporary.name)
        self.tools = self.fixture / "tools"
        self.tools.mkdir()
        self.script = self.fixture / "scripts/build-release-linux"
        self.script.parent.mkdir()
        self.script.write_bytes(BUILDER.read_bytes())
        self.events = self.fixture / "events"
        # Any lost admission guard stops at the FIRST platform command, before
        # cache/provision/build work. No stub delegates to a real command.
        for name in ("uname", "id", "bwrap", "git", "curl", "tar", "sha256sum", "flock",
                     "realpath", "stat", "mktemp", "install", "mkdir", "cp", "cat", "mv", "rm", "sed", "cargo"):
            stub = self.tools / name
            stub.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(name) + " >> "
                            + shlex.quote(str(self.events)) + "\nexit 96\n")
            stub.chmod(0o700)
        dirname = self.tools / "dirname"
        dirname.write_text("#!/bin/sh\nprintf '%s\\n' dirname >> " + shlex.quote(str(self.events))
                           + "\nprintf '%s\\n' " + shlex.quote(str(self.script.parent)) + "\n")
        dirname.chmod(0o700)

    def run_full(self, *arguments):
        # env is deliberately omitted. The wrapper sets ONLY a harmless owned
        # PATH and execs the private script copy with the inherited caller intact.
        return subprocess.run(
            ["/bin/bash", "-c", 'PATH=$1; export PATH; shift; exec /bin/bash "$@"',
             "release-caller-contract", str(self.tools), str(self.script), *arguments],
            cwd=self.fixture, capture_output=True, text=True, timeout=10,
        )

    def observed_commands(self):
        return self.events.read_text().splitlines() if self.events.exists() else []

    def guard_model(self, marker=None, value=""):
        source = BUILDER.read_text()
        start = source.index("# Full release validation includes installation and daemon lifecycle checks.")
        end = source.index("\n[[ $(uname -s)", start)
        guard = source[start:end]
        for actual, model in zip(MARKERS, MODEL_NAMES):
            self.assertIn("${" + actual + "+x}", guard)
            guard = guard.replace(actual, model)
        self.assertFalse(any(name in guard for name in MARKERS))
        fail_start = source.index("fail() {")
        fail_end = source.index("\n}\n", fail_start) + len("\n}\n")
        model = "PROGRAM=guard-model\n" + source[fail_start:fail_end]
        model += "unset " + " ".join(MODEL_NAMES) + "\n"
        if marker is not None:
            self.assertIn(marker, MODEL_NAMES)
            model += marker + "=" + shlex.quote(value) + "\n"
        model += guard + "\nprintf '%s\\n' guard-model-fallthrough\n"
        # This is pure shell syntax, not the release entry point. Actual
        # inherited authority remains untouched and is not used by this model.
        return subprocess.run(["/bin/bash", "-euc", model], cwd=self.fixture,
                              capture_output=True, text=True, timeout=10)

    def test_naturally_hosted_caller_refuses_before_platform_or_cache_commands(self):
        if not any(name in os.environ for name in MARKERS):
            self.skipTest("no naturally inherited hosted marker; no User-context full release invocation")
        result = self.run_full("--ref", "synthetic-ref", "--output", str(self.fixture / "assets"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, REFUSAL)
        self.assertEqual(self.observed_commands(), ["dirname"])
        self.assertFalse((self.fixture / "assets").exists())
        self.assertFalse((self.fixture / "dist").exists())

    def test_help_remains_available_with_inherited_caller_unchanged(self):
        for arguments in (("--help",), ("--ref", "synthetic-ref", "--help"),
                          ("--output", str(self.fixture / "assets"), "--help")):
            with self.subTest(arguments=arguments):
                self.events.unlink(missing_ok=True)
                result = self.run_full(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Usage: scripts/build-release-linux", result.stdout)
                self.assertEqual(result.stderr, "")
                self.assertEqual(self.observed_commands(), ["dirname"])
                self.assertFalse((self.fixture / "assets").exists())
                self.assertFalse((self.fixture / "dist").exists())

    def test_alpha_renamed_model_refuses_every_present_marker_even_empty(self):
        for marker in MODEL_NAMES:
            for value in ("", "synthetic-model-value-never-print"):
                with self.subTest(marker=marker, empty=not value):
                    result = self.guard_model(marker, value)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(result.stderr, REFUSAL.replace("build-release-linux:", "guard-model:"))
                    self.assertNotIn("synthetic-model-value-never-print", result.stderr)
                    self.assertEqual(self.observed_commands(), [])

    def test_absent_marker_model_falls_through_without_running_release_commands(self):
        result = self.guard_model()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "guard-model-fallthrough\n")
        self.assertEqual(result.stderr, "")
        self.assertEqual(self.observed_commands(), [])

    def test_unknown_argument_keeps_existing_readonly_failure(self):
        result = self.run_full("--not-a-release-option")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "build-release-linux: unknown argument: --not-a-release-option (see --help)\n")
        self.assertEqual(self.observed_commands(), ["dirname"])


if __name__ == "__main__":
    unittest.main()
