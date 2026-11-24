#!/usr/bin/python3
"""Contracts of tests/fixtures/conversation-work.py, run in a stand-in fixture.

The daemon's reading of its frames is checked in Rust, beside the transcript
reader: host::work::transcript::tests.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tests" / "fixtures" / "conversation-work.py"
FORGE = ROOT / "tests" / "fixtures" / "fixture-gh.sh"
QUIET_GIT = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}


class ConversationWork(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="conversation-work-")
        self.fixture = Path(self.directory.name).resolve() / "fixture"
        for name in ("bin", "home/.claude", "home/.config", "project"):
            (self.fixture / name).mkdir(parents=True)
        shutil.copy(FORGE, self.fixture / "bin" / "gh")
        project = self.fixture / "project"
        # As native-evidence makes it: one empty commit on main.
        self.git(project, "init", "--quiet", "--initial-branch=main")
        self.git(project, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "--quiet", "--allow-empty", "--message", "Fixture project")

    def tearDown(self):
        self.directory.cleanup()

    def git(self, directory, *arguments):
        return subprocess.run(["git", "-C", str(directory), *arguments], check=True, text=True,
                              stdout=subprocess.PIPE, env={**os.environ, **QUIET_GIT}).stdout.strip()

    def environment(self, **extra):
        return {"PATH": f"{self.fixture / 'bin'}:/usr/bin:/bin", "HOME": str(self.fixture / "home"),
                "CAGENTS_EVIDENCE_FIXTURE": str(self.fixture), **extra}

    def helper(self, environment=None, check=True):
        return subprocess.run(["/usr/bin/python3", "-I", str(HELPER)], text=True, capture_output=True,
                              check=check, env=self.environment() if environment is None else environment)

    def gh(self, *arguments):
        return json.loads(subprocess.run(["gh", *arguments], cwd=self.fixture / "project", check=True,
                                         text=True, capture_output=True, env=self.environment()).stdout)

    def test_it_refuses_outside_a_native_evidence_fixture(self):
        missing = self.helper({"PATH": "/usr/bin:/bin", "HOME": str(self.fixture / "home")}, check=False)
        self.assertEqual(missing.returncode, 2)
        self.assertIn("run it in a native-evidence fixture", missing.stderr)
        # Another HOME would put the provider files outside the fixture.
        elsewhere = self.helper(self.environment(HOME=self.directory.name), check=False)
        self.assertEqual(elsewhere.returncode, 2)
        self.assertIn("not a native-evidence fixture", elsewhere.stderr)
        self.assertFalse((self.fixture / "home" / ".claude" / "fixture-frames.jsonl").exists())

    def test_it_refuses_a_project_it_cannot_prepare(self):
        project = self.fixture / "project"
        (project / "notes.txt").write_text("someone's work\n")
        dirty = self.helper(check=False)
        self.assertEqual(dirty.returncode, 2)
        self.assertIn("has uncommitted changes", dirty.stderr)
        (project / "notes.txt").unlink()
        # A worktree without the rest, as an interrupted run leaves it.
        self.git(project, "worktree", "add", "--quiet", "-b", "fix/cache-key", str(project / ".worktrees/cache-key"))
        half = self.helper(check=False)
        self.assertEqual(half.returncode, 2)
        self.assertIn("half made", half.stderr)
        self.assertFalse((self.fixture / "home" / ".claude" / "fixture-frames.jsonl").exists())

    def test_the_project_gets_three_worktrees_the_forge_knows(self):
        self.helper()
        project = self.fixture / "project"
        listed = self.git(project, "worktree", "list", "--porcelain")
        for name, branch in (("cache-key", "fix/cache-key"), ("retry-budget", "feat/retry-budget"),
                             ("locale-notes", "docs/locale-notes")):
            self.assertRegex(listed, f"(?m)^worktree {re.escape(str(project / '.worktrees' / name))}\n"
                                     f"HEAD [0-9a-f]+\nbranch refs/heads/{re.escape(branch)}$")
        # The main checkout stays clean; the edited worktrees hold uncommitted work.
        self.assertEqual(self.git(project, "status", "--porcelain"), "")
        self.assertEqual(self.git(project / ".worktrees/cache-key", "status", "--porcelain"), "M src/cache.rs")
        self.assertEqual(self.git(project / ".worktrees/locale-notes", "status", "--porcelain"),
                         "M docs/locales.md")
        self.assertEqual(self.git(project / ".worktrees/retry-budget", "status", "--porcelain"), "")
        self.assertEqual(self.git(project, "rev-parse", "--abbrev-ref", "fix/cache-key@{upstream}"),
                         "origin/fix/cache-key")

        # The forge's own validation accepts the seeds, and each lookup answers.
        self.assertEqual(self.gh("repo", "view", "--json", "nameWithOwner,url")["nameWithOwner"],
                         "octo-org/fixture-app")
        listed = self.gh("pr", "list", "--state", "all", "--json", "number,state,headRefName,headRefOid")
        self.assertEqual([(row["number"], row["state"]) for row in listed],
                         [(41, "OPEN"), (38, "OPEN"), (35, "MERGED"), (29, "CLOSED"), (12, "MERGED")])
        heads = {row["number"]: row["headRefOid"] for row in listed}
        self.assertEqual(heads[41], self.git(project, "rev-parse", "fix/cache-key"))
        self.assertEqual(heads[38], self.git(project, "rev-parse", "feat/retry-budget"))
        checks = self.gh("pr", "view", "41", "--json", "statusCheckRollup")["statusCheckRollup"]
        self.assertEqual(sorted((check["name"], check["status"], check["conclusion"]) for check in checks),
                         [("build", "COMPLETED", "SUCCESS"), ("lint", "COMPLETED", "FAILURE"),
                          ("test", "IN_PROGRESS", "")])
        # The Work board lists only the viewer's own.
        authored = self.gh("pr", "list", "--state", "all", "--author", "@me", "--json", "number")
        self.assertEqual([row["number"] for row in authored], [41, 38, 35])

    def test_spent_graphql_quota_preserves_rest_and_expires(self):
        self.helper()
        marker = self.fixture / "home" / ".config" / "fixture-forge-graphql-reset"
        reset = int(time.time()) + 3600
        marker.write_text(f"{reset}\n")
        for arguments, output in (
            (("api", "--include", "graphql", "-f", "query=query Test { viewer { login } }"), "stdout"),
            (("pr", "view", "41", "-R", "octo-org/fixture-app", "--json", "number"), "stderr"),
            (("repo", "view", "octo-org/fixture-app", "--json", "nameWithOwner,url"), "stderr"),
        ):
            with self.subTest(arguments=arguments):
                refused = subprocess.run(["gh", *arguments], text=True, capture_output=True,
                                         env=self.environment())
                self.assertEqual(refused.returncode, 1)
                self.assertIn("GraphQL: API rate limit already exceeded", refused.stderr)
                headers = getattr(refused, output)
                self.assertIn("X-Ratelimit-Remaining: 0", headers)
                self.assertIn("X-Ratelimit-Resource: graphql", headers)
                self.assertIn(f"X-Ratelimit-Reset: {reset}", headers)
        self.assertEqual(self.gh("api", "repos/octo-org/fixture-app/releases"), [])
        marker.write_text("1\n")
        self.assertEqual(self.gh("pr", "view", "41", "--json", "number"), {"number": 41})

    def test_the_frames_name_this_fixture_s_paths_and_commits(self):
        self.helper()
        project = self.fixture / "project"
        text = (self.fixture / "home" / ".claude" / "fixture-frames.jsonl").read_text()
        frames = [json.loads(line) for line in text.splitlines()]
        self.assertNotIn("@PROJECT@", text)
        self.assertNotIn("@RETRY_", text)
        self.assertEqual(frames[-1], {"_end": True})
        self.assertEqual(frames[-2]["type"], "result")
        calls = [block for frame in frames if frame.get("type") == "assistant"
                 for block in frame["message"]["content"] if block["type"] == "tool_use"]
        results = {block["tool_use_id"] for frame in frames if frame.get("type") == "user"
                   for block in frame["message"]["content"]}
        self.assertEqual({call["id"] for call in calls}, results, "every call has its result")
        for call in calls:
            if call["name"] == "Edit":
                path = Path(call["input"]["file_path"])
                self.assertTrue(path.is_relative_to(project / ".worktrees"), path)
                # The edit is what the worktree holds now.
                self.assertIn(call["input"]["new_string"], path.read_text())
        retry = (self.git(project, "rev-parse", "--short=7", "feat/retry-budget~1"),
                 self.git(project, "rev-parse", "--short=7", "feat/retry-budget"))
        self.assertIn(f"{retry[0]}..{retry[1]}  feat/retry-budget -> feat/retry-budget", text)

    def test_running_it_again_keeps_the_worktrees(self):
        self.helper()
        project = self.fixture / "project"
        before = self.git(project, "rev-parse", "main", "fix/cache-key", "feat/retry-budget")
        (self.fixture / "home" / ".config" / "fixture-forge-prs.json").write_text("[]\n")
        frames = self.fixture / "home" / ".claude" / "fixture-frames.jsonl"
        written = frames.read_text()
        frames.write_text("")
        self.helper()
        self.assertEqual(frames.read_text(), written)
        self.assertEqual(self.git(project, "rev-parse", "main", "fix/cache-key", "feat/retry-budget"), before)
        self.assertEqual(len(self.gh("pr", "list", "--state", "all", "--json", "number")), 5)
        self.assertEqual(self.git(project, "remote", "get-url", "origin"),
                         "https://github.com/octo-org/fixture-app")


if __name__ == "__main__":
    unittest.main()
