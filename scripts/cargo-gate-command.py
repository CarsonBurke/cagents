#!/usr/bin/python3
"""Supervise one owned command, and keep the gate's admission clock.

    cargo-gate-command.py [--wrapper NAME] [--admission-until DEADLINE] RECEIPT COMMAND...

--wrapper NAME supervises a whole program on its caller's behalf, as
native-evidence-remote does itself: messages are NAME's, a cancelled run
dies by the signal that cancelled it, so a calling shell stops as it would
for the program, and the program gets longer than the gate's own grace to
clean up, as it may be waiting on a gate that is draining.
"""

import sys
import time


GRACE_SECONDS = 2.0
# Longer than GRACE_SECONDS, so that a gate the wrapped program runs drains
# and cleans up before this kills it.
WRAPPER_GRACE_SECONDS = 5.0
PROGRAM = "cargo-gate"


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


POLL_SECONDS = 0.02
# inotify_init1's flags are the open(2) flags of the same names.
INOTIFY_FLAGS = 0o4000 | 0o2000000  # IN_NONBLOCK | IN_CLOEXEC
# A receipt is created, written and closed, or renamed into place.
RECEIPT_EVENTS = 0x2 | 0x8 | 0x80 | 0x100  # MODIFY, CLOSE_WRITE, MOVED_TO, CREATE


class Wakeups:
    """Sleep until something the supervision loop checks may have changed.

    Supervising a build used to wake fifty times a second for as long as it
    ran, and a gate's waiting commands each kept one of these. What the loop
    checks is announced by the kernel instead: the primary's exit by its
    pidfd, an adopted descendant's exit and a direct cancellation by a signal,
    through the signal wakeup descriptor, and the receipt by inotify on its
    directory. The only timeout left is the admission deadline. Whatever is
    unavailable falls back to the former interval, never to sleeping on.
    poll(), unlike select(), takes descriptors above FD_SETSIZE, which a
    caller with many inherited descriptors leaves these with.
    """

    def __init__(self, receipt, ctypes, libc, os, signal):
        import select
        self.os, self.select, self.poller = os, select, select.poll()
        self.descriptors = []
        self.polling = False
        reader, writer = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
        self.descriptors.append(reader)
        # PEP 475 retries a select a handler interrupted, so a handler alone
        # wakes nothing: the wakeup descriptor makes the signal readable.
        signal.set_wakeup_fd(writer, warn_on_full_buffer=False)
        # A handler, unlike SIG_IGN, still leaves children to be reaped, and
        # exec restores the default for the command.
        signal.signal(signal.SIGCHLD, lambda _number, _frame: None)
        self.receipt_watch = None
        try:
            # /dev/null, as a wrapper passes, is never written: nothing to watch.
            if not receipt.exists() or receipt.is_file():
                watch = libc.inotify_init1(INOTIFY_FLAGS)
                if watch < 0:
                    raise OSError(ctypes.get_errno(), "inotify unavailable")
                if libc.inotify_add_watch(watch, bytes(receipt.parent), RECEIPT_EVENTS) < 0:
                    error = ctypes.get_errno()
                    os.close(watch)
                    raise OSError(error, "cannot watch the receipt")
                self.receipt_watch = watch
                self.descriptors.append(watch)
        except (AttributeError, OSError):
            # AttributeError: a libc without inotify.
            self.polling = True
        for descriptor in self.descriptors:
            self.poller.register(descriptor, select.POLLIN)

    def watch_exit(self, pid):
        try:
            pidfd = self.os.pidfd_open(pid)
        except (AttributeError, OSError):
            # SIGCHLD still wakes us, but a missed one must not hang the loop.
            self.polling = True
        else:
            self.poller.register(pidfd, self.select.POLLIN)

    def wait(self, deadline):
        timeout = POLL_SECONDS if self.polling else None
        if deadline is not None:
            remaining = max(0, deadline - time.monotonic_ns()) / 1e9
            timeout = remaining if timeout is None else min(timeout, remaining)
        # poll() takes milliseconds; round up, so a deadline is not missed early.
        milliseconds = None if timeout is None else int(timeout * 1000) + 1
        for descriptor, _events in self.poller.poll(milliseconds):
            if descriptor == self.descriptors[0] or descriptor == self.receipt_watch:
                try:
                    while self.os.read(descriptor, 4096):
                        pass
                except BlockingIOError:
                    pass
        # A readable pidfd stays readable; the loop finds the exit next.


