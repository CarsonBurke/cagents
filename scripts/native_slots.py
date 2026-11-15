#!/usr/bin/python3
"""Take one of the build host's native session slots, in about the order asked.

Native evidence sessions and update rehearsals each run a private desktop.
They share N slots: slot 0 is the original lock file, which tools from before
slots queue on with a plain `flock`, and slot K is `LOCK.K`. Waiters line up
on `LOCK.queue` in the kernel's order. The one at its head stays queued for
slot 0 too, in a child that shares its descriptor, so it keeps its place
among plain `flock` waiters there, and polls the other slots meanwhile. The
order is not strict: a release wakes every blocked waiter, so one arriving at
that moment can take the queue or slot 0 ahead of them.

The caller opens every descriptor and owns the open files: a lock taken here
is the caller's, and stays after this exits. Only the chosen slot remains
locked; the queue and every other slot are released before this returns.
"""
import argparse
import fcntl
import os
import select
import signal
import subprocess
import sys
import time

DEFAULT_SLOTS = 3
MAXIMUM_SLOTS = 8
POLL_SECONDS = 0.2

# Exit statuses the shell callers read; a crash exits 1, so neither is that.
BUSY = 69
CANCELLED = 75


def slot_count(value=None):
    """The configured number of slots, from CAGENTS_EVIDENCE_NATIVE_SLOTS."""
    if value is None:
        value = os.environ.get("CAGENTS_EVIDENCE_NATIVE_SLOTS", "")
    if value == "":
        return DEFAULT_SLOTS
    if not value.isdigit() or not 1 <= int(value) <= MAXIMUM_SLOTS or value != str(int(value)):
        raise ValueError(f"CAGENTS_EVIDENCE_NATIVE_SLOTS must be a whole number from 1 to {MAXIMUM_SLOTS}: {value}")
    return int(value)


def slot_path(lock, index):
    return lock if index == 0 else f"{lock}.{index}"


def try_lock(descriptor):
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except BlockingIOError:
        return False


class Waiter:
    """One blocking `flock` on a descriptor shared with this process's caller.

    The lock it takes belongs to that open file, so a waiter that outlived
    everyone who could release it would hold a slot for good. It dies with this
    process, and it cannot start waiting until that is arranged: it exits on
    its own if this process is already gone, or closes its stdin, before it
    has its pidfd.
    """

    GATE = ('import os,sys\nif os.getppid()!=int(sys.argv[2]): sys.exit(75)\n'
            'if os.read(0,1)!=b"1": sys.exit(0)\nos.execlp("flock","flock",sys.argv[1])\n')

    def __init__(self, descriptor):
        self.pidfd = None
        self.process = subprocess.Popen(
            ["setpriv", "--pdeathsig", "TERM", sys.executable, "-c", self.GATE,
             str(descriptor), str(os.getpid())],
            pass_fds=(descriptor,), stdin=subprocess.PIPE, stdout=subprocess.DEVNULL)
        try:
            self.pidfd = os.pidfd_open(self.process.pid)
        except ProcessLookupError:
            self.process.stdin.close()
            return
        self.process.stdin.write(b"1")
        self.process.stdin.close()

    def granted(self):
        status = self.process.poll()
        if status is None:
            return False
        if status != 0:
            raise RuntimeError(f"the lock waiter exited {status}")
        return True

    def wait(self, seconds):
        """Waits until the waiter ends or `seconds` pass."""
        if self.pidfd is not None and self.process.poll() is None:
            select.select([self.pidfd], [], [], seconds)

    def withdraw(self):
        if not self.process.stdin.closed:
            try:
                self.process.stdin.close()
            except BrokenPipeError:
                pass
        if self.process.poll() is None and self.pidfd is not None:
            try:
                signal.pidfd_send_signal(self.pidfd, signal.SIGTERM)
            except ProcessLookupError:
                pass
        self.process.wait()
        if self.pidfd is not None:
            os.close(self.pidfd)


class Cancelled(Exception):
    pass


