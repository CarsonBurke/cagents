#!/usr/bin/python3
"""Contracts of tests/fixtures/offline-review-cache.py, run in a stand-in fixture.

The stand-in store has the review cache tables exactly as src/persistence.rs
creates them. Whether the app reads the record back is shown natively: the
review opens with the cached files and the restored draft.
"""
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tests" / "fixtures" / "offline-review-cache.py"
FORGE = ROOT / "tests" / "fixtures" / "fixture-gh.sh"
QUIET_GIT = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
SELECTOR = "github.com/octo-org/fixture-app"


def review_cache_schema():
    """The CREATE TABLE statements for review_cache and review_cache_files."""
    source = (ROOT / "src" / "persistence.rs").read_text()
    match = re.search(r'"(CREATE TABLE review_cache \(.*?\) STRICT;\s*CREATE TABLE review_cache_files \(.*?\) STRICT;)"',
                      source, re.DOTALL)
    assert match, "src/persistence.rs no longer creates the review cache tables in one batch"
    return match.group(1)


class OfflineReviewCache(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="offline-review-cache-")
        self.fixture = Path(self.directory.name).resolve() / "fixture"
        for name in ("bin", "home/.config", "project", "data/cagents-development"):
            (self.fixture / name).mkdir(parents=True)
        shutil.copy(FORGE, self.fixture / "bin" / "gh")
        project = self.fixture / "project"
        # As native-evidence makes it: one empty commit on main.
        self.git(project, "init", "--quiet", "--initial-branch=main")
        self.git(project, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "--quiet", "--allow-empty", "--message", "Fixture project")
        self.store = self.fixture / "data" / "cagents-development" / "sessions.sqlite3"

    def tearDown(self):
        self.directory.cleanup()

    def make_store(self):
        connection = sqlite3.connect(self.store)
        connection.execute("PRAGMA journal_mode = WAL")
        connection.executescript(review_cache_schema())
        connection.close()

    def git(self, directory, *arguments):
        return subprocess.run(["git", "-C", str(directory), *arguments], check=True, text=True,
                              stdout=subprocess.PIPE, env={**os.environ, **QUIET_GIT}).stdout.strip()

    def environment(self, **extra):
        return {"PATH": f"{self.fixture / 'bin'}:/usr/bin:/bin", "HOME": str(self.fixture / "home"),
                "XDG_DATA_HOME": str(self.fixture / "data"), "CAGENTS_EVIDENCE_FIXTURE": str(self.fixture),
                "CAGENTS_DEVELOPMENT_INSTANCE": "", **extra}

    def helper(self, environment=None, check=True):
        return subprocess.run(["/usr/bin/python3", "-I", str(HELPER)], text=True, capture_output=True,
                              check=check, env=self.environment() if environment is None else environment)

    def gh(self, *arguments):
        return json.loads(subprocess.run(["gh", *arguments], cwd=self.fixture / "project", check=True,
                                         text=True, capture_output=True, env=self.environment()).stdout)

    def rows(self):
        connection = sqlite3.connect(self.store)
        try:
            reviews = connection.execute(
                "SELECT repository, number, head, viewer, record, byte_size, write_epoch FROM review_cache").fetchall()
            files = connection.execute(
                "SELECT path, position, entry, viewed, unverified, head FROM review_cache_files"
                " WHERE repository = ? AND number = 57 ORDER BY position", (SELECTOR,)).fetchall()
        finally:
            connection.close()
        return reviews, files

    def test_it_refuses_outside_a_fixture_or_before_the_app_made_its_store(self):
        missing = self.helper({"PATH": "/usr/bin:/bin", "HOME": str(self.fixture / "home")}, check=False)
        self.assertEqual(missing.returncode, 2)
        self.assertIn("run it in a native-evidence fixture", missing.stderr)
        elsewhere = self.helper(self.environment(HOME=self.directory.name), check=False)
        self.assertEqual(elsewhere.returncode, 2)
        self.assertIn("not a native-evidence fixture", elsewhere.stderr)
        early = self.helper(check=False)
        self.assertEqual(early.returncode, 2)
        self.assertIn("start a session", early.stderr)
        # Nothing was committed or seeded before the refusal.
        self.assertEqual(self.git(self.fixture / "project", "rev-list", "--count", "HEAD"), "1")
        self.assertFalse((self.fixture / "home" / ".config" / "fixture-forge-prs.json").exists())

    def test_the_cached_review_names_local_objects_and_the_forge_s_pull_request(self):
        self.make_store()
        printed = json.loads(self.helper().stdout)
        project = self.fixture / "project"
        worktree = project / ".worktrees" / "cache-key"
        self.assertEqual(self.git(worktree, "rev-parse", "--abbrev-ref", "HEAD"), "fix/cache-key")
        self.assertEqual(self.git(project, "status", "--porcelain"), "")
        for oid in (printed["head"], printed["base"], printed["merge_base"]):
            self.assertEqual(self.git(project, "cat-file", "-t", oid), "commit")
        self.assertEqual(self.git(project, "rev-parse", "refs/cagents/pull/57/head"), printed["head"])

        # The forge lists it as the worktree's pull request, at the cached head.
        self.assertEqual(self.gh("repo", "view", "--json", "nameWithOwner,url")["nameWithOwner"],
                         "octo-org/fixture-app")
        listed = self.gh("pr", "list", "--json", "number,headRefName,headRefOid")
        self.assertEqual(listed, [{"number": 57, "headRefName": "fix/cache-key", "headRefOid": printed["head"]}])

        reviews, files = self.rows()
        self.assertEqual(len(reviews), 1)
        repository, number, head, viewer, record, byte_size, epoch = reviews[0]
        self.assertEqual((repository, number, head, viewer), (SELECTOR, 57, printed["head"], "fixture"))
        self.assertFalse(epoch.startswith("boottime-v1:"), "the app's next save must supersede the seed")
        snapshot = json.loads(record)
        self.assertEqual(snapshot["files"], [])
        self.assertEqual(snapshot["pull_request"]["headRefOid"], snapshot["range"]["head"])
        self.assertEqual(bytes(snapshot["selected"]).decode(), "src/cache.rs")
        composer = snapshot["drafts"]["composer"]
        anchor = composer["destination"]["Line"]
        self.assertEqual(anchor["path"], "src/cache.rs")
        shown = self.git(project, "show", f"{printed['head']}:src/cache.rs").splitlines()
        self.assertIn("{locale}:{path}", shown[anchor["line"] - 1])
        self.assertTrue(composer["body"])
        self.assertEqual(byte_size, len(record.encode()) + sum(len(row[2].encode()) for row in files))
        self.assertEqual([(row[0], row[1], row[3], row[5]) for row in files],
                         [("docs/caching.md", 0, 1, printed["head"]), ("src/cache.rs", 1, 0, printed["head"])])
        entries = [json.loads(row[2]) for row in files]
        self.assertEqual([(bytes(entry["path"]).decode(), entry["kind"]) for entry in entries],
                         [("docs/caching.md", "added"), ("src/cache.rs", "modified")])

    def test_running_it_again_replaces_the_cache_and_keeps_other_seeds(self):
        self.make_store()
        other = {"number": 12, "url": "https://github.com/octo-org/fixture-app/pull/12", "title": "Other",
                 "state": "OPEN", "headRefName": "other", "baseRefName": "main", "updatedAt": "2026-11-01T00:00:00Z",
                 "headRepositoryOwner": {"login": "octo-org"}, "headRepository": {"name": "fixture-app"}}
        (self.fixture / "home" / ".config" / "fixture-forge-prs.json").write_text(json.dumps([other]))
        first = json.loads(self.helper().stdout)
        second = json.loads(self.helper().stdout)
        self.assertEqual(first["head"], second["head"])
        reviews, files = self.rows()
        self.assertEqual(len(reviews), 1)
        self.assertEqual(len(files), 2)
        seeds = json.loads((self.fixture / "home" / ".config" / "fixture-forge-prs.json").read_text())
        self.assertEqual([seed["number"] for seed in seeds], [12, 57])


if __name__ == "__main__":
    unittest.main()
