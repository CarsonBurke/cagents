"""Research-only original command custody; never import into the product.

Linux natural-observation methods are copied from executed engine8309. This
module intentionally has no process signal, terminate, kill or group operation.
An unresolved command retains its original Popen, pidfds and supervision owner.
"""
import ctypes
import hashlib
import json
import os
import pathlib
from pathlib import Path
import select
import signal
import subprocess
import sys
import time

_LIVE_OWNERS = []
_STOP_SIGNAL = None
_PHASE_FAILURE = None
_LAST_OWNER = None

def proc_identity(pid):
    try:
        raw = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return None
    fields = raw[raw.rfind(")") + 2:].split()
    return {"pid": pid, "ppid": int(fields[1]), "start_ticks": int(fields[19])}


def children(pid):
    try:
        return [int(x) for x in pathlib.Path(f"/proc/{pid}/task/{pid}/children").read_text().split()]
    except (FileNotFoundError, ProcessLookupError):
        if proc_identity(pid) is None:
            return []
        raise


class LinuxOwnedFamily:
    """Original8309 natural census/wait/fd methods; signaling method excluded."""

    def __init__(self, process):
        self.process = process
        self.members = {}
        self.root_fd = None
        self.root_identity = None
        self.cleanup_errors = []
        self.root_child_signal_fallback_used = False

    def capture_root(self):
        # This unreaped direct child cannot be recycled before its sole Popen
        # owner polls it. Retain the fd before fallible /proc ancestry reads.
        self.root_fd = os.pidfd_open(self.process.pid, 0)
        identity = proc_identity(self.process.pid)
        if identity is not None:
            self.root_identity = (identity["pid"], identity["start_ticks"])
        self.add(self.process.pid, os.getpid())

    def note_cleanup_error(self, error):
        value = type(error).__name__ + ": " + str(error)[:512]
        if value not in self.cleanup_errors and len(self.cleanup_errors) < 64:
            self.cleanup_errors.append(value)

    def add(self, pid, parent):
        before = proc_identity(pid)
        if before is None:
            return
        if before["ppid"] != parent:
            # A stale children-list entry must never authorize another process.
            if pid not in children(parent):
                return
            raise RuntimeError("Owned-child ancestry changed during capture")
        key = (pid, before["start_ticks"])
        if key in self.members:
            return
        uses_root_fd = key == self.root_identity and self.root_fd is not None
        try:
            fd = self.root_fd if uses_root_fd else os.pidfd_open(pid, 0)
        except ProcessLookupError:
            return
        try:
            after = proc_identity(pid)
            if after is None:
                return
            if (after["pid"], after["start_ticks"]) != key or after["ppid"] not in [parent, os.getpid()]:
                raise RuntimeError("Owned-child identity changed during pidfd capture")
            self.members[key] = {**after, "fd": fd}
        finally:
            if not uses_root_fd and key not in self.members:
                os.close(fd)

    def exited(self, member):
        poll = select.poll()
        poll.register(member["fd"], select.POLLIN)
        events = poll.poll(0)
        if any(flags & (select.POLLERR | select.POLLNVAL) for _, flags in events):
            raise RuntimeError("Owned pidfd became uninspectable")
        return bool(events)

    def census(self):
        pending = [os.getpid()]
        visited = set()
        while pending:
            parent = pending.pop()
            if parent in visited:
                continue
            visited.add(parent)
            for pid in children(parent):
                self.add(pid, parent)
                identity = proc_identity(pid)
                if identity and (pid, identity["start_ticks"]) in self.members:
                    pending.append(pid)
            if len(self.members) > 4096:
                raise RuntimeError("Owned command family exceeds census bound")
        # Do not compete with Popen for the original Cargo child's wait status.
        for pid in children(os.getpid()):
            if pid == self.process.pid:
                continue
            identity = proc_identity(pid)
            if identity is None:
                continue
            member = self.members.get((pid, identity["start_ticks"]))
            if member is None:
                raise RuntimeError("Unattributed adopted child remains")
            if self.exited(member):
                try:
                    os.waitpid(pid, os.WNOHANG)
                except ChildProcessError:
                    if proc_identity(pid) is not None:
                        raise

    def live(self):
        return [member for member in self.members.values() if not self.exited(member)]

    def settle(self, deadline, tolerate_errors=False):
        while True:
            try:
                self.process.poll()
                self.census()
                # A positive result requires this complete census, all retained
                # pidfds, and absence of any adopted/unattributed direct child.
                if self.process.returncode is not None and not self.live() and not children(os.getpid()):
                    return True
            except BaseException as error:
                if not tolerate_errors:
                    raise
                self.note_cleanup_error(error)
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.05)

    def receipt(self):
        return {"root_pid": self.process.pid, "root_pidfd_retained": self.root_fd is not None,
                "root_child_signal_fallback_used": self.root_child_signal_fallback_used,
                "root_identity": self.root_identity, "original_exit": self.process.returncode,
                "cleanup_errors": self.cleanup_errors,
                "members": [{k: v for k, v in member.items() if k != "fd"}
                            for member in self.members.values()]}

    def close(self):
        fds = {member["fd"] for member in self.members.values()}
        if self.root_fd is not None:
            fds.add(self.root_fd)
        for fd in fds:
            try:
                os.close(fd)
            except OSError as error:
                self.note_cleanup_error(error)


