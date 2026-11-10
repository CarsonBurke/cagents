#!/usr/bin/python3
"""Resolve current and isolated compatible harness installs without starting a session."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts/require-niri-harness"
PIN = 'current'


class Harness(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='harness-capabilities-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        # Name only the standard tools the resolver itself uses; never consult a real harness.
        for name in ('timeout', 'dirname', 'grep'):
            (self.bin / name).symlink_to('/usr/bin/' + name)
        self.env = os.environ | {'HOME': str(self.root), 'PATH': str(self.bin)}
        self.pinned = self.root / '.local/lib/cagents/niri-harness' / PIN / 'bin/niri-harness'

    def install(self, path, *, current):
        path.parent.mkdir(parents=True, exist_ok=True)
        body = ('echo "--root --width --portal --backend --host-bus --renderer"' if current
                else "echo 'legacy session CLI' >&2; exit 2")
        path.write_text('#!/bin/sh\nif [ "$1" = "--help" ]; then echo "start stop list env exec gc"; exit 0; fi\n[ "$1 $2" = "start --help" ] || exit 99\n' + body + '\n')
        path.chmod(0o755)

    def run_checker(self):
        return subprocess.run([str(CHECKER)], env=self.env, text=True, capture_output=True, timeout=15)

    def test_current_path_install_is_used(self):
        self.install(self.bin / 'niri-harness', current=True)
        self.assertEqual(self.run_checker().stdout.strip(), str(self.bin))

    def test_legacy_path_uses_isolated_current_install(self):
        self.install(self.bin / 'niri-harness', current=False)
        self.install(self.pinned, current=True)
        self.assertEqual(self.run_checker().stdout.strip(), str(self.pinned.parent))

    def test_isolated_pointer_resolves_to_an_immutable_version(self):
        self.install(self.bin / 'niri-harness', current=False)
        version = self.pinned.parents[2] / 'qualified-version'
        self.install(version / 'bin/niri-harness', current=True)
        self.pinned.parents[1].symlink_to(version, target_is_directory=True)
        self.assertEqual(self.run_checker().stdout.strip(), str(version / 'bin'))

    def test_incompatible_install_fails_with_the_required_cli(self):
        self.install(self.bin / 'niri-harness', current=False)
        result = self.run_checker()
        self.assertEqual(result.returncode, 1)
        self.assertIn('requires a compatible niri-harness CLI', result.stderr)
        self.assertEqual(result.stdout, '')

    def test_runtime_without_compositor_renderer_selection_is_refused(self):
        path = self.bin / 'niri-harness'
        self.install(path, current=True)
        path.write_text(path.read_text().replace('--renderer', ''))
        self.assertEqual(self.run_checker().returncode, 1)

    def test_missing_optional_capability_is_refused(self):
        self.install(self.bin / 'niri-harness', current=True)
        path = self.bin / 'niri-harness'
        path.write_text(path.read_text().replace('--portal', ''))
        self.assertEqual(self.run_checker().returncode, 1)


if __name__ == '__main__':
    unittest.main()
