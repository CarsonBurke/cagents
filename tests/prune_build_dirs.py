#!/usr/bin/env python3
"""Exercise scripts/prune-build-dirs on private directories only."""

import fcntl
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
PRUNER = ROOT / "scripts/prune-build-dirs"
DAY = 86400


class PruneBuildDirs(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-prune-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.roots = self.root / "roots"
        self.roots.mkdir(parents=True)
        self.home.mkdir()
        self.log = self.root / "removed.tsv"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_BUILD_")}
        # Only private directories are ever searched.
        self.env |= {"HOME": str(self.home), "CAGENTS_BUILD_PRUNE_ROOTS": str(self.roots)}

    def prune(self, *arguments, apply=True):
        command = [str(PRUNER), "--lock", str(self.root / "lock"), "--log", str(self.log),
                   "--mirror", str(self.roots / "cargo-build")]
        result = subprocess.run(command + (["--apply"] if apply else []) + list(arguments),
                                env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def target(self, path, age_days=30, marker="rustc"):
        """A Cargo target directory whose every entry was last written age_days ago."""
        path.mkdir(parents=True)
        if marker == "rustc":
            (path / ".rustc_info.json").write_text("{}")
        else:
            # A separate build directory: Cargo tags it.
            (path / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
        (path / "debug/.fingerprint/crate-0123").mkdir(parents=True)
        (path / "debug/.cargo-lock").write_text("")
        (path / "debug/deps").mkdir()
        (path / "debug/deps/libcrate.rlib").write_bytes(b"x" * 8192)
        self.age(path, age_days)
        return path

    def age(self, path, days):
        when = time.time() - days * DAY
        for directory, names, files in os.walk(path):
            for name in names + files:
                os.utime(os.path.join(directory, name), (when, when), follow_symlinks=False)
        os.utime(path, (when, when))

    def test_removes_idle_build_output_wherever_it_is(self):
        worktree = self.target(self.roots / "repo/.worktrees/old/target")
        lane = self.target(self.roots / "lane/target-after")
        separate = self.target(self.roots / "build/target", marker="fingerprint")
        output = self.prune()
        for path in (worktree, lane, separate):
            self.assertFalse(path.exists(), path)
            self.assertIn(f"removed {path}", output)
        # The worktree that held it stays.
        self.assertTrue((self.roots / "repo/.worktrees/old").is_dir())
        self.assertEqual(len(self.log.read_text().splitlines()), 3)

    def test_a_recent_write_anywhere_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        (path / "debug/deps/libcrate.rlib").touch()
        self.prune()
        self.assertTrue(path.exists())

    def test_idle_days_are_configurable(self):
        path = self.target(self.roots / "lane/target", age_days=3)
        self.prune()
        self.assertTrue(path.exists())
        self.prune("--max-idle", "2")
        self.assertFalse(path.exists())

    def test_a_held_cargo_lock_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        with open(path / "debug/.cargo-lock") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn("a build holds its lock", output)

    def test_a_process_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        process = subprocess.Popen(["sleep", "30"], cwd=path / "debug")
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn(f"process {process.pid} uses it", output)

    def test_an_open_file_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        process = subprocess.Popen(
            ["python3", "-c", "import sys, time; f = open(sys.argv[1]); print(flush=True); "
             "time.sleep(30)", str(path / "debug/deps/libcrate.rlib")],
            stdout=subprocess.PIPE, text=True)
        self.addCleanup(process.stdout.close)
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        process.stdout.readline()
        output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn(f"process {process.pid} uses it", output)

    def test_other_caches_are_never_taken_for_build_output(self):
        venv = self.roots / "state/.venv"
        venv.mkdir(parents=True)
        (venv / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
        (venv / "debug").mkdir()
        evidence = self.roots / "evidence"
        evidence.mkdir()
        (evidence / "handoff.md").write_text("notes")
        self.age(self.roots, 400)
        self.prune()
        self.assertTrue(venv.exists())
        self.assertTrue(evidence.exists())

    def test_kept_and_protected_directories_stay(self):
        kept = self.target(self.roots / "repo/target")
        self.prune("--keep", str(self.roots / "repo/target"))
        self.assertTrue(kept.exists())
        # The user's installs and the offload slots are never searched, even
        # when a root contains them.
        self.env["CAGENTS_BUILD_PRUNE_ROOTS"] = str(self.home)
        share = self.target(self.home / ".local/share/cagents/target")
        slot = self.target(self.home / ".cache/cagents-offload/cagents/slot-0/src/target")
        self.prune()
        self.assertTrue(share.exists())
        self.assertTrue(slot.exists())

    def test_the_search_depth_is_bounded_except_in_the_mirror(self):
        deep = self.target(self.roots / "a/b/c/d/e/f/g/target")
        mirrored = self.target(self.roots / "cargo-build/a/b/c/d/e/f/g/target")
        self.prune()
        self.assertTrue(deep.exists())
        self.assertFalse(mirrored.exists())
        self.prune("--max-depth", "8")
        self.assertFalse(deep.exists())

    def test_the_main_checkouts_build_stays_wherever_cargo_puts_it(self):
        checkout = self.root / "repo"
        in_tree = self.target(checkout / "target")
        mirrored = self.target(self.roots / "cargo-build" / str(checkout).lstrip("/") / "target")
        self.prune("--root", str(checkout), "--keep-checkout", str(checkout))
        self.assertTrue(in_tree.exists())
        self.assertTrue(mirrored.exists())

    def test_symlinks_are_not_followed(self):
        outside = self.target(self.root / "outside/target")
        (self.roots / "link").symlink_to(self.root / "outside")
        self.prune()
        self.assertTrue(outside.exists())

    def test_an_unreadable_directory_is_skipped_not_fatal(self):
        private = self.roots / "private/target"
        (private / "debug").mkdir(parents=True)
        private.chmod(0)
        self.addCleanup(private.chmod, 0o755)
        path = self.target(self.roots / "lane/target")
        self.prune()
        self.assertFalse(path.exists())

    def test_a_dry_run_removes_nothing(self):
        path = self.target(self.roots / "lane/target")
        output = self.prune(apply=False)
        self.assertTrue(path.exists())
        self.assertIn(f"would remove {path}", output)

    def test_a_removed_workspaces_mirrored_build_directory_goes_sooner(self):
        mirror = self.roots / "cargo-build"
        workspace = self.root / "workspaces/live"
        workspace.mkdir(parents=True)
        live = self.target(mirror / str(workspace).lstrip("/") / "target", age_days=1)
        gone = self.target(mirror / str(self.root / "workspaces/gone").lstrip("/") / "target",
                           age_days=1)
        self.prune("--orphan-hours", "0")
        self.assertTrue(live.exists())
        self.assertFalse(gone.exists())
        self.assertIn("workspace removed", self.log.read_text())

    def test_an_interrupted_removal_is_finished(self):
        leftover = self.roots / "lane/.target.cagents-pruned-999999999"
        (leftover / "debug").mkdir(parents=True)
        (leftover / "debug/partial").write_text("")
        live = self.target(self.roots / "lane/target", age_days=0)
        self.prune()
        self.assertFalse(leftover.exists())
        self.assertTrue(live.exists())

    def test_a_build_that_starts_after_removal_finds_no_directory(self):
        path = self.target(self.roots / "lane/target")
        self.prune()
        self.assertFalse(path.exists())
        self.assertEqual(list((self.roots / "lane").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
