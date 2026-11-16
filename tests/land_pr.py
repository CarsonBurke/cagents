#!/usr/bin/python3
"""Land pull requests against disposable Git history with fake GitHub reads; no network or Rust."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
loader = importlib.machinery.SourceFileLoader("land_pr", str(ROOT / "scripts/land-pr"))
spec = importlib.util.spec_from_loader(loader.name, loader)
land = importlib.util.module_from_spec(spec)
loader.exec_module(land)

# Answers `gh api` from the fixture's bare origin and records every call.
FAKE_GH = r'''#!/usr/bin/python3
import json, os, subprocess, sys
from pathlib import Path
root = Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
with (root / "gh-calls").open("a") as out:
    out.write(json.dumps(args) + "\n")
def origin(*command):
    return subprocess.check_output(["git", "-C", str(root / "origin.git"), *command], text=True)
pr = json.loads((root / "pr.json").read_text())
branch = pr["head"]["ref"]
head = origin("rev-parse", "refs/heads/" + branch).strip()
if args[:3] == ["api", "-X", "PUT"]:
    fields = dict(value.split("=", 1) for value in args[5::2])
    if (root / "merge-error").exists():
        print((root / "merge-error").read_text(), file=sys.stderr)
        sys.exit(1)
    (root / "merged.json").write_text(json.dumps(fields))
    if fields["sha"] != head:
        print("gh: Head branch was modified. Review and try the merge again. (HTTP 409)", file=sys.stderr)
        sys.exit(1)
    print(json.dumps({"sha": "f" * 40, "merged": True}))
elif args[1].endswith("/commits?per_page=100&page=1"):
    log = origin("log", "--reverse", "--format=%P%x00%B%x01", "refs/heads/main.." + head)
    commits = [entry.lstrip("\n").split("\0", 1) for entry in log.split("\1") if entry.strip()]
    print(json.dumps([{"commit": {"message": message.rstrip("\n")},
                       "parents": [{"sha": sha} for sha in parents.split()]}
                      for parents, message in commits]))
elif "/commits?" in args[1]:
    print("[]")
else:
    pr["head"]["sha"] = head
    print(json.dumps(pr))
'''

# Stands in for scripts/gh-pr-status: the next queued answer, or the last one.
FAKE_STATUS = r'''#!/usr/bin/python3
import json, os, subprocess, sys, time
from pathlib import Path
root = Path(os.environ["FIXTURE_ROOT"])
queue = json.loads((root / "status.json").read_text())
answer = queue.pop(0) if len(queue) > 1 else queue[0]
(root / "status.json").write_text(json.dumps(queue))
with (root / "status-calls").open("a") as out:
    out.write(json.dumps(sys.argv[1:]) + "\n")
if answer.get("dirty"):  # GitHub's REST view agrees from now on.
    pr = json.loads((root / "pr.json").read_text())
    (root / "pr.json").write_text(json.dumps({**pr, "mergeable_state": "dirty"}))
if answer.get("fail"):
    print(answer["fail"], file=sys.stderr)
    sys.exit(4)
branch = json.loads((root / "pr.json").read_text())["head"]["ref"]
head = subprocess.check_output(["git", "-C", str(root / "origin.git"), "rev-parse",
                                "refs/heads/" + branch], text=True).strip()
pr = {"state": answer.get("state", "OPEN"), "isDraft": False, "headRefOid": answer.get("head", head),
      "mergeable": answer.get("mergeable", "MERGEABLE")}
checks = answer.get("checks", [{"__typename": "CheckRun", "name": "contracts",
                                "status": "COMPLETED", "conclusion": "SUCCESS"}])
print(json.dumps({"ok": True, "data": {
    "snapshot": {"pull_request": pr, "checks_state": answer.get("ci", "SUCCESS"), "checks": checks,
                 "checks_complete": answer.get("complete", True)},
    "fetched_at": int(time.time()) + answer.get("age", 0), "retry_at": answer.get("retry_at"),
    "error": None},
    "limitations": answer.get("limitations", [])}))
'''


class LandTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="land-pr-", dir="/var/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.origin = self.root / "origin.git"
        self.repo = self.root / "repo"
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text(FAKE_GH)
        (bin_dir / "gh").chmod(0o755)
        environment = {"PATH": f"{bin_dir}:/usr/bin:/bin", "FIXTURE_ROOT": str(self.root),
                       "HOME": str(self.root), "GIT_CONFIG_NOSYSTEM": "1",
                       "GIT_CONFIG_GLOBAL": str(self.root / "gitconfig"),
                       "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                       "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
        # Nothing from an enclosing Git hook or session reaches the fixture's repositories.
        inherited = {name: value for name, value in os.environ.items() if not name.startswith("GIT_")}
        environ = patch.dict(os.environ, {**inherited, **environment}, clear=True)
        environ.start()
        self.addCleanup(environ.stop)
        self.sleeps = []
        sleeper = patch.object(land, "sleep", self.sleeps.append)
        sleeper.start()
        self.addCleanup(sleeper.stop)

        self.git(self.root, "init", "-q", "--bare", "-b", "main", str(self.origin))
        self.git(self.root, "clone", "-q", str(self.origin), str(self.repo))
        (self.repo / ".gitignore").write_text("/.worktrees/\n")
        (self.repo / "scripts").mkdir()
        status = self.repo / "scripts/gh-pr-status"
        status.write_text(FAKE_STATUS)
        status.chmod(0o755)
        (self.repo / "shared.txt").write_text("one\n")
        self.commit("baseline")
        self.git(self.repo, "push", "-q", "origin", "main")
        self.git(self.repo, "switch", "-q", "-c", "feature")
        (self.repo / "feature.txt").write_text("feature\n")
        self.commit("feat: add a feature\n\nWhy it matters.\n\n"
                    "Co-Authored-By: Claude Opus <noreply@anthropic.com>")
        self.git(self.repo, "push", "-q", "-u", "origin", "feature")
        self.pr()
        self.status({})

    def git(self, where, *args):
        return subprocess.run(["git", "-C", str(where), *args], check=True, capture_output=True,
                              text=True).stdout.strip()

    def commit(self, message, where=None):
        where = where or self.repo
        self.git(where, "add", "-A")
        self.git(where, "commit", "-q", "--allow-empty", "-m", message)
        return self.git(where, "rev-parse", "HEAD")

    def pr(self, **changes):
        pr = {"number": 7, "title": "Add a feature", "state": "open", "merged": False, "draft": False,
              "mergeable_state": "clean", "head": {"ref": "feature", "repo": {"full_name": "o/r"}},
              "base": {"ref": "main", "repo": {"full_name": "o/r"}}}
        pr.update(changes)
        (self.root / "pr.json").write_text(json.dumps(pr))

    def status(self, *answers):
        (self.root / "status.json").write_text(json.dumps(list(answers)))

    def advance_main(self, path="shared.txt", text="two\n"):
        """Land an unrelated or conflicting change on origin's main."""
        other = self.root / "other"
        if not other.exists():
            self.git(self.root, "clone", "-q", str(self.origin), str(other))
        self.git(other, "pull", "-q", "origin", "main")
        (other / path).write_text(text)
        self.commit("chore: move main", other)
        self.git(other, "push", "-q", "origin", "main")

    def run_land(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = land.main(["7", "-R", "o/r", *args], root=self.repo)
        self.output = out.getvalue() + err.getvalue()
        return status

    def merged(self):
        path = self.root / "merged.json"
        return json.loads(path.read_text()) if path.exists() else None

    def remote_head(self):
        return self.git(self.origin, "rev-parse", "refs/heads/feature")

    def test_green_head_lands_pinned_with_clean_message_and_no_push(self):
        head = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")  # Behind main, but no conflict: not re-synced.
        self.assertEqual(self.run_land(), 0, self.output)
        merged = self.merged()
        self.assertEqual(merged["sha"], head)
        self.assertEqual(merged["merge_method"], "squash")
        self.assertEqual(merged["commit_title"], "Add a feature (#7)")
        self.assertEqual(merged["commit_message"], "* feat: add a feature\n\nWhy it matters.")
        self.assertEqual(self.remote_head(), head)
        self.assertIn("landed #7", self.output)

    def test_waits_through_pending_and_unknown_checks(self):
        self.status({"ci": "PENDING"}, {"ci": None, "limitations": ["CI unknown"]},
                    {"ci": "SUCCESS", "checks": [{"__typename": "CheckRun", "name": "a",
                                                  "status": "IN_PROGRESS", "conclusion": None}]},
                    {"ci": "SUCCESS"})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(len(self.sleeps), 3)
        self.assertTrue(all(seconds >= land.REFRESH for seconds in self.sleeps), self.sleeps)

    def test_ci_failure_names_the_check_and_does_not_merge(self):
        self.status({"ci": "FAILURE", "checks": [{"__typename": "CheckRun", "name": "contracts",
                                                  "status": "COMPLETED", "conclusion": "FAILURE"}]})
        self.assertEqual(self.run_land(), land.CI_FAILED)
        self.assertIn("contracts", self.output)
        self.assertIsNone(self.merged())

    def test_timeout_when_ci_never_finishes(self):
        self.status({"ci": "PENDING"})
        clock = iter(range(0, 100000, 1000))
        with patch.object(land.time, "time", lambda: next(clock)):
            self.assertEqual(self.run_land("--timeout", "1"), land.TIMED_OUT)
        self.assertIsNone(self.merged())

    def test_head_moving_while_waiting_stops(self):
        self.status({"head": "a" * 40, "age": -1000}, {"head": "b" * 40, "age": 5})
        self.assertEqual(self.run_land(), land.HEAD_MOVED)
        self.assertEqual(len(self.sleeps), 1)  # The read from before this run was not trusted.
        self.assertIsNone(self.merged())

    def test_merge_refused_when_head_changed_under_it(self):
        original = land.merge

        def racing_merge(*args):
            (self.repo / "late.txt").write_text("late\n")
            self.git(self.repo, "switch", "-q", "--detach")
            other = self.commit("late push")
            self.git(self.repo, "push", "-q", "origin", f"{other}:refs/heads/feature")
            return original(*args)

        with patch.object(land, "merge", racing_merge):
            self.assertEqual(self.run_land(), land.HEAD_MOVED)

    def test_local_conflict_is_reported_without_pushing(self):
        (self.repo / "shared.txt").write_text("branch\n")
        self.commit("change shared")
        self.git(self.repo, "push", "-q", "origin", "feature")
        head = self.remote_head()
        self.advance_main()
        self.assertEqual(self.run_land(), land.CONFLICT)
        self.assertIn("shared.txt", self.output)
        self.assertEqual(self.remote_head(), head)

    def test_github_conflict_merges_main_in_privately_and_lands_that_head(self):
        old = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")
        self.pr(mergeable_state="dirty")
        self.assertEqual(self.run_land(), 0, self.output)
        new = self.remote_head()
        self.assertNotEqual(new, old)
        self.assertEqual(self.git(self.origin, "rev-parse", f"{new}^1"), old)  # A merge, not a rebase.
        self.assertEqual(self.merged()["sha"], new)
        self.assertNotIn("Merge origin/main", self.merged()["commit_message"])
        self.assertFalse((self.repo / ".worktrees/land-pr-7").exists())
        self.assertEqual(self.git(self.repo, "status", "--porcelain"), "")
        self.assertEqual(self.git(self.repo, "branch", "--show-current"), "feature")

    def test_update_on_request_and_plain_push_does_not_overwrite(self):
        self.advance_main("unrelated.txt", "x\n")
        original = land.push

        def raced(root, branch, commit):
            other = self.root / "racer"
            self.git(self.root, "clone", "-q", "-b", "feature", str(self.origin), str(other))
            self.commit("someone else's push", other)
            self.git(other, "push", "-q", "origin", "feature")
            return original(root, branch, commit)

        with patch.object(land, "push", raced):
            self.assertEqual(self.run_land("--update"), land.HEAD_MOVED)
        self.assertEqual(self.git(self.origin, "log", "-1", "--format=%s", "refs/heads/feature"),
                         "someone else's push")
        self.assertIsNone(self.merged())

    def test_conflict_reported_while_waiting_merges_main_again(self):
        self.advance_main("unrelated.txt", "x\n")
        self.status({"mergeable": "CONFLICTING", "dirty": True}, {})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.sleeps[:1], [land.REFRESH], self.output)
        self.assertEqual(self.git(self.origin, "rev-list", "--count", "--merges", "refs/heads/feature"), "1")

    def test_refusals(self):
        cases = [({"draft": True}, "draft"), ({"state": "closed"}, "closed"),
                 ({"merged": True}, "already merged"),
                 ({"head": {"ref": "feature", "repo": {"full_name": "fork/r"}}}, "another repository"),
                 ({"base": {"ref": "release", "repo": {"full_name": "o/r"}}}, "targets release")]
        for change, message in cases:
            with self.subTest(message):
                self.pr(**change)
                self.assertEqual(self.run_land(), land.REFUSED)
                self.assertIn(message, self.output)
        self.assertIsNone(self.merged())

    def test_refuses_dirty_or_unpushed_local_branch(self):
        (self.repo / "feature.txt").write_text("edited\n")
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("uncommitted", self.output)
        self.git(self.repo, "checkout", "--", "feature.txt")
        self.commit("local only")
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("commits the PR does not", self.output)
        self.assertIsNone(self.merged())

    def test_refuses_surviving_attribution_and_strips_known_forms(self):
        title, body = land.squash_message(3, "Title", [
            ("fix: a\n\nBody\n\n\n\nCo-authored-by: GPT-5 <x@openai.com>\n"
             "\U0001f916 Generated with [Claude Code](https://claude.com/claude-code)\n"
             "Co-Authored-By: Codex <c@example.com>\nGenerated-by: Claude Opus\n"
             "Assisted-by: Claude Opus 4\nMade with [Claude Code](https://example.com)\n"
             "Signed-off-by: GPT-5 <bot@example.com>", 1),
            ("Merge remote-tracking branch 'origin/main' into fix/a", 2),
            ("Merge origin/main into fix/a", 2),
            ("Merge branch 'other' into fix/a", 2),
            ("Merge origin/main into fix/a\n\nResolved the parser conflict by keeping both.", 2),
            ("feat: support Codex and Claude providers\n\n"
             "The snapshot fixture is generated by the codex CLI.\n"
             "Fixture transcripts generated with `claude --print`.\n"
             "Strip Co-Authored-By: Claude trailers from squash messages.\n\n"
             "Co-authored-by: Bob <bob@gptzero.dev>\nCo-authored-by: Ada <ada@example.com>", 1)])
        self.assertEqual(title, "Title (#3)")
        self.assertEqual(body, "* fix: a\n\nBody\n\n* Merge branch 'other' into fix/a\n\n"
                               "* Merge origin/main into fix/a\n\nResolved the parser conflict by keeping both.\n\n"
                               "* feat: support Codex and Claude providers\n\n"
                               "The snapshot fixture is generated by the codex CLI.\n"
                               "Fixture transcripts generated with `claude --print`.\n"
                               "Strip Co-Authored-By: Claude trailers from squash messages.\n\n"
                               "Co-authored-by: Bob <bob@gptzero.dev>\nCo-authored-by: Ada <ada@example.com>")
        kept = ("fix: b\n\nThe rollout file is\nwritten by Codex after each turn, so we read it lazily.\n"
                "Built with Codex in mind, the adapter now batches events.\n"
                "Drop the 'Generated with Claude Code' footer from squash messages.\n\n"
                "Reviewed-by: Ada <ada@example.com>, after testing with Claude")
        self.assertEqual(land.squash_message(3, "fix: drop footers", [(kept, 1)])[1], "* " + kept)
        self.assertEqual(land.squash_message(3, "T", [(
            "fix: c\n\nAI-Assisted: Claude\nCo-authored-by: Copilot <copilot@github.com>\n"
            "Generated by Claude 4.5\nCreated with OpenAI Codex", 1)])[1], "* fix: c")
        for title, message in [("Generated with Claude Code", "fix: a"),
                               ("Title", "fix: a\n\n> Co-authored-by: Claude <bot@example.com>")]:
            with self.subTest(title), self.assertRaises(land.Stop) as refused:
                land.squash_message(3, title, [(message, 1)])
            self.assertEqual(refused.exception.status, land.REFUSED)

    def test_dry_run_pushes_and_merges_nothing(self):
        self.advance_main("unrelated.txt", "x\n")
        head = self.remote_head()
        self.assertEqual(self.run_land("--dry-run", "--update"), 0, self.output)
        self.assertEqual(self.remote_head(), head)
        self.assertIsNone(self.merged())
        self.assertIn("would push", self.output)
        self.assertIn("Add a feature (#7)", self.output)
        self.assertNotIn("noreply@anthropic.com", self.output)
        self.assertFalse((self.repo / ".worktrees/land-pr-7").exists())

    def test_concurrent_landing_of_the_same_pr_is_refused(self):
        with land.landing_lock(self.repo, "o/r", 7):
            self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("another landing", self.output)

    def test_repository_comes_from_origin_and_must_match_it(self):
        self.git(self.repo, "remote", "set-url", "origin", "git@github.com:Owner/repo.name.git")
        self.assertEqual(land.origin_repository(self.repo), "Owner/repo.name")
        self.git(self.repo, "remote", "set-url", "origin", "https://github.com/Owner/repo")
        self.assertEqual(land.origin_repository(self.repo), "Owner/repo")
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("origin is Owner/repo", self.output)

    def test_refuses_a_rebase_of_the_branch_in_any_worktree(self):
        # A rebase stopped at an `edit` step: clean and detached, so no branch is listed.
        self.git(self.repo, "-c", "sequence.editor=sed -i 1s/^pick/edit/", "rebase", "-q", "-i", "HEAD~1")
        self.addCleanup(lambda: subprocess.run(["git", "-C", str(self.repo), "rebase", "--abort"],
                                               capture_output=True))
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("is rebasing feature", self.output)

    def test_refuses_a_deleted_fork_head(self):
        self.pr(head={"ref": "feature", "repo": None})
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("deleted", self.output)

    def test_closed_or_drafted_while_waiting_is_refused(self):
        self.status({"ci": "PENDING"}, {"state": "MERGED"})
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("is merged", self.output)

    def test_status_failures_and_fallback_wait_on_the_shared_cadence(self):
        self.retry_at = int(time.time()) + 50000
        self.status({"fail": "gh-pr-status: GitHub is unavailable"},
                    {"ci": None, "checks": [], "limitations": ["Single REST bootstrap read"]},
                    {"ci": "PENDING", "retry_at": self.retry_at}, {})
        clock = [time.time()]
        with patch.object(land.time, "time", lambda: clock[0]), \
                patch.object(land, "sleep", lambda seconds: (self.sleeps.append(seconds),
                                                             clock.__setitem__(0, clock[0] + seconds))):
            self.assertEqual(self.run_land("--timeout", "1000"), 0, self.output)
        self.assertIn("GitHub is unavailable", self.output)
        self.assertIn("checks unknown", self.output)
        self.assertGreaterEqual(self.sleeps[0], land.REFRESH)  # No fetched_at: still the shared cadence.
        self.assertGreaterEqual(clock[0], self.retry_at)  # retry_at was honoured.

    def test_timeout_reports_why_it_was_still_waiting(self):
        self.status({"fail": "gh-pr-status: GitHub is unavailable"})
        clock = iter(range(0, 100000, 1000))
        with patch.object(land.time, "time", lambda: next(clock)):
            self.assertEqual(self.run_land("--timeout", "1"), land.TIMED_OUT)
        self.assertIn("GitHub is unavailable", self.output.splitlines()[-1])

    def test_a_status_read_lagging_behind_its_own_push_waits(self):
        old = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")
        self.status({"head": old, "age": 5}, {})
        self.assertEqual(self.run_land("--update"), 0, self.output)
        self.assertEqual(self.merged()["sha"], self.remote_head())
        self.assertNotEqual(self.remote_head(), old)

    def test_a_conflict_from_an_older_read_does_not_resync(self):
        head = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")
        self.status({"mergeable": "CONFLICTING", "age": -1000}, {})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.merged()["sha"], head)
        self.assertEqual(len(self.sleeps), 1)  # Its green checks were not trusted either.

    def test_dry_run_reports_the_status_the_landing_would_have(self):
        self.status({"ci": "FAILURE", "checks": [{"__typename": "CheckRun", "name": "contracts",
                                                  "status": "COMPLETED", "conclusion": "FAILURE"}]})
        self.assertEqual(self.run_land("--dry-run"), land.CI_FAILED)
        self.assertIsNone(self.merged())

    def test_dry_run_of_an_update_github_needs_reports_the_push(self):
        self.advance_main("unrelated.txt", "x\n")
        self.pr(mergeable_state="dirty")
        self.status({"mergeable": "CONFLICTING"})
        self.assertEqual(self.run_land("--dry-run"), 0, self.output)
        self.assertIn("would push it, then wait for CI", self.output)

    def test_dry_run_does_not_trust_a_read_of_an_earlier_head(self):
        self.status({"head": "a" * 40, "age": -100})
        self.assertEqual(self.run_land("--dry-run"), 0, self.output)

    def test_github_refusing_the_merge_is_a_refusal(self):
        (self.root / "merge-error").write_text("gh: Pull Request is not mergeable (HTTP 405)")
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("HTTP 405", self.output)

    def test_aggregate_success_lands_past_the_hundred_listed_checks(self):
        self.status({"complete": False})
        self.assertEqual(self.run_land(), 0, self.output)

    def test_an_empty_squash_message_is_refused(self):
        with self.assertRaises(land.Stop) as refused:
            land.squash_message(3, "Title", [("Merge origin/main into fix/a", 2),
                                             ("Co-authored-by: Claude <noreply@anthropic.com>", 1)])
        self.assertEqual(refused.exception.status, land.REFUSED)


if __name__ == "__main__":
    unittest.main()
