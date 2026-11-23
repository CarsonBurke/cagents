#!/usr/bin/env python3
"""The pinned sherpa-onnx library fetcher, with a fixture curl and no network."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/sherpa-onnx-libs"
SHA256 = "082743c80ec9d3fd739df174b9e46ab08ac77c9f57a1d2b09977a7fd5324727d"
STEM = "sherpa-onnx-v2.0.2-linux-x64-static-lib"
# Bounds only a hang; a passing run returns as soon as its process exits.
HANG_GUARD = 60


class SherpaOnnxLibs(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="sherpa-onnx-libs-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.cache = self.directory / "cache with spaces"
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.calls = self.directory / "curl-calls"
        # Serves a well-formed archive whose digest is not the pinned one.
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:bz2") as output:
            payload = b"not the pinned library\n"
            info = tarfile.TarInfo(f"{STEM}/lib/libsherpa-onnx-c-api.a")
            info.size = len(payload)
            output.addfile(info, io.BytesIO(payload))
        self.served = self.directory / "served.tar.bz2"
        self.served.write_bytes(archive.getvalue())
        curl = self.tools / "curl"
        curl.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >> "$FIXTURE_CURL_CALLS"\n'
            'while [ "$#" -gt 0 ]; do\n'
            '  if [ "$1" = --output ]; then cp "$FIXTURE_SERVED" "$2" 2>/dev/null || exit 22; exit 0; fi\n'
            '  shift\ndone\nexit 22\n'
        )
        curl.chmod(0o755)
        self.environment = os.environ | {
            "PATH": f"{self.tools}:{os.environ['PATH']}",
            "FIXTURE_CURL_CALLS": str(self.calls),
            "FIXTURE_SERVED": str(self.served),
        }

    def run_script(self, *arguments):
        return subprocess.run(
            ["/bin/sh", str(SCRIPT), *arguments], env=self.environment,
            capture_output=True, text=True, timeout=HANG_GUARD,
        )

    def test_bytes_other_than_the_pinned_archive_are_refused_and_discarded(self):
        result = self.run_script(str(self.cache))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match its pinned SHA-256", result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn(f"/releases/download/v2.0.2/{STEM}.tar.bz2", self.calls.read_text())
        self.assertEqual(list(self.cache.iterdir()), [], "a refused download must leave nothing")

    def test_verified_libraries_are_reused_without_downloading(self):
        libraries = self.cache / SHA256 / STEM / "lib"
        libraries.mkdir(parents=True)
        result = self.run_script(str(self.cache))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{libraries}\n")
        self.assertFalse(self.calls.exists())

    def test_a_failed_download_names_the_archive(self):
        self.environment["FIXTURE_SERVED"] = str(self.directory / "absent")
        result = self.run_script(str(self.cache))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"could not download {STEM}", result.stderr)
        self.assertEqual(list(self.cache.iterdir()), [])

    def test_usage_requires_one_cache_directory(self):
        for arguments in ((), ("",), ("one", "two")):
            with self.subTest(arguments=arguments):
                result = self.run_script(*arguments)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("usage: sherpa-onnx-libs CACHE", result.stderr)
        self.assertFalse(self.calls.exists())

    @unittest.skipUnless(os.environ.get("SHERPA_ONNX_LIBS_NETWORK") == "1",
                         "opt in with SHERPA_ONNX_LIBS_NETWORK=1 to fetch the real archive")
    def test_the_pinned_release_archive_verifies(self):
        (self.tools / "curl").unlink()
        result = self.run_script(str(self.cache))
        self.assertEqual(result.returncode, 0, result.stderr)
        libraries = Path(result.stdout.strip())
        self.assertTrue((libraries / "libsherpa-onnx-c-api.a").is_file())
        self.assertTrue((libraries / "libonnxruntime.a").is_file())


if __name__ == "__main__":
    unittest.main()
