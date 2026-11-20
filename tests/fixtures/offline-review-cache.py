#!/usr/bin/python3
"""Give a native-evidence fixture a cached pull request review GitHub can't refresh.

Run it in a session, once the app has started, before opening the review:
    scripts/native-evidence env python3 tests/fixtures/offline-review-cache.py

It commits a base and a pull request head to the fixture project, checks the
head out as the worktree .worktrees/cache-key on fix/cache-key, gives the
project a github.com remote, and seeds the offline forge with #57 "Key the
response cache by locale", which that worktree heads. Then it writes the
review the app would have cached for #57 into the app's own session store:
its two changed files, src/cache.rs selected, docs/caching.md marked viewed,
and a draft line comment on src/cache.rs that was never posted.

The forge answers a pull request's own review queries as not found, so opening
#57's review loads this cache and stays read only: GitHub never confirms the
review, and the app has no authority to post from it. The objects the diff
names are this project's own, so the diff renders.

The store belongs to the running app, so the cache is written in one SQLite
transaction, as the app writes it, with an ordering the app's next save
replaces. Running it again rewrites the seed and the cache, with the draft as
it is here, and keeps the commits. Nothing here reaches the network.
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

REMOTE = "https://github.com/octo-org/fixture-app"
REPOSITORY = {"host": "github.com", "port": None, "owner": "octo-org", "name": "fixture-app"}
# How src/github_identity.rs's gh_selector names the repository, lowercased,
# in the review_cache tables' key.
SELECTOR = "github.com/octo-org/fixture-app"
NUMBER = 57
BRANCH = "fix/cache-key"
VIEWER = "fixture"
# Not the store's clock domain, so the app's own next save supersedes it.
WRITE_EPOCH = "fixture-offline-review"
GIT_ENVIRONMENT = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "Fixture",
    "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Fixture",
    "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
}

CACHE = """use std::collections::HashMap;

/// Responses kept in memory, keyed by request path.
pub struct Cache {
    entries: HashMap<String, String>,
}

impl Cache {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn get(&self, path: &str) -> Option<&String> {
        self.entries.get(&key(path))
    }

    pub fn insert(&mut self, path: &str, body: String) {
        self.entries.insert(key(path), body);
    }
}

fn key(path: &str) -> String {
    path.to_owned()
}
"""
CACHE_KEYED = """use std::collections::HashMap;

/// Responses kept in memory, keyed by locale and request path.
pub struct Cache {
    entries: HashMap<String, String>,
}

impl Cache {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn get(&self, locale: &str, path: &str) -> Option<&String> {
        self.entries.get(&key(locale, path))
    }

    pub fn insert(&mut self, locale: &str, path: &str, body: String) {
        self.entries.insert(key(locale, path), body);
    }
}

fn key(locale: &str, path: &str) -> String {
    format!("{locale}:{path}")
}
"""
CACHING = "# Caching\n\nResponses are cached per locale and path, so a page never comes back in another language.\n"
# The draft's line, on the head side of src/cache.rs.
ANCHORED = '    format!("{locale}:{path}")'
DRAFT = "An empty locale makes this key \":/path\". Should it fall back to the default locale first?"


def fail(message):
    print(f"offline-review-cache: {message}", file=sys.stderr)
    raise SystemExit(2)


def git(directory, *arguments, date="2026-11-18T10:00:00Z"):
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


def prepare_commits(project):
    """The pull request's base and head, made once. Returns their oids."""
    worktree = project / ".worktrees" / "cache-key"
    if not git(project, "for-each-ref", f"refs/heads/{BRANCH}"):
        if worktree.exists():
            fail(f"{worktree} exists without {BRANCH}; use a fresh native-evidence fixture")
        if git(project, "status", "--porcelain"):
            fail(f"{project} has uncommitted changes; use a fresh native-evidence fixture")
        # As the app does for its own managed worktrees.
        exclude = Path(git(project, "rev-parse", "--path-format=absolute", "--git-path", "info/exclude"))
        exclude.parent.mkdir(parents=True, exist_ok=True)
        excluded = exclude.read_text(errors="replace").splitlines() if exclude.exists() else []
        if ".worktrees/" not in excluded:
            with exclude.open("a") as file:
                file.write(".worktrees/\n")
        base = commit(project, {"src/cache.rs": CACHE}, "Cache responses by path", "2026-11-18T09:00:00Z")
        git(project, "update-ref", "refs/remotes/origin/main", base)
        git(project, "worktree", "add", "--quiet", "-b", BRANCH, str(worktree), base)
        head = commit(worktree, {"src/cache.rs": CACHE_KEYED, "docs/caching.md": CACHING},
                      "Key the response cache by locale", "2026-11-18T09:40:00Z")
        # Pushed to origin, so the forge's seed is this worktree's pull request.
        git(project, "config", f"branch.{BRANCH}.remote", "origin")
        git(project, "config", f"branch.{BRANCH}.merge", f"refs/heads/{BRANCH}")
        git(project, "update-ref", f"refs/remotes/origin/{BRANCH}", head)
    head = git(project, "rev-parse", BRANCH)
    base = git(project, "rev-parse", "refs/remotes/origin/main")
    # The private mirrors a review's fetch writes; they keep both reachable.
    git(project, "update-ref", f"refs/cagents/pull/{NUMBER}/head", head)
    git(project, "update-ref", f"refs/cagents/pull/{NUMBER}/base", base)
    return {"base": base, "head": head, "merge_base": git(project, "merge-base", base, head)}


