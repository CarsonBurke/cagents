#!/usr/bin/python3
"""Admission for local source and artifact work shared by offload clients."""

from contextlib import contextmanager, nullcontext
import os
import select
import shutil
import stat
import subprocess
import sys
import threading
import time

# An isolated Python entry point still needs its companion scripts, including
# when the gate runs from the installed copy.
sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))
sys.dont_write_bytecode = True
from native_slots import Waiter, try_lock


class LocalSyncCancelled(Exception):
    pass


class LocalSyncTimeout(TimeoutError):
    pass


class LocalSyncError(OSError):
    pass


def _slot_count(environment):
    name = "CAGENTS_BUILD_LOCAL_SYNC_SLOTS"
    value = environment.get(name) or "2"
    try:
        if value.isascii() and value.isdecimal():
            count = int(value)
            if count > 0:
                return count
    except ValueError:
        pass  # Python refused to parse an excessively long integer.
    raise LocalSyncError(f"{name} must be a positive integer")


def _open_slot(path, use_default):
    # If another client has already created the file, the slot still works
    # under a sandbox that mounts /var/tmp read-only.
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
    try:
        try:
            descriptor = os.open(path, flags)
        except FileNotFoundError:
            descriptor = os.open(path, flags | os.O_CREAT, 0o600)
    except OSError as error:
        message = f"could not open local sync lock {path} ({error.strerror})"
        if use_default:
            message += ". Set CAGENTS_BUILD_LOCAL_SYNC_LOCK to a shared writable lock prefix"
        raise LocalSyncError(message) from error
    try:
        info = os.fstat(descriptor)
        if not (stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()):
            raise LocalSyncError(f"{path} is not a local sync lock file owned by this user")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _local_budget(cancellation, deadline):
    if cancellation is not None and cancellation.poll(0):
        raise LocalSyncCancelled
    if deadline is not None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise LocalSyncTimeout("timed out waiting for a local sync slot")
        return remaining


def _wait_for_slot(descriptors, cancel_fd, cancellation, deadline):
    # Each helper blocks inside flock. Its descriptor shares the lock with
    # ours, so an exit grants the slot. Watch every slot and take the first
    # one available, including when its previous holder was killed.
    print("waiting for a local sync slot", file=sys.stderr, flush=True)
    waiters = {}
    try:
        poller = select.poll()
        if cancel_fd is not None:
            poller.register(cancel_fd, select.POLLIN)
        for descriptor in descriptors:
            _local_budget(cancellation, deadline)
            waiter = Waiter(descriptor)
            waiters[descriptor] = waiter
            if waiter.granted():
                return descriptor
            poller.register(waiter.pidfd, select.POLLIN)
        remaining = _local_budget(cancellation, deadline)
        poller.poll(None if remaining is None else int(1000 * remaining) + 1)
        _local_budget(cancellation, deadline)
        for descriptor, waiter in waiters.items():
            if waiter.granted():
                return descriptor
        raise OSError("a local sync slot waiter exited without granting its lock")
    finally:
        for waiter in waiters.values():
            waiter.withdraw()


@contextmanager
def local_phase(environment=os.environ, *, cancel_fd=None, deadline=None):
    """Hold one local slot. Deadline is monotonic and only bounds admission."""
    configured = environment.get("CAGENTS_BUILD_LOCAL_SYNC_LOCK")
    prefix = configured or f"/var/tmp/cagents-offload-local-{os.getuid()}.lock"
    cancellation = None
    if cancel_fd is not None:
        cancellation = select.poll()
        cancellation.register(cancel_fd, select.POLLIN)
    descriptors = []
    chosen = None
    try:
        for index in range(_slot_count(environment)):
            _local_budget(cancellation, deadline)
            descriptor = _open_slot(f"{prefix}.{index}", use_default=not configured)
            descriptors.append(descriptor)
            if try_lock(descriptor):
                chosen = descriptor
                break
        if chosen is None:
            chosen = _wait_for_slot(descriptors, cancel_fd, cancellation, deadline)
        _local_budget(cancellation, deadline)
        descriptors.remove(chosen)
        for descriptor in descriptors:
            os.close(descriptor)
        descriptors = [chosen]
        yield chosen
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


def lower_priority():
    """Lower priority for this Linux thread and the processes it starts."""
    os.nice(10)
    if ionice := shutil.which("ionice"):
        # A kernel or sandbox that refuses ioprio_set still gets the CPU nice.
        subprocess.run([ionice, "-c", "2", "-n", "7", "-p", str(threading.get_native_id())],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def run_local(function, *, environment=os.environ, cancel_fd=None, deadline=None):
    """Run a local phase, passing its lock descriptor to the worker function."""
    import concurrent.futures

    def work():
        with local_phase(environment, cancel_fd=cancel_fd, deadline=deadline) as descriptor:
            lower_priority()
            return function(descriptor)

    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        return executor.submit(work).result()


def main(arguments):
    supervised = arguments[:2] == ["local", "--supervise"]
    if (len(arguments) > 1 and arguments[0] in ("local", "low")
            and (not supervised or len(arguments) >= 5)):
        phase = local_phase() if arguments[0] == "local" else nullcontext()
        try:
            with phase as descriptor:
                lower_priority()
                if descriptor is not None:
                    # The slot lasts until the command and any processes
                    # still doing the transfer close the descriptor.
                    os.set_inheritable(descriptor, True)
                    if supervised:
                        name = arguments[2]
                        helper = os.path.join(os.path.dirname(os.path.realpath(__file__)),
                                              "cargo-gate-command.py")
                        os.execv(sys.executable, [sys.executable, "-I", helper, "--wrapper", name,
                                                  "--hold-fd", str(descriptor), *arguments[3:]])
                os.execvp(arguments[1], arguments[1:])
        except (OSError, ValueError, RuntimeError) as error:
            print(f"offload_client: {error}", file=sys.stderr)
            return 1
    print("usage: offload_client.py {local|low} COMMAND [ARGS...] or local --supervise NAME ARGS...",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
