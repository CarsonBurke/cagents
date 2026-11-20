#!/usr/bin/python3
"""Give a native-evidence fixture a Claude conversation with work to show.

Run it in a session's fixture, then start a Claude chat in the project:
    scripts/native-evidence env python3 tests/fixtures/conversation-work.py

It adds three worktrees of the fixture project under .worktrees/, gives the
project a github.com remote, seeds the offline forge with the pull requests
they head and two others, and writes home/.claude/fixture-frames.jsonl from
conversation-work/frames.jsonl. The next Claude turn in the fixture then
replays a conversation that

- opened #41 (`gh pr create`) from fix/cache-key, whose checks are passing,
  running and failing, and left a test there uncommitted;
- pushed feat/retry-budget, the branch of the open #38, all checks passing;
- merged #35 (`gh pr merge`);
- edited docs/locale-notes, a worktree with no pull request;
- mentioned #12 by URL and #29 by number.

The conversation's inspector lists these as its Work. Every Claude turn in
the fixture replays the same frames until fixture-frames.jsonl is removed.
Running it again rewrites the forge seeds and frames and keeps the worktrees.
Nothing here reaches the network or a real provider home.
"""

import sys
from pathlib import Path

sys.dont_write_bytecode = True
for _fixture_parent in Path(__file__).resolve().parents:
    if (_fixture_parent / "scripts/fixture_home.py").is_file():
        sys.path.insert(0, str(_fixture_parent / "scripts"))
        break
from fixture_home import require_fixture_home

import json
import os
import subprocess

HERE = Path(__file__).resolve().parent
REMOTE = "https://github.com/octo-org/fixture-app"
# Fixed authorship and dates: only the fixture project's own first commit,
# which native-evidence makes, differs between sessions.
GIT_ENVIRONMENT = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "Fixture",
    "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Fixture",
    "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
}

CACHE = "pub fn key(path: &str) -> String {\n    path.to_owned()\n}\n"
CACHE_KEYED = 'pub fn key(path: &str, locale: &str) -> String {\n    format!("{locale}:{path}")\n}\n'
CACHE_TESTED = CACHE_KEYED + (
    "\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn keys_differ_by_locale() {\n"
    '        assert_ne!(super::key("/", "en"), super::key("/", "fr"));\n    }\n}\n')
RETRY = "pub const ATTEMPTS: u32 = 5;\n"
RETRY_ATTEMPTS = RETRY + "\npub fn attempts() -> u32 {\n    ATTEMPTS\n}\n"
RETRY_BUDGET = RETRY_ATTEMPTS.replace(
    RETRY, "pub const ATTEMPTS: u32 = 3;\n"
    "pub const BUDGET: std::time::Duration = std::time::Duration::from_secs(20);\n")
LOCALES = "# Locales\n\nResponses are cached per path.\n"
LOCALES_NOTED = LOCALES.replace(
    "Responses are cached per path.",
    "Responses are cached per path and locale, so a page never comes back in another language.")


def fail(message):
    print(f"conversation-work: {message}", file=sys.stderr)
    raise SystemExit(2)


