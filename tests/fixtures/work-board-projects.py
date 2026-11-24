#!/usr/bin/python3
"""Give a native-evidence fixture many GitHub projects for the Work board.

Run it in a session's fixture, then open the projects it prints:
    scripts/native-evidence env python3 tests/fixtures/work-board-projects.py

It makes fourteen projects under projects/, each with a github.com remote
under octo-org and a worktree on a pushed branch, and seeds the offline forge
with an open pull request, the viewer's, for every other one. The board then
has fourteen projects, the In progress and In review columns filled, as a
long-lived installation has them.

    scripts/native-evidence env python3 tests/fixtures/work-board-projects.py rate-limit

spends the account's GraphQL quota, as many agents sharing one GitHub account
do: every GraphQL request fails as GitHub's does until the start of the hour
after next, so a Before and an After session in the same hour show the same
reset time. REST, a separate quota, still answers. Refresh the board, or wait
for its next poll, to see it. `rate-limit off` gives the quota back.

Running it again keeps the projects and rewrites their seeds, keeping any
other repository's. Nothing here reaches the network or a real provider home.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

OWNER = "octo-org"
# Names a team's repositories have; none is anyone's real project.
PROJECTS = ("api-gateway", "billing", "docs-site", "mobile-app", "design-system", "infra",
            "search", "notifications", "auth-service", "analytics", "web-client", "cli-tools",
            "payments", "data-pipeline")
BRANCHES = ("fix/retry-headers", "feat/invoice-export", "docs/quickstart", "fix/offline-sync",
            "feat/dark-tokens", "chore/terraform-1-9", "feat/typo-tolerance", "fix/digest-dedupe",
            "feat/passkeys", "fix/funnel-dates", "feat/saved-views", "fix/completion-zsh",
            "feat/refund-reasons", "fix/backfill-gaps")
GIT_ENVIRONMENT = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "Fixture",
    "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Fixture",
    "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
}


def fail(message):
    print(f"work-board-projects: {message}", file=sys.stderr)
    raise SystemExit(2)


def git(directory, *arguments, date="2026-11-17T09:00:00Z"):
    environment = {**os.environ, **GIT_ENVIRONMENT, "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date}
    return subprocess.run(["git", "-C", str(directory), *arguments], env=environment, check=True,
                          stdout=subprocess.PIPE, text=True).stdout.strip()


def prepare(root, index, name):
    """One project with a pushed branch in a worktree, made once. Returns
    the branch and its commit."""
    project = root / name
    branch = BRANCHES[index]
    worktree = project / ".worktrees" / branch.split("/", 1)[1]
    if not worktree.exists():
        if project.exists():
            fail(f"{project} was left half made by an earlier run; use a fresh native-evidence fixture")
        project.mkdir(parents=True)
        git(project, "init", "--quiet", "--initial-branch=main")
        (project / "README.md").write_text(f"# {name}\n")
        git(project, "add", "README.md")
        git(project, "commit", "--quiet", "--message", f"Start {name}")
        exclude = Path(git(project, "rev-parse", "--path-format=absolute", "--git-path", "info/exclude"))
        with exclude.open("a") as file:
            file.write(".worktrees/\n")
        git(project, "remote", "add", "origin", f"https://github.com/{OWNER}/{name}")
        git(project, "update-ref", "refs/remotes/origin/main", "main")
        git(project, "worktree", "add", "--quiet", "-b", branch, str(worktree), "main")
        (worktree / "NOTES.md").write_text(f"{branch}\n")
        git(worktree, "add", "NOTES.md")
        # Spread over a morning, so the cards have distinct ages.
        git(worktree, "commit", "--quiet", "--message", branch.split("/", 1)[1].replace("-", " ").capitalize(),
            date=f"2026-11-17T{10 + index // 2:02}:{(index % 2) * 30:02}:00Z")
        oid = git(project, "rev-parse", branch)
        git(project, "config", f"branch.{branch}.remote", "origin")
        git(project, "config", f"branch.{branch}.merge", f"refs/heads/{branch}")
        git(project, "update-ref", f"refs/remotes/origin/{branch}", oid)
    return branch, git(project, "rev-parse", branch)


def seed(index, name, branch, oid):
    """The viewer's open pull request for the branch, in `gh pr list --json`'s
    shape (see fixture-gh.sh), its checks passed."""
    number = 100 + index
    updated = f"2026-11-17T{10 + index // 2:02}:{(index % 2) * 30 + 5:02}:00Z"
    remote = f"https://github.com/{OWNER}/{name}"
    return {
        "id": f"PR_fixture{name}", "number": number,
        "title": branch.split("/", 1)[1].replace("-", " ").capitalize(), "body": "", "state": "OPEN",
        "isDraft": False, "headRefName": branch, "headRefOid": oid, "baseRefName": "main",
        "headRepositoryOwner": {"login": OWNER}, "headRepository": {"name": name},
        "author": {"login": "fixture"}, "createdAt": updated, "updatedAt": updated,
        "url": f"{remote}/pull/{number}", "labels": [], "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN", "reviewDecision": "REVIEW_REQUIRED", "additions": 4, "deletions": 1,
        "changedFiles": 1, "statusCheckRollup": [{
            "__typename": "CheckRun", "name": "test", "workflowName": "CI", "status": "COMPLETED",
            "conclusion": "SUCCESS", "startedAt": updated, "completedAt": updated,
            "detailsUrl": f"{remote}/actions/runs/{number}/job/1"}],
    }


def write(path, content):
    """Whole or not at all: the forge reads these on every call."""
    partial = path.with_name(path.name + ".next")
    partial.write_text(content)
    partial.replace(path)


def fixture_root():
    fixture = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not fixture:
        fail("run it in a native-evidence fixture: scripts/native-evidence env python3 " + sys.argv[0])
    fixture = Path(fixture).resolve()
    home = fixture / "home"
    # Never write forge files except inside an evidence fixture's HOME.
    if not ((fixture / "bin").is_dir() and home.is_dir() and Path.home().resolve() == home):
        fail(f"{fixture} is not a native-evidence fixture whose HOME is this one; start a session first")
    (home / ".config").mkdir(exist_ok=True)
    return fixture


def rate_limit(config, switch):
    path = config / "fixture-forge-rate-limited"
    if switch == "off":
        path.unlink(missing_ok=True)
        return {"rate_limited": False}
    reset = (int(time.time()) // 3600 + 2) * 3600
    write(path, f"{reset}\n")
    return {"rate_limited": True, "reset": reset}


def main():
    arguments = sys.argv[1:]
    if arguments not in ([], ["rate-limit"], ["rate-limit", "off"]):
        fail("takes no arguments, `rate-limit` or `rate-limit off`")
    config = fixture_root() / "home" / ".config"
    if arguments:
        print(json.dumps(rate_limit(config, arguments[-1])))
        return
    root = fixture_root() / "projects"
    made = [(index, name, *prepare(root, index, name)) for index, name in enumerate(PROJECTS)]
    prs = config / "fixture-forge-prs.json"
    ours = tuple(f"https://github.com/{OWNER}/{name}/pull/" for name in PROJECTS)
    try:
        kept = [row for row in json.loads(prs.read_text()) if not str(row.get("url", "")).startswith(ours)]
    except FileNotFoundError:
        kept = []
    except (OSError, ValueError) as error:
        fail(f"{prs} could not be read: {error}")
    seeds = [seed(index, name, branch, oid) for index, name, branch, oid in made if index % 2 == 0]
    # The switch goes on before the projects are opened: a failed read while
    # a remote is there makes the app wait before asking again.
    (config / "fixture-forge").touch()
    write(prs, json.dumps(kept + seeds, indent=1) + "\n")
    print(json.dumps({"projects": [str(root / name) for name in PROJECTS]}))


if __name__ == "__main__":
    main()
