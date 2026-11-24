#!/usr/bin/python3
"""Contracts of tests/fixtures/work-board-projects.py and the offline forge's
spent GraphQL quota, run in a stand-in fixture.

How the board groups the projects' problems is checked in Rust, beside the
view: work_ui::tests.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tests" / "fixtures" / "work-board-projects.py"
FORGE = ROOT / "tests" / "fixtures" / "fixture-gh.sh"
QUIET_GIT = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}


class WorkBoardProjects(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="work-board-projects-")
        self.fixture = Path(self.directory.name).resolve() / "fixture"
        for name in ("bin", "home/.config"):
            (self.fixture / name).mkdir(parents=True)
        shutil.copy(FORGE, self.fixture / "bin" / "gh")
        self.config = self.fixture / "home" / ".config"

    def tearDown(self):
        self.directory.cleanup()

    def git(self, directory, *arguments):
        return subprocess.run(["git", "-C", str(directory), *arguments], check=True, text=True,
                              stdout=subprocess.PIPE, env={**os.environ, **QUIET_GIT}).stdout.strip()

    def environment(self, **extra):
        return {"PATH": f"{self.fixture / 'bin'}:/usr/bin:/bin", "HOME": str(self.fixture / "home"),
                "CAGENTS_EVIDENCE_FIXTURE": str(self.fixture), **extra}

    def helper(self, *arguments, environment=None, check=True):
        return subprocess.run(["/usr/bin/python3", "-I", str(HELPER), *arguments], text=True,
                              capture_output=True, check=check,
                              env=self.environment() if environment is None else environment)

    def gh(self, project, *arguments, check=True):
        return subprocess.run(["gh", *arguments], cwd=project, check=check, text=True,
                              capture_output=True, env=self.environment())

    def authored(self, project):
        """The Work board's listing query, as the app sends it."""
        name = project.name
        return self.gh(project, "api", "graphql", "--include", "--hostname", "github.com",
                       "-f", "query=query CagentsAuthoredPullRequests{viewer{login}}",
                       "-f", f"open=repo:octo-org/{name} is:pr author:@me sort:updated-desc is:open",
                       "-f", f"all=repo:octo-org/{name} is:pr author:@me sort:updated-desc",
                       "-F", "openLimit=50", "-F", "allLimit=100", check=False)

    def test_it_refuses_outside_a_native_evidence_fixture(self):
        missing = self.helper(environment={"PATH": "/usr/bin:/bin", "HOME": str(self.fixture / "home")},
                              check=False)
        self.assertEqual(missing.returncode, 2)
        self.assertIn("run it in a native-evidence fixture", missing.stderr)
        elsewhere = self.helper(environment=self.environment(HOME=self.directory.name), check=False)
        self.assertEqual(elsewhere.returncode, 2)
        self.assertIn("not a native-evidence fixture", elsewhere.stderr)
        self.assertFalse((self.config / "fixture-forge-prs.json").exists())

    def test_fourteen_github_projects_with_worktrees_and_their_pull_requests(self):
        (self.config / "fixture-forge-prs.json").write_text(json.dumps([{
            "number": 7, "url": "https://github.com/octo-org/fixture-app/pull/7", "title": "Kept",
            "state": "OPEN", "headRefName": "main", "baseRefName": "main", "updatedAt": "2026-11-01T00:00:00Z",
            "headRepositoryOwner": {"login": "octo-org"}, "headRepository": {"name": "fixture-app"}}]))
        projects = [Path(path) for path in json.loads(self.helper().stdout)["projects"]]
        self.assertEqual(len(projects), 14)
        for index, project in enumerate(projects):
            self.assertEqual(self.git(project, "remote", "get-url", "origin"),
                             f"https://github.com/octo-org/{project.name}")
            worktrees = self.git(project, "worktree", "list", "--porcelain").count("worktree ")
            self.assertEqual(worktrees, 2, project)
            self.assertEqual(self.git(project, "status", "--porcelain"), "")
            listed = json.loads(self.gh(project, "pr", "list", "--author", "@me", "--json",
                                        "number,headRefOid,headRefName").stdout)
            if index % 2:
                self.assertEqual(listed, [])
            else:
                [row] = listed
                self.assertEqual(row["headRefOid"], self.git(project, "rev-parse", row["headRefName"]))
        seeds = json.loads((self.config / "fixture-forge-prs.json").read_text())
        self.assertIn("https://github.com/octo-org/fixture-app/pull/7", [row["url"] for row in seeds])
        # Again: the same projects and seeds.
        self.helper()
        self.assertEqual(json.loads((self.config / "fixture-forge-prs.json").read_text()), seeds)

    def test_a_spent_graphql_quota_fails_as_github_s_and_leaves_rest(self):
        project = Path(json.loads(self.helper().stdout)["projects"][0])
        self.assertEqual(self.authored(project).returncode, 0)
        reset = json.loads(self.helper("rate-limit").stdout)["reset"]
        self.assertGreater(reset, time.time())
        self.assertEqual(reset % 3600, 0)
        limited = self.authored(project)
        self.assertEqual(limited.returncode, 1)
        self.assertIn("X-Ratelimit-Remaining: 0\n", limited.stdout)
        self.assertIn(f"X-Ratelimit-Reset: {reset}\n", limited.stdout)
        self.assertIn("X-Ratelimit-Resource: graphql\n", limited.stdout)
        self.assertIn('"type":"RATE_LIMITED"', limited.stdout)
        self.assertIn("API rate limit already exceeded", limited.stderr)
        # REST is a quota of its own.
        rest = self.gh(project, "api", "repos/octo-org/api-gateway/issues?creator=fixture&per_page=20")
        self.assertEqual(json.loads(rest.stdout)[0]["number"], 100)
        # A reset in the past, or the switch off, gives the quota back.
        (self.config / "fixture-forge-rate-limited").write_text("1\n")
        self.assertEqual(self.authored(project).returncode, 0)
        self.helper("rate-limit")
        self.helper("rate-limit", "off")
        self.assertEqual(self.authored(project).returncode, 0)
        (self.config / "fixture-forge-rate-limited").write_text("soon\n")
        refused = self.authored(project)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("must hold the Unix time", refused.stderr)


if __name__ == "__main__":
    unittest.main()