def git(directory, *arguments, date="2026-11-17T15:00:00Z"):
    environment = {**os.environ, **GIT_ENVIRONMENT, "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date}
    return subprocess.run(["git", "-C", str(directory), *arguments], env=environment, check=True,
                          stdout=subprocess.PIPE, text=True).stdout.strip()


def commit(directory, files, message, date):
    for name, content in files.items():
        path = directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    git(directory, "add", "--", *files)
    git(directory, "commit", "--quiet", "--message", message, date=date)
    return git(directory, "rev-parse", "HEAD")


def track(project, branch, oid):
    """Records `branch` as pushed to origin at `oid`, as a push leaves it."""
    git(project, "config", f"branch.{branch}.remote", "origin")
    git(project, "config", f"branch.{branch}.merge", f"refs/heads/{branch}")
    git(project, "update-ref", f"refs/remotes/origin/{branch}", oid)


def prepare_worktrees(project):
    """The project's three worktrees, made once. Returns their branches' oids."""
    worktrees = project / ".worktrees"
    # The last step tracks the retry budget's branch; until then, nothing is.
    started = (worktrees / "cache-key").exists()
    finished = bool(git(project, "for-each-ref", "refs/remotes/origin/feat/retry-budget"))
    if started and not finished:
        fail(f"{worktrees} was left half made by an earlier run; use a fresh native-evidence fixture")
    if not started:
        if git(project, "status", "--porcelain"):
            fail(f"{project} has uncommitted changes; use a fresh native-evidence fixture")
        # As the app does for its own managed worktrees.
        exclude = Path(git(project, "rev-parse", "--path-format=absolute", "--git-path", "info/exclude"))
        exclude.parent.mkdir(parents=True, exist_ok=True)
        excluded = exclude.read_text(errors="replace").splitlines() if exclude.exists() else []
        if ".worktrees/" not in excluded:
            with exclude.open("a") as file:
                file.write(".worktrees/\n")
        base = commit(project, {"src/cache.rs": CACHE, "src/retry.rs": RETRY, "docs/locales.md": LOCALES},
                      "Add the cache, retry and locale modules", "2026-11-17T14:00:00Z")
        git(project, "update-ref", "refs/remotes/origin/main", base)
        for name, branch in (("cache-key", "fix/cache-key"), ("retry-budget", "feat/retry-budget"),
                             ("locale-notes", "docs/locale-notes")):
            git(project, "worktree", "add", "--quiet", "-b", branch, str(worktrees / name), base)
        cache = commit(worktrees / "cache-key", {"src/cache.rs": CACHE_KEYED},
                       "Key the response cache by locale", "2026-11-17T15:18:00Z")
        track(project, "fix/cache-key", cache)
        (worktrees / "cache-key" / "src/cache.rs").write_text(CACHE_TESTED)
        commit(worktrees / "retry-budget", {"src/retry.rs": RETRY_ATTEMPTS},
               "Retry failed requests", "2026-11-17T14:30:00Z")
        retry = commit(worktrees / "retry-budget", {"src/retry.rs": RETRY_BUDGET},
                       "Bound the retry budget", "2026-11-17T15:08:00Z")
        (worktrees / "locale-notes" / "docs/locales.md").write_text(LOCALES_NOTED)
        track(project, "feat/retry-budget", retry)
    return {
        "base": git(project, "rev-parse", "main"),
        "cache": git(project, "rev-parse", "fix/cache-key"),
        "retry_base": git(project, "rev-parse", "feat/retry-budget~1"),
        "retry": git(project, "rev-parse", "feat/retry-budget"),
    }


JOBS = {"build": 1, "lint": 2, "test": 3}


def check_run(name, status, conclusion, started, completed=None):
    return {"__typename": "CheckRun", "name": name, "workflowName": "CI", "status": status,
            "conclusion": conclusion, "startedAt": started, "completedAt": completed,
            "detailsUrl": f"{REMOTE}/actions/runs/7001/job/{JOBS[name]}"}


def pull_requests(oids):
    """Seeds in the shape `gh pr list --json` prints; see fixture-gh.sh."""
    def pull(number, title, state, branch, author, updated, **extra):
        return {
            "id": f"PR_fixture{number}", "number": number, "title": title, "body": "", "state": state,
            "isDraft": False, "headRefName": branch, "headRefOid": extra.pop("head", "0" * 40),
            "baseRefName": "main", "baseRefOid": oids["base"],
            "headRepositoryOwner": {"login": "octo-org"}, "headRepository": {"name": "fixture-app"},
            "author": {"login": author}, "createdAt": updated, "updatedAt": updated,
            "url": f"{REMOTE}/pull/{number}", "labels": [], "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN", "reviewDecision": None, "statusCheckRollup": [], **extra}
    return [
        pull(41, "Key the response cache by locale", "OPEN", "fix/cache-key", "fixture",
             "2026-11-17T15:20:00Z", head=oids["cache"], additions=3, deletions=2, changedFiles=1,
             reviewDecision="REVIEW_REQUIRED", mergeStateStatus="UNSTABLE", statusCheckRollup=[
                 check_run("build", "COMPLETED", "SUCCESS", "2026-11-17T15:20:10Z", "2026-11-17T15:22:40Z"),
                 check_run("lint", "COMPLETED", "FAILURE", "2026-11-17T15:20:10Z", "2026-11-17T15:21:05Z"),
                 check_run("test", "IN_PROGRESS", "", "2026-11-17T15:20:10Z")]),
        pull(38, "Bound the retry budget", "OPEN", "feat/retry-budget", "fixture",
             "2026-11-17T15:10:00Z", head=oids["retry"], additions=6, deletions=1, changedFiles=1,
             reviewDecision="APPROVED", statusCheckRollup=[
                 check_run("build", "COMPLETED", "SUCCESS", "2026-11-17T15:08:20Z", "2026-11-17T15:10:30Z"),
                 check_run("lint", "COMPLETED", "SUCCESS", "2026-11-17T15:08:20Z", "2026-11-17T15:09:10Z"),
                 check_run("test", "COMPLETED", "SUCCESS", "2026-11-17T15:08:20Z", "2026-11-17T15:12:50Z")]),
        pull(35, "Drop the legacy cache flag", "MERGED", "chore/legacy-cache-flag", "fixture",
             "2026-11-17T14:55:00Z", head="35" * 20, additions=2, deletions=14, changedFiles=3,
             reviewDecision="APPROVED", mergeable="UNKNOWN", mergeStateStatus="UNKNOWN"),
        pull(29, "Read the locale from Accept-Language", "CLOSED", "fix/locale-header", "hubot",
             "2026-11-12T09:30:00Z", head="29" * 20, additions=9, deletions=3, changedFiles=2),
        pull(12, "Fall back to the default locale", "MERGED", "feat/locale-fallback", "octocat",
             "2026-11-10T16:45:00Z", head="12" * 20, additions=21, deletions=4, changedFiles=3,
             reviewDecision="APPROVED", mergeable="UNKNOWN", mergeStateStatus="UNKNOWN"),
    ]


def frames(project, oids):
    template = (HERE / "conversation-work" / "frames.jsonl").read_text()
    replacements = {"@PROJECT@": str(project), "@RETRY_BASE@": oids["retry_base"][:7],
                    "@RETRY_HEAD@": oids["retry"][:7]}
    lines = []
    for line in template.splitlines():
        # Substituted in the decoded strings, so a path is escaped as JSON.
        def substitute(value):
            if isinstance(value, str):
                for placeholder, replacement in replacements.items():
                    value = value.replace(placeholder, replacement)
                return value
            if isinstance(value, list):
                return [substitute(item) for item in value]
            if isinstance(value, dict):
                return {key: substitute(item) for key, item in value.items()}
            return value
        lines.append(json.dumps(substitute(json.loads(line)), ensure_ascii=False, separators=(",", ":")))
    return "\n".join(lines) + "\n"


def write(path, content):
    """Whole or not at all: the provider and forge read these on every call."""
    partial = path.with_name(path.name + ".next")
    partial.write_text(content)
    partial.replace(path)


def main():
    require_fixture_home("HOME")
    if len(sys.argv) > 1:
        fail(f"takes no arguments; run it as: scripts/native-evidence env python3 {sys.argv[0]}")
    fixture = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not fixture:
        fail("run it in a native-evidence fixture: scripts/native-evidence env python3 " + sys.argv[0])
    fixture = Path(fixture).resolve()
    project = fixture / "project"
    home = fixture / "home"
    # Never write provider or forge files except inside an evidence fixture's HOME.
    if not ((fixture / "bin").is_dir() and (project / ".git").exists() and (home / ".claude").is_dir()
            and Path.home().resolve() == home):
        fail(f"{fixture} is not a native-evidence fixture whose HOME is this one; start a session first")
    config = require_fixture_home("XDG_CONFIG_HOME", home / ".config")
    claude = require_fixture_home("CLAUDE_CONFIG_DIR", home / ".claude")
    oids = prepare_worktrees(project)
    config.mkdir(exist_ok=True)
    # The switch goes on before the remote appears: a failed read while the
    # remote is there makes the app wait before asking again.
    (config / "fixture-forge").touch()
    write(config / "fixture-forge-prs.json", json.dumps(pull_requests(oids), indent=1) + "\n")
    if "origin" not in git(project, "remote").split():
        git(project, "remote", "add", "origin", REMOTE)
    write(claude / "fixture-frames.jsonl", frames(project, oids))
    print(json.dumps({"project": str(project),
                      "worktrees": sorted(str(path) for path in (project / ".worktrees").iterdir())}))


if __name__ == "__main__":
    main()
