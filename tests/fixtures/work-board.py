#!/usr/bin/python3
"""Give a native-evidence fixture a Work board at a long-lived installation's scale.

Run it in a session's fixture, before the projects are opened:
    scripts/native-evidence env python3 -I tests/fixtures/work-board.py

It makes eight invented projects under projects/, each with linked worktrees
under its .worktrees/, and seeds the offline forge with their pull requests.
Ages are relative to now, so the board's columns come out the same whenever it
runs. Every project gets:

- a few checkouts touched in the last days, which the board derives as In
  progress;
- many older ones, untouched for weeks, which it derives as Stale.

and some projects get the cards the board's design is judged by:

- atlas: feat/search-ranking, an open pull request under way, the worktree
  `work-board-scene.sh live` starts four agents in; an open, approved pull
  request in review; a merged one left to clean up; and a detached checkout,
  pr-207-review, with a commit of its own from two days ago;
- billing-service: a draft pull request with uncommitted changes, and a
  detached checkout, invoice-bisect, still at the base it was made from;
- journal: offline-mode, where `live` starts one working agent.

It prints the projects' paths, one a line, for the scene to open. Running it
again in the same fixture changes nothing in Git and rewrites the forge seeds.
Nothing here reaches the network or a real provider home.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

DAY = 24 * 60 * 60
HOUR = 60 * 60
NOW = int(time.time())

GIT_ENVIRONMENT = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "Fixture",
    "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Fixture",
    "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
}

# Projects with a github.com remote under this owner have pull requests.
OWNER = "octo-org"
REMOTES = {"atlas", "billing-service", "edge-proxy", "journal"}

# Branches checked out in linked worktrees, by project: (branch, age of its
# own last commit in seconds). The first group is In progress, the second
# Stale, under the default seven days.
RECENT = {
    "atlas": [("refactor/query-parser", 2 * DAY), ("docs/search-syntax", 4 * DAY)],
    "billing-service": [("fix/rounding-mode", 5 * DAY), ("feat/tax-export", 6 * DAY)],
    "design-system": [("feat/dark-palette", 9 * HOUR), ("fix/focus-ring", 3 * DAY),
                      ("chore/token-names", 5 * DAY)],
    "edge-proxy": [("feat/tls-renewal", 1 * DAY), ("fix/header-casing", 4 * DAY)],
    "ferry": [("feat/ios-push", 2 * DAY), ("fix/resume-crash", 6 * DAY)],
    "glacier-ml": [("exp/ablation-lr", 1 * DAY), ("exp/warmup-steps", 3 * DAY)],
    "harbor": [("docs/getting-started", 5 * DAY)],
    "journal": [("fix/sync-conflicts", 2 * DAY)],
}
STALE = {
    "atlas": ["spike/vector-index", "fix/typo-search-box", "feat/saved-queries", "chore/ci-cache",
              "exp/bm25-weights", "fix/pagination-off-by-one"],
    "billing-service": ["feat/stripe-webhooks", "refactor/ledger", "fix/currency-format",
                        "exp/invoice-pdf", "chore/drop-v1-api"],
    "design-system": ["feat/motion-tokens", "fix/button-padding", "exp/variable-fonts",
                      "chore/storybook-8", "fix/contrast-ratios", "feat/density-modes"],
    "edge-proxy": ["feat/http3", "fix/keepalive-leak", "exp/rate-limiter", "chore/rustls-bump"],
    "ferry": ["feat/offline-queue", "fix/deeplink-routing", "exp/new-onboarding",
              "chore/gradle-8", "fix/dark-splash"],
    "glacier-ml": ["exp/tokenizer-drift", "exp/longer-context", "fix/eval-seed",
                   "feat/sweep-runner", "exp/mixed-precision"],
    "harbor": ["docs/api-reference", "fix/broken-links", "feat/search-page", "chore/theme-update"],
    "journal": ["feat/markdown-export", "fix/emoji-titles", "exp/crdt-sync", "chore/sqlite-3-46"],
}


def fail(message):
    print(f"work-board: {message}", file=sys.stderr)
    raise SystemExit(2)


def git(directory, *arguments, at=NOW):
    date = f"@{at} +0000"
    environment = {**os.environ, **GIT_ENVIRONMENT, "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date}
    return subprocess.run(["git", "-C", str(directory), *arguments], env=environment, check=True,
                          stdout=subprocess.PIPE, text=True).stdout.strip()


def commit(directory, name, content, message, at):
    path = directory / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    git(directory, "add", "--", name)
    git(directory, "commit", "--quiet", "--message", message, at=at)
    return git(directory, "rev-parse", "HEAD")


def subject(branch):
    """`fix/currency-format` becomes `Fix currency format`."""
    words = branch.split("/", 1)[-1].replace("-", " ")
    return words[:1].upper() + words[1:]


def checkout(project, branch, at, detached=False, slug=None, message=None):
    """A linked worktree with one commit of its own made `at`. Returns its head."""
    slug = slug or branch.replace("/", "-")
    path = project / ".worktrees" / slug
    if detached:
        git(project, "worktree", "add", "--quiet", "--detach", str(path), "main")
    else:
        git(project, "worktree", "add", "--quiet", "-b", branch, str(path), "main")
    message = message or subject(branch)
    return commit(path, f"notes/{slug}.md", f"# {message}\n", message, at)


def make_project(projects, name):
    project = projects / name
    if (project / ".git").exists():
        return project, None
    project.mkdir(parents=True)
    base_at = NOW - 120 * DAY
    git(project, "init", "--quiet", "--initial-branch=main")
    # As the app does for its own managed worktrees.
    with (project / ".git" / "info" / "exclude").open("a") as file:
        file.write(".worktrees/\n")
    commit(project, "README.md", f"# {name}\n", "Initial commit", base_at)
    if name in REMOTES:
        git(project, "remote", "add", "origin", f"https://github.com/{OWNER}/{name}")
        git(project, "update-ref", "refs/remotes/origin/main", git(project, "rev-parse", "HEAD"))
    heads = {}
    for branch, age in RECENT[name]:
        heads[branch] = checkout(project, branch, NOW - age)
    for index, branch in enumerate(STALE[name]):
        heads[branch] = checkout(project, branch, NOW - (12 + 9 * index) * DAY)
    return project, heads


def pull(name, number, title, branch, head, updated, state="OPEN", draft=False, **extra):
    """A seed in the shape `gh pr list --json` prints; see fixture-gh.sh."""
    updated_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(updated))
    remote = f"https://github.com/{OWNER}/{name}"
    return {
        "id": f"PR_{name}{number}", "number": number, "title": title, "body": "", "state": state,
        "isDraft": draft, "headRefName": branch, "headRefOid": head, "baseRefName": "main",
        "headRepositoryOwner": {"login": OWNER}, "headRepository": {"name": name},
        "author": {"login": "fixture"}, "createdAt": updated_at, "updatedAt": updated_at,
        "url": f"{remote}/pull/{number}", "labels": [], "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN", "reviewDecision": None, "statusCheckRollup": [],
        "additions": 40, "deletions": 12, "changedFiles": 4, **extra}


def check(name, status, conclusion, started):
    at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started))
    return {"__typename": "CheckRun", "name": name, "workflowName": "CI", "status": status,
            "conclusion": conclusion, "startedAt": at, "completedAt": at if status == "COMPLETED" else None,
            "detailsUrl": "https://github.com/octo-org/actions/runs/1"}


def special_cards(projects):
    """The cards the scene is judged by. Returns the forge seeds that go with them."""
    atlas = projects / "atlas"
    billing = projects / "billing-service"
    journal = projects / "journal"
    seeds = []
    worktrees = atlas / ".worktrees"
    if not (worktrees / "feat-search-ranking").exists():
        checkout(atlas, "feat/search-ranking", NOW - 50 * 60)
        checkout(atlas, "fix/login-timeout", NOW - 2 * DAY)
        checkout(atlas, "chore/bump-deps", NOW - 3 * DAY)
        checkout(atlas, "pr-207-review", NOW - 2 * DAY, detached=True,
                 message="Try the reviewer's suggestion for #207")
        checkout(billing, "feat/proration", NOW - 1 * DAY)
        proration = billing / ".worktrees" / "feat-proration"
        for name in ("src/proration.rs", "tests/proration.rs"):
            path = proration / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("// proration, in progress\n")
            os.utime(path, (NOW - 20 * HOUR, NOW - 20 * HOUR))
        git(billing, "worktree", "add", "--quiet", "--detach",
            str(billing / ".worktrees" / "invoice-bisect"), "main")
        checkout(journal, "feat/offline-mode", NOW - 6 * HOUR)
    head = lambda project, slug: git(project / ".worktrees" / slug, "rev-parse", "HEAD")
    seeds += [
        pull("atlas", 212, "Rank search results by recency and click-through",
             "feat/search-ranking", head(atlas, "feat-search-ranking"), NOW - 40 * 60,
             statusCheckRollup=[check("build", "COMPLETED", "SUCCESS", NOW - 40 * 60),
                                check("test", "IN_PROGRESS", "", NOW - 30 * 60)]),
        pull("atlas", 208, "Retry the login request once when it times out",
             "fix/login-timeout", head(atlas, "fix-login-timeout"), NOW - 1 * DAY,
             reviewDecision="APPROVED",
             statusCheckRollup=[check("build", "COMPLETED", "SUCCESS", NOW - 1 * DAY),
                                check("test", "COMPLETED", "SUCCESS", NOW - 1 * DAY)]),
        pull("atlas", 201, "Bump dependencies for the spring release", "chore/bump-deps",
             head(atlas, "chore-bump-deps"), NOW - 2 * DAY, state="MERGED",
             reviewDecision="APPROVED", mergeable="UNKNOWN", mergeStateStatus="UNKNOWN"),
        pull("billing-service", 77, "Prorate plan changes in the middle of a billing cycle",
             "feat/proration", head(billing, "feat-proration"), NOW - 1 * DAY, draft=True),
        pull("journal", 54, "Keep editing while offline and sync on reconnect", "feat/offline-mode",
             head(journal, "feat-offline-mode"), NOW - 5 * HOUR,
             statusCheckRollup=[check("build", "COMPLETED", "FAILURE", NOW - 5 * HOUR)]),
    ]
    return seeds


def write(path, content):
    """Whole or not at all: the forge reads it on every call."""
    partial = path.with_name(path.name + ".next")
    partial.write_text(content)
    partial.replace(path)


def main():
    if len(sys.argv) > 1:
        fail(f"takes no arguments; run it as: scripts/native-evidence env python3 -I {sys.argv[0]}")
    fixture = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not fixture:
        fail("run it in a native-evidence fixture: scripts/native-evidence env python3 -I " + sys.argv[0])
    fixture = Path(fixture).resolve()
    home = fixture / "home"
    # Never write forge files except inside an evidence fixture's HOME.
    if not ((fixture / "bin").is_dir() and Path.home().resolve() == home):
        fail(f"{fixture} is not a native-evidence fixture whose HOME is this one; start a session first")
    projects = fixture / "projects"
    for name in RECENT:
        make_project(projects, name)
    seeds = special_cards(projects)
    config = home / ".config"
    config.mkdir(exist_ok=True)
    (config / "fixture-forge").touch()
    write(config / "fixture-forge-prs.json", json.dumps(seeds, indent=1) + "\n")
    for name in RECENT:
        print(projects / name)


if __name__ == "__main__":
    main()
