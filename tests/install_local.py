#!/usr/bin/env python3
"""Check which checkout states scripts/install-local agrees to build.

A fixture repository carries the real script and a packaging stub that records
the build and stops it, so nothing is compiled, staged or installed.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PACKAGE_STUB = '#!/bin/sh\nprintf "%s\\n" "$VERSION" >> "$FIXTURE_BUILDS"\nexit 42\n'
# mise, when present, would trust and run the fixture's tools; keep it inert.
MISE_STUB = '#!/bin/sh\nwhile [ "$1" != -- ]; do [ "$#" -gt 0 ] || exit 0; shift; done\nshift\nexec "$@"\n'


@unittest.skipUnless(shutil.which("git") and shutil.which("bash"), "git and bash are required")
class InstallLocalHead(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-install-local-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.home = self.directory / "home"
        self.home.mkdir()
        tools = self.directory / "tools"
        tools.mkdir()
        (tools / "mise").write_text(MISE_STUB)
        (tools / "mise").chmod(0o755)
        self.builds = self.directory / "builds"
        # No caller authority, credentials or live installation cross into the fixture.
        self.environment = {
            "PATH": f"{tools}{os.pathsep}{os.defpath}",
            "HOME": str(self.home),
            "XDG_DATA_HOME": str(self.home / "data"),
            "TMPDIR": str(self.directory),
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "Fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
            "FIXTURE_BUILDS": str(self.builds),
        }
        self.origin = self.directory / "origin.git"
        self.git(self.directory, "init", "--quiet", "--bare", "--initial-branch=main", str(self.origin))
        seed = self.directory / "seed"
        self.git(self.directory, "init", "--quiet", "--initial-branch=main", str(seed))
        (seed / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/install-local", seed / "scripts/install-local")
        (seed / "scripts/package-linux.sh").write_text(PACKAGE_STUB)
        (seed / "scripts/package-linux.sh").chmod(0o755)
        (seed / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.2.3"\n')
        (seed / ".gitignore").write_text(".worktrees/\n")
        self.git(seed, "add", ".")
        self.git(seed, "commit", "--quiet", "-m", "first")
        self.git(seed, "remote", "add", "origin", str(self.origin))
        self.git(seed, "push", "--quiet", "origin", "main")
        self.git(self.origin, "symbolic-ref", "HEAD", "refs/heads/main")
        self.checkout = self.directory / "checkout"
        self.git(self.directory, "clone", "--quiet", str(self.origin), str(self.checkout))
        self.publisher = seed

    def git(self, cwd, *arguments):
        return subprocess.run(["git", *arguments], cwd=cwd, env=self.environment, check=True,
                              capture_output=True, text=True).stdout.strip()

    def publish(self, message):
        """A commit that lands on origin/main after this checkout last fetched it."""
        (self.publisher / "Cargo.toml").write_text(
            (self.publisher / "Cargo.toml").read_text() + f"# {message}\n")
        self.git(self.publisher, "commit", "--quiet", "-am", message)
        self.git(self.publisher, "push", "--quiet", "origin", "main")

    def commit(self, message):
        (self.checkout / "notes").write_text(message)
        self.git(self.checkout, "add", "notes")
        self.git(self.checkout, "commit", "--quiet", "-m", message)

    def run_script(self, *arguments):
        return subprocess.run(["bash", str(self.checkout / "scripts/install-local"), *arguments],
                              cwd=self.checkout, env=self.environment, capture_output=True,
                              text=True, timeout=60)

    def short(self, revision="HEAD"):
        return self.git(self.checkout, "rev-parse", "--short=9", revision)

    def assert_built(self, result):
        # The packaging stub stops the build after the head was accepted.
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertEqual(self.builds.read_text(), f"1.2.3+local.{self.short()}\n")

    def assert_refused(self, result, *lines):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        for line in lines:
            self.assertIn(line + "\n", result.stderr)
        self.assertIn(f"To build {self.short()} anyway, add --this-commit.\n", result.stderr)
        self.assertFalse(self.builds.exists(), "a refused head was built")

    def test_current_main_is_built(self):
        result = self.run_script()
        self.assert_built(result)
        self.assertIn(f"Building main at {self.short()}.\n", result.stdout)

    def test_stale_detached_head_is_refused_with_the_update_command(self):
        stale = self.short()
        self.publish("second")
        self.publish("third")
        self.git(self.checkout, "checkout", "--quiet", "--detach")
        result = self.run_script("--build-host", "builder")
        self.assert_refused(
            result,
            f"install-local: HEAD is detached at {stale} (2 commits behind origin/main).",
            "To build main, run: git checkout main && git pull --ff-only",
        )

    def test_detached_head_points_at_the_worktree_that_has_main(self):
        self.git(self.checkout, "worktree", "add", "--quiet", "--detach", ".worktrees/review")
        review = self.checkout / ".worktrees/review"
        self.publish("second")
        result = subprocess.run(["bash", str(review / "scripts/install-local"), "--stage-only"],
                                cwd=review, env=self.environment, capture_output=True,
                                text=True, timeout=60)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(f"To build main, run: cd {self.checkout} && git pull --ff-only"
                      " && scripts/install-local --stage-only\n", result.stderr)
        self.assertFalse(self.builds.exists())

    def test_main_worktree_is_found_when_the_worktree_list_outgrows_a_pipe(self):
        # The lookup once stopped reading at its first match. Once the list
        # outgrew a pipe buffer, git died of SIGPIPE, and pipefail with set -e
        # ended the script without a word.
        deep = Path(*(["d" * 200] * 15))
        for index in range(30):
            self.git(self.checkout, "worktree", "add", "--quiet", "--detach",
                     str(Path(".worktrees") / f"w{index}" / deep))
        listing = self.git(self.checkout, "worktree", "list", "--porcelain")
        self.assertGreater(len(listing.encode()), 65536, "the listing must outgrow a pipe buffer")
        review = self.checkout / ".worktrees/w0" / deep
        self.publish("second")
        result = subprocess.run(["bash", str(review / "scripts/install-local")],
                                cwd=review, env=self.environment, capture_output=True,
                                text=True, timeout=60)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(f"To build main, run: cd {self.checkout} && git pull --ff-only"
                      " && scripts/install-local\n", result.stderr)
        self.assertFalse(self.builds.exists())

    def test_detached_head_is_built_when_chosen(self):
        self.publish("second")
        self.git(self.checkout, "checkout", "--quiet", "--detach")
        result = self.run_script("--this-commit")
        self.assert_built(result)
        self.assertIn(f"Building detached HEAD {self.short()}.\n", result.stdout)

    def test_detached_head_at_the_published_tip_is_built(self):
        self.publish("second")
        self.git(self.checkout, "fetch", "--quiet")
        self.git(self.checkout, "checkout", "--quiet", "--detach", "origin/main")
        result = self.run_script()
        self.assert_built(result)
        self.assertIn(f"Building detached HEAD {self.short()}, the tip of origin/main.\n",
                      result.stdout)

    def test_branch_behind_its_upstream_is_refused_after_fetching(self):
        # The checkout has not fetched the new commit; the script must.
        self.publish("second")
        result = self.run_script()
        self.assert_refused(result, "install-local: main is 1 commit behind origin/main.",
                            "To update it, run: git pull --ff-only")

    def test_diverged_branch_is_refused(self):
        self.publish("second")
        self.commit("local")
        result = self.run_script()
        self.assert_refused(
            result, "install-local: main has diverged from origin/main (1 ahead, 1 behind).",
            "Bring in origin/main first, for example with: git pull --rebase")

    def test_branch_behind_its_upstream_is_built_when_chosen(self):
        self.publish("second")
        self.assert_built(self.run_script("--this-commit"))

    def test_unpublished_branch_is_built_and_named(self):
        self.git(self.checkout, "checkout", "--quiet", "-b", "integration/candidate")
        self.commit("candidate")
        result = self.run_script()
        self.assert_built(result)
        self.assertIn(f"Building branch integration/candidate at {self.short()}, not main.\n",
                      result.stdout)


if __name__ == "__main__":
    unittest.main()
