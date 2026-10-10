#!/usr/bin/env python3
"""Run the public installer's platform gate without downloading or installing."""
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"


@unittest.skipUnless(
    platform.system() == "Linux" and platform.machine() == "x86_64" and os.getuid() != 0,
    "the installer requires an ordinary Linux x86_64 user",
)
class InstallerPlatform(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cagents-installer-platform-", dir="/var/tmp")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.home = self.root / "home"
        self.home.mkdir()
        self.prefix = self.root / "install"
        self.prefix.mkdir()
        (self.prefix / "sentinel").write_bytes(b"existing installation\n")
        (self.home / "sentinel").write_bytes(b"saved user data\n")
        shim = self.bin / "getconf"
        shim.write_text(
            '#!/bin/sh\n'
            '[ "$#" -eq 1 ] && [ "$1" = GNU_LIBC_VERSION ] || exit 99\n'
            'printf "called\\n" >> "$PLATFORM_CALLS"\n'
            'printf "%s\\n" "$PLATFORM_GLIBC"\n'
            'exit "$PLATFORM_STATUS"\n'
        )
        shim.chmod(0o755)

    def run_installer(self, libc, *, status=0, relative_home=False, args=()):
        # Inherit no credentials, application runtime or desktop environment.
        env = {
            "PATH": str(self.bin) + os.pathsep + os.defpath,
            "HOME": "relative-home" if relative_home else str(self.home),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_DATA_HOME": str(self.home / "data"),
            "PLATFORM_GLIBC": libc,
            "PLATFORM_STATUS": str(status),
            "PLATFORM_CALLS": str(self.root / "calls"),
        }
        return subprocess.run(
            ["/bin/sh", str(INSTALLER), "--prefix", str(self.prefix), *args],
            env=env, cwd=self.root, text=True, capture_output=True, timeout=3,
        )

    def assert_unchanged(self):
        self.assertEqual(list(self.prefix.iterdir()), [self.prefix / "sentinel"])
        self.assertEqual((self.prefix / "sentinel").read_bytes(), b"existing installation\n")
        self.assertEqual(list(self.home.iterdir()), [self.home / "sentinel"])
        self.assertEqual((self.home / "sentinel").read_bytes(), b"saved user data\n")

    def test_old_glibc_rejected_before_install_mutation(self):
        for libc in ("glibc 2.34", "glibc 2.9", "glibc 1.99"):
            with self.subTest(libc=libc):
                result = self.run_installer(libc)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"glibc 2.35 or newer is required (found {libc})", result.stderr)
                self.assert_unchanged()

    def test_supported_versions_reach_the_next_public_guard(self):
        # An invalid HOME intentionally stops the public script immediately
        # after successful platform validation, without beginning installation.
        for libc in ("glibc 2.35", "glibc 2.36", "glibc 2.100", "glibc 3.0", "glibc 02.035"):
            with self.subTest(libc=libc):
                result = self.run_installer(libc, relative_home=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("HOME must be an absolute path", result.stderr)
                self.assert_unchanged()

    def test_malformed_glibc_version_fails_closed(self):
        for libc in ("glibc unknown", "glibc 2", "glibc 2.35suffix", "glibc 2.35.1", "glibc 2.35\nglibc 3.0"):
            with self.subTest(libc=libc):
                result = self.run_installer(libc)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("could not determine a valid glibc version", result.stderr)
                self.assert_unchanged()

    def test_unknown_or_musl_environment_fails_closed(self):
        for libc, status in (("", 1), ("glibc 2.35", 1), ("musl 1.2.5", 0), ("unknown", 0)):
            with self.subTest(libc=libc):
                result = self.run_installer(libc, status=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("requires a glibc system", result.stderr)
                self.assert_unchanged()

    def test_old_glibc_allows_owned_uninstall_preserving_user_data(self):
        self.remove_fixture(purge=False)

    def test_old_glibc_allows_explicit_purge_of_only_owned_user_data(self):
        self.remove_fixture(purge=True)

    def remove_fixture(self, *, purge):
        (self.prefix / ".cagents-install-root").write_text("cagents install root\n")
        binaries = self.prefix / "versions" / "test" / "bin"
        binaries.mkdir(parents=True)
        cli = binaries / "cagents"
        cli.write_text('#!/bin/sh\n[ "$*" = "--json daemon status" ] || exit 99\nprintf \'%s\\n\' \'{"ok":true,"command":"daemon.status","data":{"running":false}}\'\n')
        cli.chmod(0o755)
        (self.prefix / "current").symlink_to("versions/test")
        saved_paths = [self.home / kind / "cagents" / "saved" for kind in ("data", "config")]
        for path in saved_paths:
            path.parent.mkdir(parents=True)
            path.write_bytes(b"preserve unless explicitly purged\n")
        # These traps fail if removal enters any artifact/download path.
        for command in ("curl", "gh", "wget", "tar"):
            trap = self.bin / command
            trap.write_text('#!/bin/sh\nprintf "called\\n" >> "$HOME/network-called"\nexit 99\n')
            trap.chmod(0o755)
        result = self.run_installer("glibc 2.34", args=("--purge" if purge else "--uninstall",))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.prefix / "current").exists())
        self.assertFalse((self.prefix / "versions").exists())
        self.assertFalse((self.prefix / ".cagents-install-root").exists())
        self.assertEqual((self.prefix / "sentinel").read_bytes(), b"existing installation\n")
        self.assertEqual((self.home / "sentinel").read_bytes(), b"saved user data\n")
        self.assertFalse((self.home / "network-called").exists())
        for path in saved_paths:
            self.assertEqual(path.exists(), not purge)
            if not purge:
                self.assertEqual(path.read_bytes(), b"preserve unless explicitly purged\n")

    def test_help_stays_available_without_platform_detection(self):
        result = self.run_installer("", status=1, relative_home=True, args=("--help",))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Usage: install.sh", result.stdout)
        self.assertFalse((self.root / "calls").exists())
        self.assert_unchanged()


if __name__ == "__main__":
    unittest.main()
