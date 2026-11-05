#!/usr/bin/env python3
"""Exercise scripts/seed-build-dir on private directories, without Cargo."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


CARGO_TAG = (
    "Signature: 8a477f597d28d172789f06886806bc55\n"
    "# This file is a cache directory tag created by cargo.\n"
    "# For information about cache directory tags see https://bford.info/cachedir/\n"
)
SEED = Path(__file__).resolve().parents[1] / "scripts/seed-build-dir"


class SeedBuildDirApplicability(unittest.TestCase):
    """Exercise CI's actual applicability block with synthetic source heads."""

    source = "scripts/seed-build-dir"
    fixture = "tests/seed_build_dir.py"

    @classmethod
    def setUpClass(cls):
        lines = (SEED.parents[1] / ".github/workflows/script-contracts.yml").read_text().splitlines(keepends=True)

        def block(name, *, optional=False):
            starts = [index for index, line in enumerate(lines) if line == f"      - name: {name}\n"]
            if optional and not starts:
                return None
            if len(starts) != 1:
                raise AssertionError(f"expected exactly one workflow step: {name}")
            run = lines[starts[0] + 1]
            if not run.startswith("        run: "):
                raise AssertionError(f"expected inline run command for {name}")
            if run != "        run: |\n":
                return run.removeprefix("        run: ")
            body = []
            for line in lines[starts[0] + 2:]:
                if line.startswith("          "):
                    body.append(line[10:])
                elif not line.strip():
                    body.append("\n")
                else:
                    break
            return "".join(body)

        cls.run_block = block("Check validation checkout and process custody contracts")
        if not cls.run_block.startswith("python3 -I -B - <<'PY'\n"):
            raise AssertionError("applicability must use its inline isolated Python")
        cls.syntax_block = block("Check shell syntax")
        # Include the old unconditional step if present: the absent-head test
        # must reproduce its real failure, rather than only notice a new tuple.
        cls.legacy_block = block("Check build directory seeding contracts", optional=True)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="seed-applicability-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def case(self, name):
        root = self.root / name
        root.mkdir(mode=0o700)
        return root

    def pair(self, root, *, exit_code=0):
        source, fixture = root / self.source, root / self.fixture
        source.parent.mkdir(parents=True, exist_ok=True)
        fixture.parent.mkdir(parents=True, exist_ok=True)
        # Valid shell syntax, but executing this source would fail. The workflow
        # should only syntax-check it and execute the private fixture below.
        source.write_text("#!/bin/sh\nexit 79\n")
        fixture.write_text(
            "import json\nfrom pathlib import Path\nimport sys\n"
            "Path('calls.json').write_text(json.dumps({'arguments': sys.argv[1:], "
            "'isolated': sys.flags.isolated, 'bytecode': sys.flags.dont_write_bytecode}))\n"
            f"raise SystemExit({exit_code})\n"
        )

    def invoke(self, root, *, whole_seeder_path=False, blocks=None):
        if blocks is None:
            blocks = ([self.legacy_block, self.run_block, self.syntax_block]
                      if whole_seeder_path else [self.run_block])
        stdout, stderr = "", ""
        for command in filter(None, blocks):
            result = subprocess.run(
                ["/bin/sh", "-e", "-c", command], cwd=root,
                env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                capture_output=True, text=True, timeout=15,
            )
            stdout += result.stdout
            stderr += result.stderr
            if result.returncode:
                break
        return subprocess.CompletedProcess(blocks, result.returncode, stdout, stderr)

    def other_shell_scripts(self, root):
        """The scripts the shell-syntax step checks besides the seeder's, as a
        head that predates the seeder still has them."""
        (root / "scripts").mkdir()
        for line in self.syntax_block.splitlines():
            words = line.split()
            if len(words) == 3 and words[:2] in (["sh", "-n"], ["bash", "-n"]):
                if words[2] != self.source and words[2].startswith("scripts/"):
                    (root / words[2]).write_text("#!/bin/sh\nexit 0\n")

    def assert_not_executed(self, root):
        self.assertFalse((root / "calls.json").exists())

    def test_old_head_without_seeder_passes_all_seeder_workflow_steps(self):
        root = self.case("old-head")
        self.other_shell_scripts(root)
        result = self.invoke(root, whole_seeder_path=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Not applicable: build directory seeding (source and fixture both absent at this head)\n",
                      result.stdout)
        self.assert_not_executed(root)
        self.assertFalse((root / self.source).exists())
        self.assertFalse((root / self.fixture).exists())

    def test_shell_syntax_step_also_accepts_a_head_without_the_seeder(self):
        root = self.case("old-shell-syntax")
        self.other_shell_scripts(root)
        result = self.invoke(root, blocks=[self.syntax_block])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_not_executed(root)

    def test_each_partial_pair_fails_instead_of_being_skipped(self):
        for missing in (self.source, self.fixture):
            with self.subTest(missing=missing):
                root = self.case("partial-" + Path(missing).name)
                self.pair(root)
                (root / missing).unlink()
                result = self.invoke(root)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("Incomplete contract pair:", result.stderr)
                self.assert_not_executed(root)

    def test_complete_pair_syntax_checks_source_and_runs_only_isolated_fixture(self):
        root = self.case("complete")
        self.pair(root)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Checking: build directory seeding\n", result.stdout)
        self.assertEqual(json.loads((root / "calls.json").read_text()), {
            "arguments": ["-v"], "isolated": 1, "bytecode": 1,
        })

    def test_fixture_failure_status_is_preserved(self):
        root = self.case("failed-fixture")
        self.pair(root, exit_code=43)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 43, result.stderr)
        self.assertTrue((root / "calls.json").exists())

    def test_shell_syntax_failure_stops_before_fixture_execution(self):
        root = self.case("bad-syntax")
        self.pair(root)
        (root / self.source).write_text("#!/bin/sh\nif\n")
        result = self.invoke(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(self.source, result.stderr)
        self.assert_not_executed(root)

    def test_nonregular_and_symlink_pair_members_fail_closed(self):
        for member in (self.source, self.fixture):
            for kind in ("directory", "symlink", "dangling"):
                with self.subTest(member=member, kind=kind):
                    root = self.case(Path(member).name + "-" + kind)
                    self.pair(root)
                    path = root / member
                    contents = path.read_bytes()
                    path.unlink()
                    if kind == "directory":
                        path.mkdir()
                    else:
                        target = root / "private-target"
                        if kind == "symlink":
                            target.write_bytes(contents)
                        path.symlink_to(target)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assert_not_executed(root)

    def test_nonregular_and_symlink_parents_fail_without_following_them(self):
        for parent in ("scripts", "tests"):
            for kind in ("file", "symlink", "dangling"):
                with self.subTest(parent=parent, kind=kind):
                    root = self.case(parent + "-" + kind)
                    path = root / parent
                    if kind == "file":
                        path.write_text("not a directory\n")
                    else:
                        target = root / "private-directory"
                        if kind == "symlink":
                            target.mkdir()
                        path.symlink_to(target, target_is_directory=True)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assert_not_executed(root)


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
        # Cargo tags only directories it creates; the pruner needs the tag.
        self.assertEqual((self.destination / "CACHEDIR.TAG").read_text(), CARGO_TAG)
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
        (self.destination / ".seed-lock").touch()
        result = self.seed()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertFalse((self.destination / "debug/deps").exists())
        self.assertFalse((self.destination / "dev-runtime").exists())
        # Seeded before seeding tagged directories, it still gets the tag.
        self.assertEqual((self.destination / "CACHEDIR.TAG").read_text(), CARGO_TAG)
        # A directory Cargo built itself, such as the main checkout's, is not
        # made prunable.
        (self.destination / "CACHEDIR.TAG").unlink()
        (self.destination / ".seed-lock").unlink()
        result = self.seed()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertFalse((self.destination / "CACHEDIR.TAG").exists())

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
            # Unbuilt, but tagged: Cargo will not tag a directory it did not create.
            self.assertEqual(sorted(path.name for path in self.destination.iterdir()),
                             [".seed-lock", "CACHEDIR.TAG"])


if __name__ == "__main__":
    unittest.main()
