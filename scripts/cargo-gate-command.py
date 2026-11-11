#!/usr/bin/python3
"""Supervise one owned command without passing build-slot descriptors on."""

import sys
import time


GRACE_SECONDS = 2.0


def positive_integer(value):
    if not value.isascii() or not value.isdecimal() or int(value) <= 0:
        raise ValueError("expected a positive integer")
    return int(value)


def admission_clock(arguments):
    if len(arguments) != 2:
        raise ValueError("invalid admission clock arguments")
    if arguments[0] == "--admission-deadline":
        try:
            seconds = positive_integer(arguments[1])
        except ValueError:
            print("cargo-gate: CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS must be a positive integer", file=sys.stderr)
            return 2
        print(time.monotonic_ns() + seconds * 1_000_000_000)
        return 0
    deadline = positive_integer(arguments[1])
    return admission_expired(deadline)


def admission_expired(deadline):
    if deadline is not None and time.monotonic_ns() >= deadline:
        print("cargo-gate: build admission timed out", file=sys.stderr)
        return 124
    return 0


def wait_lock(arguments):
    """Acquire one shared lock description without outliving its gate."""
    import ctypes
    import errno
    import fcntl
    import os
    import signal

    if len(arguments) != 3:
        raise ValueError("invalid lock waiter arguments")
    parent = positive_integer(arguments[0])
    descriptor = positive_integer(arguments[1])
    deadline = positive_integer(arguments[2]) if arguments[2] else None
    if descriptor <= 2:
        raise ValueError("lock descriptor must be above standard streams")
    libc = ctypes.CDLL(None, use_errno=True)
    # Only admission waiters die with the gate. Successful build commands may
    # intentionally leave background services, so their supervisor stays separate.
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0):  # PR_SET_PDEATHSIG
        raise OSError(ctypes.get_errno(), "cannot bind lock waiter to gate")
    # Covers death before prctl, including death while Python was starting.
    if os.getppid() != parent:
        return 125
    # The gate retains the caller's descriptors for the admitted command. The
    # waiter needs only this shared lock description; don't pin caller locks or
    # pipes, including descriptors above the shell's single-digit range.
    for value in os.listdir("/proc/self/fd"):
        number = int(value)
        if number > 2 and number != descriptor:
            try:
                os.close(number)
            except OSError as error:
                if error.errno != errno.EBADF:
                    raise

    def cancelled(number, _frame):
        raise SystemExit(128 + number)

    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
        signal.signal(number, cancelled)

    def expired(_number, _frame):
        print("cargo-gate: build admission timed out", file=sys.stderr)
        raise SystemExit(124)

    if deadline is not None:
        if admission_expired(deadline):
            return 124
        signal.signal(signal.SIGALRM, expired)
        signal.setitimer(signal.ITIMER_REAL, max(0.000001, (deadline - time.monotonic_ns()) / 1_000_000_000))
    # Keep kernel FIFO ordering: a polling LOCK_NB request could lose every
    # release to plain flock callers. The gate shares this open description.
    fcntl.flock(descriptor, fcntl.LOCK_EX)
    return 0


