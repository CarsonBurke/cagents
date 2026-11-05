#!/usr/bin/env python3
"""Exercise scripts/seed-build-dir on private directories, without Cargo."""

import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


SEED = Path(__file__).resolve().parents[1] / "scripts/seed-build-dir"


def reflinks(directory: Path) -> bool:
    source = directory / "probe-source"
    source.write_bytes(b"probe")
    result = subprocess.run(
        ["cp", "--reflink=always", source, directory / "probe-copy"],
        capture_output=True,
    )
    for path in (source, directory / "probe-copy"):
        path.unlink(missing_ok=True)
    return result.returncode == 0


class SeedBuildDir(unittest.TestCase):
    def setUp(self):
        # Reflinks need a file system that has them, which a RAM-backed /tmp
        # usually does not. /var/tmp is on disk.
        self.temporary = tempfile.TemporaryDirectory(prefix="cagents-seed-", dir="/var/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "main/target"
        for profile in ("debug", "dev-runtime"):
            for part in ("deps", "build/zstd-sys-1/out", ".fingerprint/zstd-sys-1"):
                (self.source / profile / part).mkdir(parents=True)
            (self.source / profile / "deps/libzstd-1.rlib").write_bytes(b"rlib")
            (self.source / profile / ".fingerprint/zstd-sys-1/lib-zstd").write_text("fingerprint")
            (self.source / profile / "incremental/cagents-1").mkdir(parents=True)
        # Fingerprints compare times, so the copy must keep them.
        self.stamp = 1_700_000_000
        os.utime(self.source / "debug/deps/libzstd-1.rlib", (self.stamp, self.stamp))
        self.destination = self.root / "worktree/target"
        self.env = dict(os.environ)
        self.env.pop("CAGENTS_BUILD_SEED", None)

    def seed(self, *arguments, **changes):
        return subprocess.run(
            [str(SEED), "--from", str(self.source), "--to", str(self.destination), *arguments],
            env={**self.env, **changes}, capture_output=True, text=True, timeout=60,
        )

    def test_a_new_build_directory_gets_the_compiled_dependencies(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        # A seeder killed outright leaves its staging directory behind.
        (self.destination / ".seeding.killed/debug").mkdir(parents=True)
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("seeded", result.stderr)
        self.assertEqual(
            (self.destination / "debug/deps/libzstd-1.rlib").stat().st_mtime, self.stamp)
        for profile in ("debug", "dev-runtime"):
            self.assertEqual((self.destination / profile / "deps/libzstd-1.rlib").read_bytes(), b"rlib")
            self.assertTrue((self.destination / profile / "build/zstd-sys-1/out").is_dir())
            self.assertTrue((self.destination / profile / ".fingerprint/zstd-sys-1/lib-zstd").is_file())
            # Incremental caches belong to the checkout's own crates.
            self.assertFalse((self.destination / profile / "incremental").exists())
        leftovers = [path.name for path in self.destination.iterdir() if path.name.startswith(".seeding")]
        self.assertEqual(leftovers, [])
        # Seeded once, it is a build directory like any other.
        (self.source / "debug/deps/libzstd-1.rlib").write_bytes(b"newer")
        result = self.seed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.destination / "debug/deps/libzstd-1.rlib").read_bytes(), b"rlib")

    def test_a_build_holding_either_cargo_lock_delays_the_copy(self):
        if not reflinks(self.root):
            self.skipTest("this file system cannot reflink")
        for lock in (".cargo-lock", ".cargo-build-lock"):
            with self.subTest(lock=lock):
                for profile in ("debug", "dev-runtime"):
                    subprocess.run(["rm", "-rf", "--", self.destination / profile], check=True)
                holder = subprocess.Popen(
                    ["flock", "-x", self.source / "debug" / lock, "sleep", "2"])
                self.addCleanup(holder.kill)
                time.sleep(0.3)
                started = time.monotonic()
                result = self.seed()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("seeded", result.stderr)
                self.assertGreaterEqual(time.monotonic() - started, 1.5)
                holder.wait(timeout=10)

    def test_a_built_directory_is_left_alone(self):
        (self.destination / "debug/.fingerprint").mkdir(parents=True)
        result = self.seed()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertFalse((self.destination / "debug/deps").exists())
        self.assertFalse((self.destination / "dev-runtime").exists())

    def test_nothing_to_copy_or_opted_out_does_nothing(self):
        for name, run in (
            ("missing source", lambda: subprocess.run(
                [str(SEED), "--from", str(self.root / "absent"), "--to", str(self.destination)],
                env=self.env, capture_output=True, text=True, timeout=60)),
            ("opted out", lambda: self.seed(CAGENTS_BUILD_SEED="0")),
            ("same directory", lambda: subprocess.run(
                [str(SEED), "--from", str(self.source), "--to", str(self.source)],
                env=self.env, capture_output=True, text=True, timeout=60)),
        ):
            with self.subTest(case=name):
                result = run()
                self.assertEqual((result.returncode, result.stderr), (0, ""))
                self.assertFalse((self.destination / "debug").exists())

    def test_a_file_system_without_reflinks_leaves_the_destination_unbuilt(self):
        shm = Path("/dev/shm")
        if not shm.is_dir():
            self.skipTest("no tmpfs to stand in for a file system without reflinks")
        with tempfile.TemporaryDirectory(prefix="cagents-seed-", dir=shm) as other:
            if reflinks(Path(other)):
                self.skipTest("/dev/shm can reflink here")
            self.destination = Path(other) / "target"
            result = self.seed()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("cannot reflink", result.stderr)
            self.assertEqual(sorted(path.name for path in self.destination.iterdir()), [".seed-lock"])


if __name__ == "__main__":
    unittest.main()