def _request_stop(signum, _frame):
    global _STOP_SIGNAL, _PHASE_FAILURE
    _STOP_SIGNAL = signum
    _PHASE_FAILURE = _PHASE_FAILURE or "cancelled-by-signal-" + str(signum)


def _save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def _fail(owner, reason):
    """Latch before fallible persistence; no later phase may clear a failure."""
    global _PHASE_FAILURE
    _PHASE_FAILURE = _PHASE_FAILURE or reason
    record = owner["record"]
    if record["failure"] is None:
        record["failure"] = reason
        record["failure_observed"] = time.time()
        record["state"] = "failed; passive original-command drain only"
        record["observed_output_bytes"] = owner.get("observed", 0)
        record["log_bytes"] = owner.get("stored", 0)
        _save(owner["receipt_path"], record)


def _fence(owner):
    if _STOP_SIGNAL is not None:
        _fail(owner, "cancelled-by-signal-" + str(_STOP_SIGNAL))
    deadline = owner.get("deadline")
    if deadline is not None and time.monotonic() >= deadline:
        _fail(owner, "work-deadline-exceeded")
    if _PHASE_FAILURE is not None:
        _fail(owner, _PHASE_FAILURE)


def _unknown_hold(owner, reason):
    """Retain custody and drain only the already-owned pipe; never signal."""
    global _PHASE_FAILURE
    _PHASE_FAILURE = _PHASE_FAILURE or reason
    record = owner["record"]
    record["failure"] = record["failure"] or reason
    record["custody_unknown"] = reason
    record["state"] = "UNKNOWN; original objects and supervision retained"
    record["output_complete"] = False
    try:
        _save(owner["receipt_path"], record)
        print("PASSIVE HOLD: " + reason, flush=True)
    except BaseException:
        pass
    # Do not census, reap, signal, close, release or start another command in
    # Unknown. Keep pumping the single original pipe so a chatty child can
    # complete naturally even after capture/logging failed. Discarded bytes
    # explicitly make output incomplete; retained content never exceeds 64 MiB.
    while True:
        try:
            process = owner["process"]
            stream = process.stdout if process is not None else None
            if stream is None or owner.get("hold_eof"):
                time.sleep(1)
                continue
            ready, _, _ = select.select([stream], [], [], 1)
            if not ready:
                continue
            block = os.read(stream.fileno(), 64 * 1024)
            if not block:
                owner["hold_eof"] = True
            else:
                owner["observed"] = owner.get("observed", 0) + len(block)
                record["observed_output_bytes"] = owner["observed"]
                record["discarded_during_unknown"] = record.get("discarded_during_unknown", 0) + len(block)
        except BaseException:
            # Failure to inspect/drain is not settlement. Avoid a hot loop.
            time.sleep(1)


