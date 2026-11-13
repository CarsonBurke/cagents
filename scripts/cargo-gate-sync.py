#!/usr/bin/python3
"""Install origin/main's Cargo gate when main has changed it since the
installed one.

Usage: cargo-gate-sync.py INSTALLED MAIN

INSTALLED is the installed gate (/var/tmp/cagents-cargo-gate) and MAIN the
commit origin/main names in the current directory's repository, which the gate
found to differ from the last one judged. Exits 0 when it installed a gate,
1 when there was nothing to install or another gate is installing, and 2 when
installing failed, which it reports.

The installed gate is main's to replace when its files differ from main's
and either main contains the commit it was installed from or main changed
the gate after it was installed. One installed from elsewhere, such as a
branch under test, stays until main changes the gate again; so does one of a
newer gate_version than main's. Only checkouts of the repository it came
from act on it, so a fork's main never replaces it.
"""

import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.parse

INSTALLER = "scripts/install-cargo-gate"
# What the gate needs beside it, should a record not say.
GATE_FILES = ["scripts/cargo-gate", "scripts/cargo-gate-command.py", "scripts/cargo-gate-sync.py",
              "scripts/cargo-host-test-helper.py",
              "scripts/cargo-gate-rustc-guard",
              "scripts/cargo-offload", "scripts/seed-build-dir", "scripts/build-disk-guard",
              "scripts/memory-capped", "scripts/prune-build-dirs", INSTALLER]


