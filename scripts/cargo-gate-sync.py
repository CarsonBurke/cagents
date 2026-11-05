#!/usr/bin/env python3
"""Install origin/main's Cargo gate when main has changed it since the
installed one.

Usage: cargo-gate-sync.py INSTALLED MAIN

INSTALLED is the installed gate (/var/tmp/cagents-cargo-gate) and MAIN the
commit origin/main names in the current directory's repository, which the gate
found to differ from the last one judged. Exits 0 when it installed a gate,
1 when there was nothing to install or another gate is installing, and 2 when
installing failed, which it reports.

The installed gate is main's to replace when its files differ from main's and
main changed the gate after it was installed. One installed later, such as a
branch's under test, stays until main changes the gate again. Only checkouts
of the repository it was installed from act on it, so another clone's or a
fork's main never replaces it.
"""

import fcntl
import json
import os
import subprocess
import sys
import tempfile
import time

INSTALLER = "scripts/install-cargo-gate"
# What the gate needs beside it, should a record not say.
GATE_FILES = ["scripts/cargo-gate", "scripts/cargo-gate-command.py", "scripts/cargo-gate-sync.py",
              "scripts/cargo-offload", "scripts/seed-build-dir", "scripts/build-disk-guard",
              INSTALLER]


def git(*arguments):
    result = subprocess.run(["git", *arguments], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


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

    def judge(self, main, repository):
        """What to do about MAIN: "current", "keep" or "install"."""
        record = self.record()
        if record.get("repository") and record["repository"] != repository:
            return "keep"  # Another clone's or a fork's main.
        files = record.get("files")
        paths = sorted(files) if isinstance(files, dict) and files else list(GATE_FILES)
        if INSTALLER not in paths:
            paths.append(INSTALLER)
        blobs = git("rev-parse", *[f"{main}:{path}" for path in paths])
        if blobs is not None and isinstance(files, dict) and \
                dict(zip(paths, blobs.split())) == files:
            return "current"
        changed = git("log", "-1", "--format=%ct", main, "--", *paths)
        if not changed or int(changed) <= record["installed_at"]:
            return "keep"  # Installed after main last changed the gate.
        return "install"

    def install(self, main, repository):
        with tempfile.TemporaryDirectory(prefix="cagents-gate-sync-") as stage:
            archive = os.path.join(stage, "scripts.tar")
            steps = [["git", "archive", "-o", archive, main, "scripts"],
                     ["tar", "-x", "-m", "-f", archive, "-C", stage],
                     ["sh", os.path.join(stage, INSTALLER), os.path.dirname(self.installed)]]
            environment = dict(os.environ, CAGENTS_BUILD_INSTALL_LOCK_HELD="1",
                               CAGENTS_BUILD_INSTALL_COMMIT=main,
                               CAGENTS_BUILD_INSTALL_REPOSITORY=repository)
            output = []
            for step in steps:
                result = subprocess.run(step, capture_output=True, text=True, env=environment)
                output.append(result.stdout + result.stderr)
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
    repository = git("rev-parse", "--path-format=absolute", "--git-common-dir")
    # Not a checkout of the repository the gate belongs to.
    if repository is None or git("cat-file", "-e", f"{main}:{INSTALLER}") is None:
        return 1
    if installation.judge(main, repository) != "install":
        installation.checked(main)
        return 1
    failed = read_json(installation.failed_path)
    if isinstance(failed, dict) and failed.get("main") == main:
        say(f"{installed} is older than origin/main's gate, which could not be installed; "
            f"{installation.failed_path} says why")
        return 1
    with open(installation.lock_path, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 1  # Another gate or the installer is installing.
        # Decided again under the lock: another gate may have installed
        # meanwhile, or origin/main moved.
        latest = git("rev-parse", "-q", "--verify", "refs/remotes/origin/main^{commit}") or main
        verdict = installation.judge(latest, repository)
        if verdict != "install":
            installation.checked(latest)
            # Current now: another gate installed it while this one waited.
            return 0 if verdict == "current" else 1
        started = time.monotonic()
        installed_ok, output = installation.install(latest, repository)
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