def pull_request(oids):
    """In the shape `gh pr list --json` prints and src/github.rs's PullRequest
    serializes, camelCase; see fixture-gh.sh."""
    return {
        "id": f"PR_fixture{NUMBER}", "number": NUMBER, "title": "Key the response cache by locale",
        "body": "", "state": "OPEN", "isDraft": False, "headRefName": BRANCH, "headRefOid": oids["head"],
        "headRepository": {"name": "fixture-app"},
        "headRepositoryOwner": {"login": "octo-org", "name": None, "is_bot": False},
        "baseRefName": "main", "baseRefOid": oids["base"],
        "author": {"login": "octocat", "name": None, "is_bot": False},
        "createdAt": "2026-11-18T09:42:00Z", "updatedAt": "2026-11-18T09:42:00Z",
        "url": f"{REMOTE}/pull/{NUMBER}", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
        "reviewDecision": "REVIEW_REQUIRED", "labels": [], "additions": 9, "deletions": 6,
        "changedFiles": 2, "statusCheckRollup": [],
    }


def path_bytes(path):
    """src/repository.rs's RepoPath serializes as its bytes, a JSON array."""
    return list(os.fsencode(path))


def diff_entries(project, oids):
    """Each changed path as src/repository.rs's DiffEntry, in Git's order."""
    kinds = {"A": "added", "M": "modified", "D": "deleted"}
    statuses = git(project, "diff", "--no-renames", "--name-status", oids["merge_base"], oids["head"])
    counts = git(project, "diff", "--no-renames", "--numstat", oids["merge_base"], oids["head"])
    lines = {}
    for row in counts.splitlines():
        added, removed, path = row.split("\t", 2)
        lines[path] = (int(added), int(removed))
    entries = []
    for row in statuses.splitlines():
        status, path = row.split("\t", 1)
        added, removed = lines[path]
        entries.append({"path": path_bytes(path), "old_path": None, "kind": kinds[status],
                        "added_lines": added, "removed_lines": removed})
    return entries


def snapshot(oids):
    """src/review_cache.rs's Snapshot without its files, as the store keeps it."""
    range_ = {
        "number": NUMBER, "base_branch": "main",
        "head_mirror": f"refs/cagents/pull/{NUMBER}/head", "base_mirror": f"refs/cagents/pull/{NUMBER}/base",
        "head": oids["head"], "base_tip": oids["base"], "merge_base": oids["merge_base"],
    }
    line = CACHE_KEYED.splitlines().index(ANCHORED) + 1
    draft = {
        # The authority the draft was written under; the review itself has none.
        "context": {"pull_request_id": f"PR_fixture{NUMBER}", "head_oid": oids["head"],
                    "base_oid": oids["base"], "viewer": VIEWER, "viewer_is_author": False, "pending": None},
        "range": range_,
        "destination": {"Line": {"path": "src/cache.rs", "side": "RIGHT", "start_line": line, "line": line}},
        "body": DRAFT,
    }
    return {
        "key": {"repository": REPOSITORY, "number": NUMBER},
        "viewer": VIEWER,
        "pull_request": pull_request(oids),
        "range": range_,
        "files": [],
        "unknown_paths": [],
        "selected": path_bytes("src/cache.rs"),
        "drafts": {"composer": draft, "summary": "", "event": "comment", "unknown": None},
        "drafts_changed": False,
        "drafts_owner": "fixture-offline-review",
        "drafts_version": "fixture-offline-review-1",
        "saved_ms": int(time.time() * 1000),
    }


