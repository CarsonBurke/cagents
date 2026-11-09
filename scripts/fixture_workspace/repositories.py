"""Real Git repositories for the workspace's projects.

History is written with `git fast-import` under a fixed author and fixed
dates, so commit IDs depend only on the seed. Git runs without the user's or
the system's configuration.
"""

import os
import shutil
import subprocess


AUTHOR = "Fixture Author <fixture@example.invalid>"

SOURCE_FILES = {
    "README.md": "# {name}\n\nAn invented project for cagents fixtures.\n",
    "src/lib.rs": "pub mod auth;\npub mod api;\n\npub fn version() -> &'static str {{\n    \"{name} {revision}\"\n}}\n",
    "src/auth.rs": "pub fn refresh(token: &str) -> Option<String> {{\n    (!token.is_empty()).then(|| format!(\"{{token}}-{revision}\"))\n}}\n",
    "src/api.rs": "pub fn orders(cart: &[u64]) -> u64 {{\n    cart.iter().sum::<u64>() + {revision}\n}}\n",
    "tests/api.rs": "#[test]\nfn empty_cart_totals_zero() {{\n    assert_eq!({name_ident}::api::orders(&[]), {revision});\n}}\n",
    "docs/runbook.md": "# Runbook\n\nRevision {revision}.\n",
}

COMMIT_MESSAGES = [
    "Initial commit", "Add the refresh path", "Batch the address query", "Fix the empty cart total",
    "Document the on-call runbook", "Add rate limit tests", "Bump dependencies", "Handle the error path",
]


def environment(root, epoch):
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": os.path.join(root, "home"),
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_SYSTEM": os.devnull,
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_AUTHOR_NAME": "Fixture Author",
        "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
        "GIT_COMMITTER_NAME": "Fixture Author",
        "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        "GIT_AUTHOR_DATE": f"{epoch} +0000",
        "GIT_COMMITTER_DATE": f"{epoch} +0000",
        "LC_ALL": "C",
    }


def git(env, *arguments, cwd, stdin=None):
    result = subprocess.run(["git", *arguments], cwd=cwd, env=env, input=stdin, capture_output=True)
    if result.returncode != 0:
        raise SystemExit(f"git {' '.join(arguments)} failed in {cwd}:\n{result.stderr.decode(errors='replace')}")
    return result.stdout.decode()


def history(name, commits, start):
    """A fast-import stream of `commits` commits on main, a day apart."""
    ident = name.replace("-", "_")
    out = []
    for revision in range(commits):
        when = start + revision * 86400
        message = COMMIT_MESSAGES[revision % len(COMMIT_MESSAGES)]
        if revision >= len(COMMIT_MESSAGES):
            message += f" ({revision})"
        out.append("commit refs/heads/main\n")
        out.append(f"committer {AUTHOR} {when} +0000\n")
        data = message.encode()
        out.append(f"data {len(data)}\n{message}\n")
        if revision == 0:
            out.append("deleteall\n")
        for path, template in SOURCE_FILES.items():
            # Each revision touches a few files, as real history does.
            if revision and (hash_index(path) + revision) % 3:
                continue
            content = template.format(name=name, name_ident=ident, revision=revision).encode()
            out.append(f"M 100644 inline {path}\ndata {len(content)}\n")
            out.append(content.decode() + "\n")
    return "".join(out).encode()


def hash_index(text):
    return sum(text.encode())


def create(root, project, epoch):
    """Creates one project's checkout and worktrees as the plan describes, with
    history that ends a month before `epoch`."""
    env = environment(root, epoch)
    checkout = project["checkout"]
    os.makedirs(checkout, exist_ok=True)
    git(env, "init", "--quiet", "--initial-branch=main", cwd=checkout)
    commits = project["commits"]
    git(env, "fast-import", "--quiet", cwd=checkout,
        stdin=history(project["name"], commits, epoch - commits * 86400 - 30 * 86400))
    git(env, "reset", "--quiet", "--hard", "main", cwd=checkout)
    with open(os.path.join(checkout, ".git", "info", "exclude"), "a") as exclude:
        exclude.write(".worktrees/\n")
    if project.get("remote"):
        git(env, "remote", "add", "origin", project["remote"], cwd=checkout)
    for worktree in project["worktrees"]:
        if worktree["path"] == checkout:
            continue
        # Branches start a little behind main, as feature branches do.
        base = f"main~{min(commits - 1, 1 + hash_index(worktree['branch']) % 3)}"
        git(env, "worktree", "add", "--quiet", "-b", worktree["branch"], worktree["path"], base, cwd=checkout)
        if worktree["removed"]:
            # Merged and cleaned up: the path is gone and Git has forgotten it,
            # but the app's history and shared views still name it.
            git(env, "worktree", "remove", "--force", worktree["path"], cwd=checkout)
    for path in project.get("dirty", ()):
        dirty(path)


def dirty(path):
    """Uncommitted work: a modified tracked file and a new one."""
    with open(os.path.join(path, "src", "api.rs"), "a") as file:
        file.write("\n// TODO: handle the empty cart before totals.\n")
    with open(os.path.join(path, "notes.md"), "w") as file:
        file.write("Scratch notes, not committed.\n")


def remove_tree(path):
    if os.path.lexists(path):
        shutil.rmtree(path)