def run_owned_phase(label, command, logs, work_seconds):
    """Await the original command; cancellation/deadline remain failed.

    Linux additionally requires the copied natural-family census. Darwin
    records original-command completion only; selected-parent assertions and
    their source bindings belong to the controller, not this process helper.
    External hard termination/power loss remains unqualified on both systems.
    """
    global _PHASE_FAILURE, _LAST_OWNER
    if _PHASE_FAILURE is not None or _STOP_SIGNAL is not None:
        raise RuntimeError("Prior failure/cancellation prevents another phase")
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, _request_stop)
    logs = Path(logs)
    path = logs / (label + ".txt")
    receipt_path = logs / (label + "-custody.json")
    record = {
        "label": label, "command": command, "cwd": str(Path.cwd()),
        "started": time.time(), "work_seconds": work_seconds,
        "policy": "NO_SIGNALS_PASSIVE_HOLD", "state": "preflight",
        "failure": None, "output_complete": True,
        "original_wait_complete": False, "owning_completion": False,
    }
    owner = {"record": record, "receipt_path": receipt_path, "process": None,
             "family": None, "output": None, "stored": 0, "observed": 0}
    _LIVE_OWNERS.append(owner)
    _LAST_OWNER = owner
    previous_subreaper = ctypes.c_int()
    subreaper_changed = False
    libc = None
    process = None
    family = None
    output = None
    settled = False
    original_exit = None
    cleanup_errors = []
    try:
        if not isinstance(command, list) or not command or not all(
            isinstance(arg, str) and "\0" not in arg for arg in command
        ):
            raise ValueError("Literal nonempty command argv required")
        if work_seconds <= 0:
            raise ValueError("Positive work budget required")
        if sys.platform == "linux":
            if not hasattr(os, "pidfd_open"):
                raise RuntimeError("Linux natural-family admission requires pidfds")
            # Verify before changing the process-wide state. Every successful
            # mutation is now inside this restoration owner's try/finally.
            if children(os.getpid()):
                raise RuntimeError("Preexisting children cannot enter a phase family")
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.prctl(37, ctypes.byref(previous_subreaper), 0, 0, 0) != 0:
                raise OSError(ctypes.get_errno(), "Cannot read subreaper state")
            if libc.prctl(36, 1, 0, 0, 0) != 0:
                raise OSError(ctypes.get_errno(), "Cannot retain original family custody")
            subreaper_changed = True
            if children(os.getpid()):
                raise RuntimeError("Preexisting children appeared before spawn")
        elif sys.platform != "darwin":
            raise RuntimeError("Unreviewed research process platform")
        output = path.open("xb")
        owner["output"] = output
        _save(receipt_path, record)
        print("Starting " + label, flush=True)
        _fence(owner)
        if record["failure"] is not None:
            raise RuntimeError("Stop observed before command spawn")
        owner["deadline"] = time.monotonic() + work_seconds
        process = subprocess.Popen(command, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT)
        # Install the exact Popen before any fallible fd/census/receipt work.
        owner["process"] = process
        record.update(pid=process.pid, state="running")
        os.set_blocking(process.stdout.fileno(), False)
        if sys.platform == "linux":
            family = LinuxOwnedFamily(process)
            owner["family"] = family
            family.capture_root()
        _save(receipt_path, record)
        eof = False
        while not eof or process.poll() is None:
            _fence(owner)
            if family is not None:
                family.census()
            try:
                block = os.read(process.stdout.fileno(), 64 * 1024)
            except BlockingIOError:
                block = None
            if block == b"":
                eof = True
            elif block:
                owner["observed"] += len(block)
                available = max(0, 64 * 1024 * 1024 - owner["stored"])
                output.write(block[:available])
                owner["stored"] += min(len(block), available)
                if len(block) > available:
                    record["output_complete"] = False
                    _fail(owner, "output-limit-exceeded")
            if block is None or eof:
                time.sleep(0.05)
        original_exit = process.wait()
        record.update(original_wait_complete=True, exit=original_exit)
        if original_exit != 0:
            _fail(owner, "original-command-nonzero-" + str(original_exit))
        _fence(owner)
        if family is not None:
            settled = family.settle(time.monotonic() + 10)
            record["owned_family"] = family.receipt()
            record["completion_scope"] = "linux-observed-original-family-natural"
        else:
            settled = True
            record["completion_scope"] = "darwin-original-command-only"
        if not settled:
            _unknown_hold(owner, "Original family has no positive natural settlement")
        _fence(owner)
        record["owning_completion"] = True
        output.flush()
        record["log_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
        record.update(log_bytes=owner["stored"], observed_output_bytes=owner["observed"])
        (logs / (label + ".exit")).write_text(str(original_exit) + "\n")
        _fence(owner)
        record.update(finished=time.time(), state="original command settled")
        _save(receipt_path, record)
    except BaseException as error:
        reason = type(error).__name__ + ": " + str(error)[:512]
        _PHASE_FAILURE = _PHASE_FAILURE or reason
        if process is not None and not settled:
            _unknown_hold(owner, reason)
        try:
            _fail(owner, reason)
        except BaseException:
            pass
        raise
    finally:
        # Unknown never reaches this point. Attempt every settled resource
        # close and restoration independently; one I/O error cannot skip the
        # process-wide subreaper restore or the remaining descriptor closes.
        if process is None or settled:
            actions = []
            if output is not None and not output.closed:
                actions.append(("log-close", output.close))
            if process is not None and process.stdout is not None:
                actions.append(("pipe-close", process.stdout.close))
            if family is not None:
                actions.append(("family-fd-close", family.close))
            try:
                for name, action in actions:
                    try:
                        action()
                    except BaseException as error:
                        cleanup_errors.append(name + ": " + type(error).__name__ + ": " + str(error)[:512])
                if family is not None:
                    record["owned_family"] = family.receipt()
                    cleanup_errors.extend(family.cleanup_errors)
            finally:
                try:
                    if subreaper_changed and libc.prctl(36, previous_subreaper.value, 0, 0, 0) != 0:
                        cleanup_errors.append("subreaper-restore: errno " + str(ctypes.get_errno()))
                except BaseException as error:
                    cleanup_errors.append("subreaper-restore: " + type(error).__name__)
                finally:
                    if cleanup_errors:
                        record["cleanup_errors"] = cleanup_errors
                        _PHASE_FAILURE = _PHASE_FAILURE or "settled-resource-cleanup-error"
                        record["failure"] = record["failure"] or "settled-resource-cleanup-error"
                    try:
                        _fence(owner)
                        _save(receipt_path, record)
                    except BaseException:
                        _PHASE_FAILURE = _PHASE_FAILURE or "final-receipt-or-fence-error"
                        raise
                    finally:
                        _LIVE_OWNERS.remove(owner)
    # These are still within the phase failure boundary. A late cancellation
    # or expiry during log decoding/printing must not become a successful return.
    try:
        text = path.read_text(errors="replace")
        print(text[-14000:], flush=True)
        _fence(owner)
        if record["failure"] is not None or _PHASE_FAILURE is not None:
            raise RuntimeError("Original phase failed: " + str(record))
        return text, record
    except BaseException as error:
        reason = type(error).__name__ + ": " + str(error)[:512]
        _PHASE_FAILURE = _PHASE_FAILURE or reason
        try:
            _fail(owner, reason)
        except BaseException:
            pass
        raise


def assert_sequence_success():
    """Final controller fence, including the last phase's original deadline."""
    if _LAST_OWNER is None or not _LAST_OWNER["record"]["owning_completion"]:
        raise RuntimeError("No positively completed original phase")
    _fence(_LAST_OWNER)
    if _STOP_SIGNAL is not None or _PHASE_FAILURE is not None:
        raise RuntimeError("Late stop/failure forbids overall success")