def main():
    # Clock-only admission checks do not need command supervision's imports.
    import ctypes
    import errno
    import os
    from pathlib import Path
    import signal
    import subprocess

    global PROGRAM
    arguments = sys.argv[1:]
    wrapper = False
    grace = GRACE_SECONDS
    if arguments[:1] == ["--wrapper"]:
        wrapper, PROGRAM, grace = True, arguments[1], WRAPPER_GRACE_SECONDS
        arguments = arguments[2:]
    signals = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)
    # A signal the caller ignores, as nohup ignores HUP, stays ignored here and
    # in the command: it cancels nothing. The gate starts this as a background
    # job, for which its shell, not its caller, ignores INT and QUIT; the gate
    # traps those itself and writes them to the receipt, which any signal of
    # these can still cancel through.
    kept = signals if wrapper else (signal.SIGTERM, signal.SIGHUP)
    handled = [number for number in signals
               if number not in kept or signal.getsignal(number) != signal.SIG_IGN]
    admission_deadline = None
    if arguments[:1] == ["--admission-until"]:
        admission_deadline = positive_integer(arguments[1])
        arguments = arguments[2:]
    cancellation = 0

    def cancelled(number, _frame):
        nonlocal cancellation
        if not cancellation:
            cancellation = number

    for number in handled:
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

    def cancelled_status():
        if wrapper:
            # Dies as the program would have, once nothing of it is left.
            signal.signal(cancellation, signal.SIG_DFL)
            os.kill(os.getpid(), cancellation)
        return 128 + cancellation

    # Watched before the first read, so a receipt written after it wakes us.
    wakeups = Wakeups(receipt, ctypes, libc, os, signal)
    if cancellation_requested():
        return cancelled_status()
    if admission_expired(admission_deadline):
        return 124
    try:
        # Preserve the caller's terminal/session and inherited descriptors;
        # the gate has already closed its own 8 and 9. Cleanup signals only
        # actual unreaped children, never the caller's shared process group.
        primary = subprocess.Popen(arguments[1:], close_fds=False)
    except OSError as error:
        print(f"{PROGRAM}: {error}", file=sys.stderr)
        return 127 if error.errno == errno.ENOENT else 126

    def primary_exited():
        # Do not poll(), send_signal(), or wait() the Popen here: all may reap
        # the primary and allow its PID to be reused before cleanup ends.
        return os.waitid(os.P_PID, primary.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)

    def adopted_children():
        return [pid for pid in map(int, children_path.read_text().split()) if pid != primary.pid]

    def reap_exited_adoptees():
        # Descendants whose parents exit are adopted by this subreaper, such
        # as the intermediate process and then the master of an offload's
        # ssh ControlPersist connection. Left unreaped, each that finishes
        # stays a zombie for the rest of the build.
        # Reap only listed PIDs that have already exited, never waitpid(-1):
        # the primary stays unreaped to reserve its PID, and live adoptees
        # keep theirs for drain().
        for pid in adopted_children():
            os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG)

    def send(pid, number):
        try:
            os.kill(pid, number)
        except ProcessLookupError:
            pass

    def drain():
        deadline = time.monotonic() + grace
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
            for pid in adopted_children():
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

    wakeups.watch_exit(primary.pid)
    try:
        while True:
            if cancellation_requested():
                drain()
                return cancelled_status()
            if admission_expired(admission_deadline):
                drain()
                return 124
            if primary_exited():
                if cancellation_requested():
                    continue
                # Normal completion commits here. Background cache services
                # may outlive successful commands; only cancellation drains.
                status = primary.wait()
                return status if status >= 0 else 128 - status
            reap_exited_adoptees()
            wakeups.wait(admission_deadline)
    except BaseException:
        # A supervisor failure after launch must not release a live build.
        drain()
        raise


if __name__ == "__main__":
    try:
        if sys.argv[1:2] in (["--admission-deadline"], ["--admission-check"]):
            sys.exit(admission_clock(sys.argv[1:]))
        sys.exit(main())
    except Exception as error:
        print(f"{PROGRAM}: command supervision failed: {error}", file=sys.stderr)
        sys.exit(2)