def main():
    # Clock-only admission checks do not need command supervision's imports.
    import ctypes
    import errno
    import os
    from pathlib import Path
    import signal
    import subprocess

    signals = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)
    arguments = sys.argv[1:]
    admission_deadline = None
    if arguments[:1] == ["--admission-until"]:
        admission_deadline = positive_integer(arguments[1])
        arguments = arguments[2:]
    cancellation = 0

    def cancelled(number, _frame):
        nonlocal cancellation
        if not cancellation:
            cancellation = number

    for number in signals:
        signal.signal(number, cancelled)
    # An inherited SIGCHLD=SIG_IGN must not auto-reap our owned children.
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    receipt = Path(arguments[0])
    children_path = Path(f"/proc/self/task/{os.getpid()}/children")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0):  # PR_SET_CHILD_SUBREAPER, before any spawn.
        raise OSError(ctypes.get_errno(), "cannot supervise command descendants")
    children_path.read_text()  # Fail before launch if child enumeration is unavailable.

    def cancellation_requested():
        nonlocal cancellation
        if not cancellation:
            try:
                number = int(receipt.read_text())
            except (FileNotFoundError, ValueError):
                pass  # The outer gate may be in the middle of writing its receipt.
            else:
                if number in signals:
                    cancellation = number
        return cancellation

    if cancellation_requested():
        return 128 + cancellation
    if admission_expired(admission_deadline):
        return 124
    try:
        # Preserve the caller's terminal/session and inherited descriptors;
        # the gate has already closed its own 8 and 9. Cleanup signals only
        # actual unreaped children, never the caller's shared process group.
        primary = subprocess.Popen(arguments[1:], close_fds=False)
    except OSError as error:
        print(f"cargo-gate: {error}", file=sys.stderr)
        return 127 if error.errno == errno.ENOENT else 126

    def primary_exited():
        # Do not poll(), send_signal(), or wait() the Popen here: all may reap
        # the primary and allow its PID to be reused before cleanup ends.
        return os.waitid(os.P_PID, primary.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)

    def send(pid, number):
        try:
            os.kill(pid, number)
        except ProcessLookupError:
            pass

    def reap_adopted():
        # Subreaping also makes us responsible for orphans that exit during
        # a running command. Leave live services alone and retain the primary
        # as cancellation's PID anchor until its final wait().
        for value in children_path.read_text().split():
            pid = int(value)
            if pid != primary.pid:
                os.waitpid(pid, os.WNOHANG)

    def drain():
        deadline = time.monotonic() + GRACE_SECONDS
        terminating = set()
        killing = set()
        # Retaining the primary reserves its PID until the last signal. Its
        # descendants become our children when their parents exit; even a
        # parent that ignores TERM is killed after the grace period.
        send(primary.pid, signal.SIGTERM)
        send(primary.pid, signal.SIGCONT)
        while True:
            force = time.monotonic() >= deadline
            if force:
                send(primary.pid, signal.SIGKILL)
            children = [int(value) for value in children_path.read_text().split()]
            others = [pid for pid in children if pid != primary.pid]
            for pid in others:
                # These are this supervisor's actual unreaped children. With
                # SIGCHLD at default and one waiter, their IDs cannot be reused
                # before this loop reaps them. Adoption includes setsid children.
                if force and pid not in killing:
                    send(pid, signal.SIGKILL)
                    killing.add(pid)
                elif pid not in terminating:
                    send(pid, signal.SIGTERM)
                    send(pid, signal.SIGCONT)
                    terminating.add(pid)
                reaped, _status = os.waitpid(pid, os.WNOHANG)
                if reaped:
                    terminating.discard(pid)
                    killing.discard(pid)
            # Reaping a parent may adopt another generation; rescan before
            # declaring completion. Never reap the anchor via waitpid(-1).
            if primary_exited() and children_path.read_text().split() == [str(primary.pid)]:
                primary.wait()
                return
            time.sleep(0.02)

    try:
        while True:
            if cancellation_requested():
                drain()
                return 128 + cancellation
            if admission_expired(admission_deadline):
                drain()
                return 124
            reap_adopted()
            if primary_exited():
                if cancellation_requested():
                    continue
                # Normal completion commits here. Background cache services
                # may outlive successful commands; only cancellation drains.
                status = primary.wait()
                return status if status >= 0 else 128 - status
            time.sleep(0.02)
    except BaseException:
        # A supervisor failure after launch must not release a live build.
        drain()
        raise


if __name__ == "__main__":
    try:
        if sys.argv[1:2] in (["--admission-deadline"], ["--admission-check"]):
            sys.exit(admission_clock(sys.argv[1:]))
        if sys.argv[1:2] == ["--wait-lock"]:
            try:
                sys.exit(wait_lock(sys.argv[2:]))
            except Exception as error:
                print(f"cargo-gate: lock waiter failed: {error}", file=sys.stderr)
                sys.exit(2)
        sys.exit(main())
    except Exception as error:
        print(f"cargo-gate: command supervision failed: {error}", file=sys.stderr)
        sys.exit(2)
