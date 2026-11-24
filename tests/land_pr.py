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
    queued = root / "merge-errors.json"  # One refusal per merge attempt, then success.
    errors = json.loads(queued.read_text()) if queued.exists() else []
    if errors:
        error = errors.pop(0)
        queued.write_text(json.dumps(errors))
        if error.get("dirty"):  # GitHub sees the moved main conflict from now on.
            (root / "pr.json").write_text(json.dumps({**pr, "mergeable_state": "dirty"}))
        print(error["error"], file=sys.stderr)
        sys.exit(1)
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
pr = {"state": answer.get("state", "OPEN"), "isDraft": answer.get("draft", False), "headRefOid": answer.get("head", head),
      "mergeable": answer.get("mergeable", "MERGEABLE")}
# As gh-pr-status reports them: passing checks counted, the rest listed.
checks = answer.get("checks", [])
print(json.dumps({"ok": True, "data": {
    "snapshot": {"pull_request": pr, "checks_state": answer.get("ci", "SUCCESS"), "checks": checks,
                 "checks_passed": answer.get("passed", 1),
                 "checks_complete": answer.get("complete", True),
                 "transport": answer.get("transport", "graphql")},
    "fetched_at": int(os.environ.get("FAKE_STATUS_NOW") or time.time()) + answer.get("age", 0), "retry_at": answer.get("retry_at"),
    "error": answer.get("error"), "stale": answer.get("stale", bool(answer.get("error"))),
    "limitations": answer.get("data_limitations", [])},
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

    def write_protocol(self, where, protocol, proven, recordings):
        (where / "src/terminal/ipc").mkdir(parents=True, exist_ok=True)
        (where / "src/terminal/ipc.rs").write_text(
            f"pub const PROTOCOL_VERSION: u16 = {protocol};\n")
        clients = [f"const C{index}: OlderClient = OlderClient {{\n    proven_at: {value},\n}};\n"
                   for index, value in enumerate(proven)]
        # Leave stable context between the proof main updates and an added client.
        separator = "\nconst OTHER_VERSIONS: &[u16] = &[\n" + "".join(
            f"    {version},\n" for version in range(8)) + "];\n\n"
        (where / "src/terminal/ipc/older_clients.rs").write_text(
            clients[0] + separator + "".join(clients[1:]))
        (where / "tests/fixtures/wire-formats").mkdir(parents=True, exist_ok=True)
        for recording in recordings:
            (where / f"tests/fixtures/wire-formats/{recording}.json").write_text("")
        self.commit(f"test: record protocol {protocol} and older-client proofs", where)

    def protocol_feature(self):
        self.git(self.repo, "switch", "-q", "main")
        self.write_protocol(self.repo, 5, [5], [5])
        self.git(self.repo, "push", "-q", "origin", "main")
        self.git(self.repo, "switch", "-q", "feature")
        self.git(self.repo, "merge", "-q", "--no-edit", "main")
        self.write_protocol(self.repo, 5, [5, 5], [5])
        self.git(self.repo, "push", "-q", "origin", "feature")

    def advance_main(self, path="shared.txt", text="two\n", protocol=None):
        """Land an unrelated or conflicting change on origin's main."""
        other = self.root / "other"
        if not other.exists():
            self.git(self.root, "clone", "-q", str(self.origin), str(other))
        self.git(other, "pull", "-q", "origin", "main")
        if protocol is None:
            (other / path).write_text(text)
            self.commit("chore: move main", other)
        else:
            self.write_protocol(other, *protocol)
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

    def squash_onto_moved_main(self):
        """Squash with `reset --soft` onto a main that moved since the branch began.

        The index keeps the branch's older tree, so the one commit also carries
        the older copy of everything main changed meanwhile (#3875).
        """
        self.advance_main("shared.txt", "two\n")
        self.advance_main("added.txt", "main's new file\n")
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "reset", "-q", "--soft", "origin/main")
        self.commit("feat: add a feature")
        self.git(self.repo, "push", "-q", "-f", "origin", "feature")

    def test_a_branch_squashed_onto_a_moved_main_is_refused_for_what_it_reverts(self):
        self.squash_onto_moved_main()
        self.assertEqual(self.run_land(), land.REFUSED, self.output)
        self.assertIn("would undo work already on main", self.output)
        self.assertIn("shared.txt returns to its content before", self.output)
        self.assertIn("(chore: move main)", self.output)
        self.assertIn("added.txt is deleted", self.output)
        self.assertNotIn("feature.txt", self.output)
        self.assertIsNone(self.merged())
        self.assertEqual(self.run_land("--dry-run"), land.REFUSED, self.output)

    def test_an_intended_revert_lands_with_allow_restore(self):
        self.squash_onto_moved_main()
        head = self.remote_head()
        self.assertEqual(self.run_land("--allow-restore"), 0, self.output)
        self.assertEqual(self.merged()["sha"], head)

    def lines(self, changed=None):
        return "".join(changed.get(n, f"line {n}\n") if changed else f"line {n}\n" for n in range(1, 13))

    def test_a_stale_squash_that_also_edits_a_file_main_changed_is_refused(self):
        # The branch starts after a long file exists, then main edits its top.
        self.advance_main("long.txt", self.lines())
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "merge", "-q", "--no-edit", "origin/main")
        self.git(self.repo, "push", "-q", "origin", "feature")
        self.advance_main("long.txt", self.lines({2: "main's line 2\n"}))
        # The PR edits the bottom of its older copy, then squashes onto the moved main.
        (self.repo / "long.txt").write_text(self.lines({11: "feature's line 11\n"}))
        self.commit("feat: edit the bottom")
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "reset", "-q", "--soft", "origin/main")
        self.commit("feat: edit the bottom")
        self.git(self.repo, "push", "-q", "-f", "origin", "feature")
        self.assertEqual(self.run_land(), land.REFUSED, self.output)
        self.assertIn("long.txt lacks", self.output)

    def test_a_change_main_took_out_again_is_not_missing(self):
        self.advance_main("long.txt", self.lines())
        self.advance_main("long.txt", self.lines({6: "line 6\nadded then removed\n"}))
        self.advance_main("long.txt", self.lines())
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "merge", "-q", "--no-edit", "origin/main")
        (self.repo / "long.txt").write_text(self.lines({11: "feature's line 11\n"}))
        self.commit("feat: edit the bottom")
        self.git(self.repo, "push", "-q", "origin", "feature")
        self.assertEqual(self.run_land(), 0, self.output)

    def test_removing_one_file_of_a_recent_larger_change_lands(self):
        other = self.root / "other"
        self.git(self.root, "clone", "-q", str(self.origin), str(other))
        (other / "recent.txt").write_text("recent\n")
        (other / "shared.txt").write_text("two\n")
        self.commit("feat: add two things", other)
        self.git(other, "push", "-q", "origin", "main")
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "merge", "-q", "--no-edit", "origin/main")
        (self.repo / "recent.txt").unlink()
        self.commit("chore: drop the recent file")
        self.git(self.repo, "push", "-q", "origin", "feature")
        self.assertEqual(self.run_land(), 0, self.output)

    def test_a_refused_branch_is_not_pushed_even_when_asked_to_update(self):
        self.squash_onto_moved_main()
        self.advance_main("later.txt", "x\n")  # --update would otherwise merge this in and push.
        head = self.remote_head()
        self.assertEqual(self.run_land("--update"), land.REFUSED, self.output)
        self.assertEqual(self.remote_head(), head)

    def test_moving_a_file_main_recently_added_is_not_a_deletion(self):
        self.advance_main("added.txt", "main's new file\nwith enough lines\nto be recognised\n")
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "merge", "-q", "--no-edit", "origin/main")
        (self.repo / "moved").mkdir()
        self.git(self.repo, "mv", "added.txt", "moved/added.txt")
        self.commit("refactor: move the new file")
        self.git(self.repo, "push", "-q", "origin", "feature")
        self.assertEqual(self.run_land(), 0, self.output)

    def test_new_content_for_a_file_main_changed_is_not_a_restore(self):
        self.advance_main("shared.txt", "two\n")
        self.git(self.repo, "fetch", "-q", "origin")
        self.git(self.repo, "merge", "-q", "--no-edit", "origin/main")
        (self.repo / "shared.txt").write_text("three\n")
        (self.repo / "obsolete.txt").write_text("x\n")
        self.commit("feat: change shared")
        (self.repo / "obsolete.txt").unlink()
        self.commit("chore: drop a file this branch added")
        self.git(self.repo, "push", "-q", "origin", "feature")
        self.assertEqual(self.run_land(), 0, self.output)

    def test_a_protocol_bump_on_main_after_ci_refuses_the_landing(self):
        self.protocol_feature()
        head = self.remote_head()
        original = land.wait_for_ci

        def main_moves_after_ci(*args):
            verdict = original(*args)
            self.advance_main(protocol=(6, [6], [5, 6]))
            return verdict

        with patch.object(land, "wait_for_ci", main_moves_after_ci):
            self.assertEqual(self.run_land(), land.REFUSED, self.output)
        self.assertIn("fails the older-client check", self.output)
        self.assertIn("proven at protocol 5, not 6", self.output)
        self.assertEqual(self.remote_head(), head)
        self.assertIsNone(self.merged())

    def test_a_current_protocol_lands(self):
        self.protocol_feature()
        head = self.remote_head()
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.merged()["sha"], head)
        self.assertEqual(self.remote_head(), head)

    def test_a_dry_run_reports_the_older_client_refusal(self):
        self.protocol_feature()
        self.advance_main(protocol=(6, [6], [5, 6]))
        head = self.remote_head()
        main = self.git(self.origin, "rev-parse", "refs/heads/main")
        self.assertEqual(self.run_land("--dry-run"), land.REFUSED, self.output)
        self.assertIn("fails the older-client check", self.output)
        self.assertIn("proven at protocol 5, not 6", self.output)
        self.assertEqual(self.remote_head(), head)
        self.assertEqual(self.git(self.origin, "rev-parse", "refs/heads/main"), main)
        self.assertIsNone(self.merged())

    def test_waits_through_pending_and_unknown_checks(self):
        # One clock for the fake's reads and land-pr's waits, so each wait is
        # measured from the read it follows, however slowly the fake answers.
        now = 1_800_000_000
        self.enterContext(patch.dict(os.environ, {"FAKE_STATUS_NOW": str(now)}))
        self.enterContext(patch.object(land.time, "time", lambda: now))
        self.status({"ci": "PENDING"}, {"ci": None, "limitations": ["CI unknown"]},
                    {"ci": "SUCCESS", "checks": [{"__typename": "CheckRun", "name": "a",
                                                  "status": "IN_PROGRESS", "conclusion": None}]},
                    {"ci": "SUCCESS"})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(len(self.sleeps), 3)
        self.assertEqual(self.sleeps, [land.REFRESH + 1] * 3)

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

    def test_a_draft_from_a_read_older_than_the_ready_pr_waits_and_lands(self):
        self.status({"draft": True, "age": -1000}, {})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.merged()["sha"], self.remote_head())
        self.assertIn("waiting for GitHub to report it ready", self.output)
        self.assertEqual(len(self.sleeps), 1)

    def test_a_closed_state_from_an_older_read_waits_and_lands(self):
        self.status({"state": "CLOSED", "age": -1000}, {})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.merged()["sha"], self.remote_head())
        self.assertIn("waiting for GitHub to report it open", self.output)
        self.assertEqual(len(self.sleeps), 1)

    def test_a_current_draft_read_is_still_refused(self):
        self.status({"draft": True}, {})
        self.assertEqual(self.run_land(), land.REFUSED)
        self.assertIn("converted to draft", self.output)
        self.assertIsNone(self.merged())

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

    def test_a_failed_refresh_never_decides_from_the_snapshot_it_kept(self):
        head = "c" * 40
        kept = {"data": {"snapshot": {"pull_request": {"state": "OPEN", "isDraft": False, "headRefOid": head,
                                                       "mergeable": "MERGEABLE"},
                                      "checks_state": "SUCCESS", "checks": []},
                         "fetched_at": 1_000, "stale": True, "retry_at": 1_000_000_000,
                         "error": "GitHub GraphQL rate limit exceeded"}}
        verdict, detail = land.assess(kept, head, since=2_000)
        self.assertEqual(verdict, "wait")
        self.assertIn("GitHub GraphQL rate limit exceeded", detail)
        for snapshot in ({"checks_state": "FAILURE"}, {"pull_request": {**kept["data"]["snapshot"]["pull_request"],
                                                                         "state": "CLOSED"}}):
            changed = {"data": {**kept["data"], "snapshot": {**kept["data"]["snapshot"], **snapshot}}}
            self.assertEqual(land.assess(changed, head, since=0)[0], "wait")
        # A successful read that is past its refresh interval is no fresher.
        aged = {"data": {**kept["data"], "error": None}}
        self.assertEqual(land.assess(aged, head, since=0)[0], "wait")

    def test_checks_rerun_on_the_same_head_after_a_failed_refresh_are_read_before_landing(self):
        # The kept snapshot was green; the checks were rerun on the same head
        # and failed while status reads were paused.
        now = 1_800_000_000
        retry_at = now + 900
        self.enterContext(patch.dict(os.environ, {"FAKE_STATUS_NOW": str(now)}))
        self.enterContext(patch.object(land.time, "time", lambda: now))
        self.status({"age": -3_600, "error": "GitHub GraphQL rate limit exceeded", "retry_at": retry_at},
                    {"ci": "FAILURE", "checks": [{"__typename": "CheckRun", "name": "contracts",
                                                  "status": "COMPLETED", "conclusion": "FAILURE"}]})
        self.assertEqual(self.run_land(), land.CI_FAILED, self.output)
        self.assertIsNone(self.merged())
        self.assertIn("status refresh failed (GitHub GraphQL rate limit exceeded)", self.output)
        self.assertEqual(self.sleeps, [retry_at - now + 1])  # The advertised retry time was honoured.

    def test_dry_run_does_not_report_a_kept_snapshot_as_its_verdict(self):
        self.status({"age": -3_600, "error": "GitHub GraphQL rate limit exceeded",
                     "ci": "FAILURE", "checks": [{"__typename": "CheckRun", "name": "contracts",
                                                  "status": "COMPLETED", "conclusion": "FAILURE"}]})
        self.assertEqual(self.run_land("--dry-run"), 0, self.output)
        self.assertIn("status refresh failed", self.output)

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

    def test_a_report_that_still_lists_passing_checks_lands(self):
        # An older gh-pr-status's REST fallback lists every check, passed or not.
        self.status({"passed": None, "checks": [
            {"__typename": "CheckRun", "name": "contracts", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "StatusContext", "context": "deploy", "state": "SUCCESS"}]})
        self.assertEqual(self.run_land(), 0, self.output)

    def test_aggregate_success_lands_past_the_hundred_listed_checks(self):
        self.status({"complete": False})
        self.assertEqual(self.run_land(), 0, self.output)

    def test_ci_read_through_rest_while_graphql_is_limited_lands_on_green(self):
        # The CLI's REST read knows CI but not the review decision, which
        # land-pr does not use; only its aggregate decides.
        rest = {"transport": "rest", "data_limitations": [
            "Read through REST while GitHub GraphQL is rate limited; the review decision is unknown."]}
        self.status({**rest, "ci": "PENDING", "complete": False}, {**rest, "ci": "SUCCESS"})
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(len(self.sleeps), 1)
        self.assertIn("landed #7", self.output)

    BASE_MOVED = "gh: Base branch was modified. Review and try the merge again. (HTTP 405)"

    def merge_attempts(self):
        calls = [json.loads(line) for line in (self.root / "gh-calls").read_text().splitlines()]
        return [dict(value.split("=", 1) for value in call[5::2])["sha"]
                for call in calls if call[:3] == ["api", "-X", "PUT"]]

    def test_main_moving_during_the_merge_retries_it_once_at_the_same_head(self):
        head = self.remote_head()
        (self.root / "merge-errors.json").write_text(json.dumps([{"error": self.BASE_MOVED}]))
        self.assertEqual(self.run_land(), 0, self.output)
        self.assertEqual(self.merge_attempts(), [head, head])
        self.assertEqual(self.merged()["sha"], head)
        self.assertEqual(self.sleeps, [land.MINIMUM_WAIT])

    def test_main_moving_into_a_conflict_during_the_merge_takes_the_conflict_path(self):
        old = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")
        (self.root / "merge-errors.json").write_text(json.dumps([{"error": self.BASE_MOVED, "dirty": True}]))
        self.assertEqual(self.run_land(), 0, self.output)
        new = self.remote_head()
        self.assertEqual(self.git(self.origin, "rev-parse", f"{new}^1"), old)
        self.assertEqual(self.merge_attempts(), [old, new])
        self.assertIn("now conflicts with main", self.output)

    def test_a_protocol_bump_while_merging_is_checked_before_the_retry(self):
        self.protocol_feature()
        head = self.remote_head()
        (self.root / "merge-errors.json").write_text(json.dumps([{"error": self.BASE_MOVED}]))
        original = land.merge

        def main_moves_during_the_merge(*args):
            if not self.merge_attempts():
                self.advance_main(protocol=(6, [6], [5, 6]))
            return original(*args)

        with patch.object(land, "merge", main_moves_during_the_merge):
            self.assertEqual(self.run_land(), land.REFUSED, self.output)
        self.assertIn("fails the older-client check", self.output)
        self.assertEqual(self.merge_attempts(), [head])
        self.assertIsNone(self.merged())

    def test_a_conflict_github_was_still_computing_takes_the_conflict_path(self):
        old = self.remote_head()
        self.advance_main("unrelated.txt", "x\n")
        (self.root / "merge-errors.json").write_text(json.dumps([
            {"error": self.BASE_MOVED},
            {"error": "gh: Pull Request is not mergeable (HTTP 405)", "dirty": True}]))
        self.assertEqual(self.run_land(), 0, self.output)
        new = self.remote_head()
        self.assertEqual(self.merge_attempts(), [old, old, new])
        self.assertEqual(self.git(self.origin, "rev-parse", f"{new}^1"), old)

    def test_a_head_that_moved_with_main_is_not_merged_again(self):
        (self.root / "merge-errors.json").write_text(json.dumps([{"error": self.BASE_MOVED}]))
        original = land.read_pr
        reads = []

        def moved(repository, number):
            pr = original(repository, number)
            reads.append(pr)
            if len(reads) > 1:  # Someone pushed while the merge was refused.
                pr["head"]["sha"] = "e" * 40
            return pr

        with patch.object(land, "read_pr", moved):
            self.assertEqual(self.run_land(), land.HEAD_MOVED, self.output)
        self.assertEqual(len(self.merge_attempts()), 1)
        self.assertIsNone(self.merged())

    def test_main_moving_during_both_merges_is_refused_without_looping(self):
        head = self.remote_head()
        (self.root / "merge-errors.json").write_text(json.dumps([{"error": self.BASE_MOVED}] * 3))
        self.assertEqual(self.run_land(), land.REFUSED, self.output)
        self.assertEqual(self.merge_attempts(), [head, head])
        self.assertIsNone(self.merged())
        self.assertIn("keeps moving", self.output)

    def test_an_empty_squash_message_is_refused(self):
        with self.assertRaises(land.Stop) as refused:
            land.squash_message(3, "Title", [("Merge origin/main into fix/a", 2),
                                             ("Co-authored-by: Claude <noreply@anthropic.com>", 1)])
        self.assertEqual(refused.exception.status, land.REFUSED)


if __name__ == "__main__":
    unittest.main()