def store_path(fixture):
    """The primary window's store, as src/instance.rs's window_database_path names it."""
    instance = os.environ.get("CAGENTS_DEVELOPMENT_INSTANCE", "")
    storage = "cagents-development" + (f"-{instance}" if instance else "")
    data = Path(os.environ.get("XDG_DATA_HOME") or fixture / "data")
    return data / storage / "sessions.sqlite3"


def write_cache(database, oids, entries):
    """One transaction, as src/persistence/review_cache.rs's save makes it."""
    record = json.dumps(snapshot(oids), separators=(",", ":"))
    files = [json.dumps(entry, separators=(",", ":")) for entry in entries]
    byte_size = len(record.encode()) + sum(len(entry.encode()) for entry in files)
    viewed = {"docs/caching.md"}
    connection = sqlite3.connect(database, timeout=30, isolation_level=None)
    try:
        tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
        if not {"review_cache", "review_cache_files"} <= tables:
            fail(f"{database} has no review cache; start a session with this side's app first")
        connection.execute("BEGIN IMMEDIATE")
        for table in ("review_cache_files", "review_cache"):
            connection.execute(f"DELETE FROM {table} WHERE repository = ? AND number = ?", (SELECTOR, NUMBER))
        (latest,) = connection.execute("SELECT coalesce(max(rowid), 0) FROM review_cache").fetchone()
        connection.execute(
            "INSERT INTO review_cache(rowid, repository, number, head, viewer, record, saved_ms,"
            " write_epoch, write_order, byte_size) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (latest + 1, SELECTOR, NUMBER, oids["head"], VIEWER, record, json.loads(record)["saved_ms"],
             WRITE_EPOCH, 0, byte_size))
        for position, (entry, text) in enumerate(zip(entries, files)):
            path = os.fsdecode(bytes(entry["path"]))
            connection.execute(
                "INSERT INTO review_cache_files(repository, number, head, path, position, entry, viewed,"
                " unverified) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (SELECTOR, NUMBER, oids["head"], path, position, text, int(path in viewed), 0))
        connection.execute("COMMIT")
    finally:
        connection.close()


def write(path, content):
    """Whole or not at all: the forge reads it on every call."""
    partial = path.with_name(path.name + ".next")
    partial.write_text(content)
    partial.replace(path)


def main():
    if len(sys.argv) > 1:
        fail(f"takes no arguments; run it as: scripts/native-evidence env python3 {sys.argv[0]}")
    fixture = os.environ.get("CAGENTS_EVIDENCE_FIXTURE")
    if not fixture:
        fail("run it in a native-evidence fixture: scripts/native-evidence env python3 " + sys.argv[0])
    fixture = Path(fixture).resolve()
    project = fixture / "project"
    home = fixture / "home"
    # Never write a store or forge files except inside an evidence fixture's HOME.
    if not ((fixture / "bin").is_dir() and (project / ".git").exists() and home.is_dir()
            and Path.home().resolve() == home):
        fail(f"{fixture} is not a native-evidence fixture whose HOME is this one; start a session first")
    database = store_path(fixture)
    if fixture not in database.resolve().parents:
        fail(f"{database} is outside the fixture {fixture}")
    if not database.is_file():
        fail(f"{database} does not exist yet; start a session with this side's app first")
    oids = prepare_commits(project)
    config = home / ".config"
    config.mkdir(exist_ok=True)
    # The switch goes on before the remote appears: a failed read while the
    # remote is there makes the app wait before asking again.
    (config / "fixture-forge").touch()
    seeds = config / "fixture-forge-prs.json"
    others = [seed for seed in (json.loads(seeds.read_text()) if seeds.exists() else [])
              if seed.get("number") != NUMBER or not seed.get("url", "").startswith(REMOTE + "/")]
    write(seeds, json.dumps([*others, pull_request(oids)], indent=1) + "\n")
    if "origin" not in git(project, "remote").split():
        git(project, "remote", "add", "origin", REMOTE)
    write_cache(database, oids, diff_entries(project, oids))
    print(json.dumps({"project": str(project), "worktree": str(project / ".worktrees" / "cache-key"),
                      "pull_request": f"{REMOTE}/pull/{NUMBER}", "store": str(database), **oids}))


if __name__ == "__main__":
    main()
