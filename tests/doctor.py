#!/usr/bin/env python3
"""scripts/doctor names each missing prerequisite and changes nothing."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
LIBRARIES = ("dbus-1", "egl", "fontconfig", "freetype2", "gbm", "gl", "libudev",
             "openssl", "wayland-client", "xkbcommon")


class Doctor(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-doctor-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "scripts").mkdir()
        shutil.copy(ROOT / "scripts/doctor", self.root / "scripts/doctor")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.93.0"\n')
        (self.root / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.log"
        self.offload(0, "fw16")
        for name, text in {
            "git": "git version 2.43.0",
            "rustc": "rustc 1.93.0 (abcdef 2026-01-01)",
            "cargo": "cargo 1.93.0 (abcdef 2026-01-01)",
            "rustfmt": "rustfmt 1.8.0-stable (abcdef 2026-01-01)",
            "zig": "0.15.2",
            "rustup": "rustup 1.28.2",
            "ssh": "",
            "niri-harness": "",
            "bwrap": "",
        }.items():
            self.tool(name, f"echo '{text}'")
        for name in ("cc", "c++", "clang", "cmake", "ninja"):
            self.tool(name, "")
        self.tool("pkg-config", "exit 0")

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(f'#!/bin/sh\necho "{name} $* auto=${{RUSTUP_AUTO_INSTALL-}}" >> "{self.log}"\n{body}\n')
        path.chmod(0o755)

    def offload(self, status, output=""):
        path = self.root / "scripts/cargo-offload"
        path.write_text(f"#!/bin/sh\nprintf '%s' '{output}'\nexit {status}\n")
        path.chmod(0o755)

    def doctor(self, *arguments, environment=None):
        # Only the fixture's tools: the doctor and the stubs name their interpreters absolutely.
        env = {"PATH": str(self.bin), "HOME": str(self.root), "WAYLAND_DISPLAY": "wayland-0"}
        env.update(environment or {})
        return subprocess.run([str(self.root / "scripts/doctor"), *arguments], cwd=self.root, env=env,
                              capture_output=True, text=True, timeout=60)

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_a_ready_checkout_with_a_reachable_build_host_passes(self):
        result = self.doctor()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ok   build host: fw16 answers over SSH", result.stdout)
        self.assertIn("ssh -o BatchMode=yes -o ConnectTimeout=10 fw16 true auto=", self.calls())
        # Rust is asked without permission to download a toolchain.
        self.assertTrue(any(call.startswith("rustc --version auto=0") for call in self.calls()))
        # With a remote build host, no native build library is required here.
        self.assertFalse(any(call.startswith("pkg-config") for call in self.calls()))

    def test_an_unreachable_build_host_fails_with_the_way_out(self):
        self.tool("ssh", "exit 255")
        result = self.doctor()
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL build host: fw16 does not answer over SSH", result.stdout)
        self.assertIn("CAGENTS_BUILD_REMOTE=local", result.stdout)

    def test_a_loopback_build_host_needs_no_ssh(self):
        self.offload(0, "fw16\tloopback")
        result = self.doctor()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("fw16, through its own build slots", result.stdout)
        self.assertFalse(any(call.startswith("ssh") for call in self.calls()))

    def test_building_here_requires_the_native_libraries_and_names_the_missing_ones(self):
        self.offload(75)
        self.tool("pkg-config", 'case "$2" in gbm|xkbcommon) exit 1 ;; esac')
        (self.bin / "ninja").unlink()
        result = self.doctor()
        self.assertEqual(result.returncode, 1)
        self.assertIn("ok   build host: Rust compiles on this host", result.stdout)
        self.assertIn("FAIL build tools: missing ninja", result.stdout)
        self.assertIn("FAIL build libraries: pkg-config cannot find gbm, xkbcommon", result.stdout)
        self.assertEqual(sorted(call.split()[2] for call in self.calls() if call.startswith("pkg-config")),
                         sorted(LIBRARIES))

    def test_an_undecidable_build_host_fails(self):
        self.offload(2)
        result = self.doctor()
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL build host: cannot tell where Rust compiles", result.stdout)

    def test_wrong_or_missing_toolchains_fail(self):
        cases = {
            "rustc": ("echo 'rustc 1.94.0 (x 2026-03-01)'", "FAIL rust: 1.93.0 from rust-toolchain.toml"),
            "rustfmt": ("exit 1", "FAIL rust: 1.93.0 has no rustfmt"),
            "zig": ("echo 0.14.1", "FAIL zig: 0.14.1 does not match the mise.toml pin 0.15.2"),
            "git": ("echo 'git version 2.34.1'", "FAIL git: 2.34.1 is older than 2.36"),
        }
        for name, (body, message) in cases.items():
            with self.subTest(tool=name):
                original = (self.bin / name).read_text()
                self.tool(name, body)
                result = self.doctor()
                self.assertEqual(result.returncode, 1)
                self.assertIn(message, result.stdout)
                (self.bin / name).write_text(original)

    def test_a_missing_tool_is_named_rather_than_raised(self):
        for name, message in (("zig", "FAIL zig: not found"), ("rustup", "FAIL rust: rustup not found"),
                              ("git", "FAIL git: not found")):
            with self.subTest(tool=name):
                original = (self.bin / name).read_text()
                (self.bin / name).unlink()
                result = self.doctor()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(message, result.stdout)
                self.assertNotIn("Traceback", result.stderr)
                self.tool(name, "")
                (self.bin / name).write_text(original)

    def test_optional_prerequisites_only_warn(self):
        (self.bin / "niri-harness").unlink()
        (self.bin / "bwrap").unlink()
        result = self.doctor(environment={"WAYLAND_DISPLAY": ""})
        self.assertEqual(result.returncode, 0, result.stdout)
        for line in ("warn wayland:", "warn niri-harness: not found", "warn bwrap: not found"):
            self.assertIn(line, result.stdout)

    def test_the_build_host_check_can_be_skipped(self):
        self.offload(2)
        result = self.doctor("--no-build-host")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("build host", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
