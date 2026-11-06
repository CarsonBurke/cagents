"""Fixture directories that outlive every process their tests start.

Importing this makes the test runner a child subreaper, so a fixture's
orphaned descendants, even those in sessions of their own, become its
children instead of init's. Closing a directory ends its commands' groups,
then kills and reaps every adopted orphan before removing the directory.
"""

import atexit
import contextlib
import ctypes
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

PR_SET_CHILD_SUBREAPER = 36
GRACE_SECONDS = 2
SETTLE_SECONDS = 30

# Commands still started through a fixture directory, whose statuses belong
# to their Popen objects rather than to the sweep.
_tracked = set()


def adopt_orphans():
    if ctypes.CDLL(None, use_errno=True).prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0):
        raise OSError(ctypes.get_errno(), "cannot adopt orphaned fixture descendants")


def _children():
    pids = set()
    for task in Path("/proc/self/task").iterdir():
        with contextlib.suppress(FileNotFoundError):  # The thread ended.
            pids.update(map(int, (task / "children").read_text().split()))
    return pids


def _stop(processes):
    """Ends each running command's process group: SIGTERM, a short grace, then SIGKILL.

    A reaped leader's pid may already belong to another group, so only
    leaders not yet reaped are signalled; the sweep ends their groups' rest.
    """
    for number in (signal.SIGTERM, signal.SIGKILL):
        for process in processes:
            if process.poll() is not None:
                continue
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.killpg(process.pid, number)
        deadline = time.monotonic() + GRACE_SECONDS
        for process in processes:
            with contextlib.suppress(subprocess.TimeoutExpired):
                process.wait(max(0, deadline - time.monotonic()))
    for process in processes:
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream:
                stream.close()


def settle(timeout=SETTLE_SECONDS):
    """Kills and reaps adopted orphans until none are left.

    Each pid is reaped by name, never by waitpid(-1), so tracked commands
    keep their statuses. An orphan's own children are adopted as it dies,
    so the sweep repeats until a pass finds nothing.
    """
    deadline = time.monotonic() + timeout
    while strays := _children() - {process.pid for process in _tracked if process.returncode is None}:
        if time.monotonic() > deadline:
            raise RuntimeError(f"fixture descendants {sorted(strays)} did not settle")
        for pid in strays:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, os.WNOHANG)
        time.sleep(0.01)


class FixtureDirectory:
    """A temporary directory removed only once nothing started for it runs."""

    def __init__(self, prefix="cagents-fixture-", dir=None):
        self.name = tempfile.mkdtemp(prefix=prefix, dir=dir)
        self.processes = []
        self.closed = False

    def popen(self, argv, **options):
        options.setdefault("start_new_session", True)
        process = subprocess.Popen(argv, **options)
        self.processes.append(process)
        _tracked.add(process)
        return process

    def run(self, argv, *, input=None, capture_output=False, timeout=None, check=False, **options):
        if input is not None:
            options["stdin"] = subprocess.PIPE
        if capture_output:
            options.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        process = self.popen(argv, **options)
        try:
            stdout, stderr = process.communicate(input, timeout)
        except BaseException:
            _stop([process])
            raise
        if check and process.returncode:
            raise subprocess.CalledProcessError(process.returncode, argv, stdout, stderr)
        return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)

    def close(self):
        if self.closed:
            return
        _stop(self.processes)
        _tracked.difference_update(self.processes)
        settle()
        shutil.rmtree(self.name)
        self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *exception):
        self.close()


adopt_orphans()
atexit.register(settle)