def git(*arguments):
    result = subprocess.run(["git", *arguments], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


def same_repository(first, second):
    """Whether two origin URLs name one repository, however they reach it:
    by HTTPS or SSH, in either SSH form, with or without a port, user or
    `.git`. Hosts such as GitHub ignore case, so this does too."""
    def plain(url):
        url = url.strip()
        if "://" in url:
            parts = urllib.parse.urlsplit(url)
            host, path = parts.hostname or "", parts.path
        else:  # scp-like: [user@]host:path
            host, _, path = url.rpartition("@")[2].partition(":")
        return host.lower(), path.strip("/").removesuffix(".git").lower()
    return plain(first) == plain(second)


def gate_version(text):
    match = re.search(r"^gate_version=([0-9]+)$", text or "", re.M)
    return int(match.group(1)) if match else 0


def read_text(path):
    try:
        with open(path) as file:
            return file.read()
    except OSError:
        return None


def read_json(path):
    try:
        with open(path) as file:
            return json.load(file)
    except (OSError, ValueError):
        return None


def write_atomically(path, text):
    directory = os.path.dirname(path)
    descriptor, temporary = tempfile.mkstemp(prefix=".sync.", dir=directory)
    try:
        with os.fdopen(descriptor, "w") as file:
            file.write(text)
            os.fchmod(file.fileno(), 0o644)
        os.replace(temporary, path)
    except OSError:
        try:
            os.unlink(temporary)
        except OSError:
            pass


class Installation:
    def __init__(self, installed):
        self.installed = installed
        self.record_path = installed + ".installation"
        self.checked_path = installed + ".checked"
        self.failed_path = installed + ".install-failed"
        self.lock_path = installed + ".install.lock"

    def record(self):
        record = read_json(self.record_path)
        if not isinstance(record, dict):
            record = {}
        if not isinstance(record.get("installed_at"), (int, float)):
            # Installed before installations were recorded: when its gate
            # file was put there.
            try:
                record["installed_at"] = os.stat(self.installed).st_mtime
            except OSError:
                record["installed_at"] = 0
        return record

    def checked(self, main):
        write_atomically(self.checked_path, main + "\n")

    def judge(self, main, repository, origin):
        """What to do about MAIN: "current", "keep" or "install"."""
        record = self.record()
        if record.get("origin") and origin:
            if not same_repository(record["origin"], origin):
                return "keep"  # A fork's main, or another project's.
        elif record.get("repository") and record["repository"] != repository:
            return "keep"  # Another clone's main, which cannot say where it came from.
        files = record.get("files")
        paths = sorted(files) if isinstance(files, dict) and files else list(GATE_FILES)
        if INSTALLER not in paths:
            paths.append(INSTALLER)
        blobs = git("rev-parse", *[f"{main}:{path}" for path in paths])
        if blobs is not None and isinstance(files, dict) and \
                dict(zip(paths, blobs.split())) == files:
            return "current"
        if gate_version(git("show", f"{main}:scripts/cargo-gate")) < \
                gate_version(read_text(self.installed)):
            return "keep"  # Installed from a newer gate than main's.
        commit = record.get("commit")
        if commit and git("merge-base", "--is-ancestor", commit, main) is not None:
            return "install"  # Main has what was installed and has changed it since.
        # Installed from elsewhere, such as a branch under test: it stays
        # until main changes the gate after it was installed. Main's own
        # history dates each change by when it reached main, not when the
        # branch that made it was written.
        changed = git("log", "-1", "--first-parent", "--format=%ct", main, "--", *paths)
        if not changed or int(changed) <= record["installed_at"]:
            return "keep"
        return "install"

    def install(self, main, repository, origin):
        with tempfile.TemporaryDirectory(prefix="cagents-gate-sync-") as stage:
            archive = os.path.join(stage, "scripts.tar")
            steps = [["git", "archive", "-o", archive, main, "scripts"],
                     ["tar", "-x", "-m", "-f", archive, "-C", stage],
                     ["sh", os.path.join(stage, INSTALLER), os.path.dirname(self.installed)]]
            environment = dict(os.environ, CAGENTS_BUILD_INSTALL_LOCK_HELD="1",
                               CAGENTS_BUILD_INSTALL_COMMIT=main,
                               CAGENTS_BUILD_INSTALL_REPOSITORY=repository,
                               CAGENTS_BUILD_INSTALL_ORIGIN=origin or "")
            output = []
            for step in steps:
                result = subprocess.run(step, capture_output=True, text=True, env=environment)
                output.append(result.stdout + result.stderr)
                if result.returncode == 3 and step[0] == "sh":
                    return None, "".join(output)  # The installed gate is a newer version.
                if result.returncode:
                    return False, "".join(output)
        record = read_json(self.record_path)
        if not isinstance(record, dict) or record.get("commit") != main:
            # An installer from before installations were recorded, which
            # every gate would otherwise run again.
            return False, "".join(output) + "main's installer did not record what it installed\n"
        return True, "".join(output)


def say(message):
    print(f"cargo-gate: {message}", file=sys.stderr, flush=True)


def main(installed, main):
    installation = Installation(installed)
    failed = read_json(installation.failed_path)
    if isinstance(failed, dict) and failed.get("main") == main:
        say(f"{installed} is not origin/main's gate, which could not be installed; "
            f"{installation.failed_path} says why")
        return 1
    repository = git("rev-parse", "--path-format=absolute", "--git-common-dir")
    # Not a checkout of the repository the gate belongs to.
    if repository is None or git("cat-file", "-e", f"{main}:{INSTALLER}") is None:
        return 1
    origin = git("remote", "get-url", "origin")
    if installation.judge(main, repository, origin) != "install":
        installation.checked(main)
        return 1
    with open(installation.lock_path, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 1  # Another gate or the installer is installing.
        # Decided again under the lock: another gate may have installed
        # meanwhile, or origin/main moved.
        latest = git("rev-parse", "-q", "--verify", "refs/remotes/origin/main^{commit}") or main
        verdict = installation.judge(latest, repository, origin)
        if verdict != "install":
            installation.checked(latest)
            # Current now: another gate installed it while this one waited.
            return 0 if verdict == "current" else 1
        started = time.monotonic()
        installed_ok, output = installation.install(latest, repository, origin)
        if installed_ok is None:
            # Installed later from a newer gate than main's: it stays.
            installation.checked(latest)
            return 1
        if not installed_ok:
            write_atomically(installation.failed_path, json.dumps(
                {"main": latest, "output": output}, indent=1) + "\n")
            say(f"could not install origin/main's gate ({latest[:12]}) as {installed}:\n"
                + output.rstrip())
            return 2
        try:
            os.unlink(installation.failed_path)
        except FileNotFoundError:
            pass
        installation.checked(latest)
        say(f"installed origin/main's gate ({latest[:12]}) as {installed} "
            f"in {time.monotonic() - started:.1f}s")
        return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2]))