def acquire(queue, slots, budget, cancelled):
    """Returns the index of the slot taken, or None when the budget ran out.

    `queue` may be None for a single slot that needs no line of its own.
    `budget` is in seconds, or None to wait as long as it takes; 0 still takes
    an idle slot. `cancelled()` is polled while waiting and raises Cancelled.
    """
    deadline = None if budget is None else time.monotonic() + budget
    waiters = []
    chosen = None

    def expired():
        return deadline is not None and time.monotonic() >= deadline

    try:
        cancelled()
        # The queue first: only its head takes a slot, so a slot that frees
        # goes to whoever has waited longest.
        if queue is not None and not try_lock(queue):
            if budget == 0:
                return None
            waiter = Waiter(queue)
            waiters.append(waiter)
            while not waiter.granted():
                cancelled()
                if expired():
                    return None
                waiter.wait(POLL_SECONDS)
            waiters.remove(waiter)
            waiter.withdraw()
        first = None
        while True:
            # Slot 0 is where older tools wait, in the kernel's order: the
            # head joins that line rather than retrying it.
            if first is None:
                if try_lock(slots[0]):
                    chosen = 0
                    return chosen
                if budget != 0:
                    first = Waiter(slots[0])
                    waiters.append(first)
            elif first.granted():
                chosen = 0
                return chosen
            for index in range(1, len(slots)):
                if try_lock(slots[index]):
                    chosen = index
                    return chosen
            cancelled()
            if budget == 0 or expired():
                return None
            if first is None:
                time.sleep(POLL_SECONDS)
            else:
                first.wait(POLL_SECONDS)
    finally:
        # A second cancellation must not interrupt joining the waiters.
        blocked = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT, signal.SIGHUP})
        try:
            for waiter in waiters:
                waiter.withdraw()
            # A withdrawn waiter may have been granted just before it ended;
            # the lock is on the caller's open file, so give back all but the
            # chosen slot, and the queue.
            for index, descriptor in enumerate(slots):
                if index != chosen:
                    fcntl.flock(descriptor, fcntl.LOCK_UN)
            if queue is not None:
                fcntl.flock(queue, fcntl.LOCK_UN)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, blocked)


def held(lock, slots):
    """The slots some process holds now, by index."""
    busy = []
    for index in range(slots):
        try:
            descriptor = os.open(slot_path(lock, index), os.O_RDONLY | os.O_CLOEXEC)
        except FileNotFoundError:
            continue
        try:
            if try_lock(descriptor):
                fcntl.flock(descriptor, fcntl.LOCK_UN)
            else:
                busy.append(index)
        finally:
            os.close(descriptor)
    return busy


def main(arguments):
    parser = argparse.ArgumentParser(prog="native_slots.py", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    take = commands.add_parser("acquire", help="take a slot on descriptors the caller opened")
    take.add_argument("--queue-fd", type=int, required=True)
    take.add_argument("--slot-fd", type=int, action="append", required=True,
                      help="one per slot, slot 0 first")
    take.add_argument("--wait", type=int, help="give up after this many seconds")
    take.add_argument("--stop", help="give up once this file exists")
    take.add_argument("--starter", type=int, help="give up once this process is gone")
    take.add_argument("--parent-pid", type=int, required=True)
    commands.add_parser("count", help="print the configured number of slots")
    paths = commands.add_parser("paths", help="print every slot's lock file")
    paths.add_argument("lock")
    status = commands.add_parser("held", help="print the slots held now")
    status.add_argument("lock")
    options = parser.parse_args(arguments)
    try:
        count = slot_count()
    except ValueError as error:
        print(f"native_slots: {error}", file=sys.stderr)
        return 2
    if options.command == "count":
        print(count)
        return 0
    if options.command == "paths":
        print("\n".join(slot_path(options.lock, index) for index in range(count)))
        return 0
    if options.command == "held":
        print(" ".join(map(str, held(options.lock, count))))
        return 0

    # A waiter whose caller is gone would take a slot nobody uses.
    if os.getppid() != options.parent_pid:
        return CANCELLED
    if options.wait is not None and options.wait < 0:
        print("native_slots: --wait must not be negative", file=sys.stderr)
        return 2

    def cancelled():
        if os.getppid() != options.parent_pid:
            raise Cancelled
        if options.stop and os.path.exists(options.stop):
            raise Cancelled
        if options.starter is not None:
            try:
                os.kill(options.starter, 0)
            except ProcessLookupError:
                raise Cancelled from None

    def interrupted(signum, frame):
        raise SystemExit(128 + signum)

    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, interrupted)
    try:
        index = acquire(options.queue_fd, options.slot_fd, options.wait, cancelled)
    except Cancelled:
        return CANCELLED
    if index is None:
        return BUSY
    print(index)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
