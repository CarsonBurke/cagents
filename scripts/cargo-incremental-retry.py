#!/usr/bin/python3 -I
"""Retry one workspace compiler invocation after an undefined-symbol link error."""

from collections import Counter
import contextlib
import ctypes
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


BUILD_DIR = "CAGENTS_BUILD_OFFLOAD_BUILD_DIR"
WORKSPACE_DIRS = "CAGENTS_BUILD_OFFLOAD_WORKSPACE_DIRS"
UNDEFINED = re.compile(rb"\bundefined (?:hidden symbol|symbol|reference to)\b", re.I)
COLOR = re.compile(rb"\x1b\[[0-9;]*m")
HUMAN_DIAGNOSTIC = re.compile(rb"^(?:error|warning|note|help)(?:\[[^]]+\])?:")


def flag(arguments, name):
    value = ""
    for index, word in enumerate(arguments):
        if word == name and index + 1 < len(arguments):
            value = arguments[index + 1]
        elif word.startswith(name + "="):
            value = word[len(name) + 1:]
    return value


def codegen(arguments, name):
    value = ""
    for index, word in enumerate(arguments):
        if word == "-C" and index + 1 < len(arguments):
            word += arguments[index + 1]
        if word.startswith("-C" + name + "="):
            value = word[len(name) + 3:]
    return value


def invocation(arguments):
    manifest = os.environ.get("CARGO_MANIFEST_DIR")
    workspace = os.environ.get(WORKSPACE_DIRS)
    if not manifest or not workspace or str(Path(manifest).resolve()) not in json.loads(workspace):
        return None
    crate = flag(arguments, "--crate-name")
    incremental = codegen(arguments, "incremental")
    if not re.fullmatch(r"\w+", crate) or not incremental:
        return None
    # An explicit rustc flag can override Cargo's incremental location. Only
    # touch a cache under the slot's build directory.
    path = Path(incremental).resolve()
    build_dir = os.environ.get(BUILD_DIR)
    if not build_dir or path.name != "incremental" or not path.is_relative_to(build_dir):
        return None
    return crate, path


def inode(path):
    status = path.stat(follow_symlinks=False)
    return status.st_dev, status.st_ino


def crate_cache(arguments, crate, incremental):
    """Find the failed rustc unit's cache inside its incremental root.

    Rustc appends its stable crate ID inside the root Cargo named. When the
    profile contains multiple units with that name, Rustc's hard-linked
    CGU outputs identify which unit failed, without guessing from mtimes.
    """
    candidates = [path for path in incremental.glob(f"{crate}-*")
                  if path.is_dir() and not path.is_symlink()]
    if len(candidates) <= 1:
        return next(iter(candidates), None)
    out = flag(arguments, "--out-dir")
    extra = codegen(arguments, "extra-filename")
    if not out or not Path(out).resolve().is_relative_to(os.environ[BUILD_DIR]):
        return None
    if not re.fullmatch(r"-[0-9a-f]+", extra):
        return None
    products = {inode(path) for path in Path(out).glob(crate + extra + ".*.rcgu.o")}
    if not products:
        return None

    def matches(cache):
        return any(inode(obj) in products for session in cache.glob("s-*")
                   if not session.is_symlink() and session.is_dir()
                   for obj in session.glob("*.o"))

    found = [cache for cache in candidates if matches(cache)]
    return found[0] if len(found) == 1 else None


def evict(cache):
    with contextlib.ExitStack() as locks:
        # Cargo serializes its builds, but leave a unit a rustc still holds.
        for path in cache.glob("s-*.lock"):
            lock = locks.enter_context(path.open("rb"))
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        evicted = Path(os.environ["CAGENTS_OFFLOAD_SLOT"], "evicted")
        evicted.mkdir(exist_ok=True)
        cache.rename(evicted / f"{time.time_ns()}-{os.getpid()}-{cache.name}")


def diagnostic(line):
    """Whether this rustc JSON diagnostic reports an undefined-symbol link."""
    if not line.startswith(b"{") or b"linking with " not in line or not UNDEFINED.search(line):
        return False
    try:
        found = json.loads(line)
    except ValueError:
        return False
    return found.get("level") == "error" and str(found.get("message", "")).startswith("linking with ")


def diagnostics(stderr):
    """Rustc separates human diagnostics with blank lines and JSON with newlines."""
    human = []
    for line in stderr:
        plain = COLOR.sub(b"", line)
        if not human and not HUMAN_DIAGNOSTIC.match(plain):
            yield line
            continue
        human.append(line)
        if not plain.strip():
            yield b"".join(human)
            human.clear()
    if human:
        yield b"".join(human)


def forward(line):
    sys.stderr.buffer.write(line)
    sys.stderr.buffer.flush()


def follow_parent(parent):
    # Cargo or the build host may cancel this process while rustc is active.
    ctypes.CDLL(None).prctl(1, signal.SIGTERM)  # PR_SET_PDEATHSIG
    if os.getppid() != parent:
        os.kill(os.getpid(), signal.SIGTERM)


def compiler(arguments):
    parent = os.getpid()
    return subprocess.Popen(arguments, stderr=subprocess.PIPE, close_fds=False,
                            preexec_fn=lambda: follow_parent(parent))


def main(arguments):
    target = invocation(arguments)
    if target is None:
        os.execvp(arguments[0], arguments)

    # Normally Cargo reads JSON diagnostics from rustc's stderr. Pass warnings
    # through promptly, holding only a possible failed link and its summary.
    held = []
    forwarded = Counter()
    undefined = False
    human_link = False
    with compiler(arguments) as rustc:
        for record in diagnostics(rustc.stderr):
            plain = COLOR.sub(b"", record)
            human_link = human_link or plain.startswith(b"error: linking with ")
            undefined = undefined or diagnostic(record) or (human_link and bool(UNDEFINED.search(plain)))
            if human_link or undefined:
                held.append(record)
            else:
                forward(record)
                forwarded[record] += 1
        status = rustc.wait()

    if status == 1 and undefined:
        try:
            cache = crate_cache(arguments, *target)
            if cache is not None:
                evict(cache)
        except OSError:
            cache = None
        if cache is not None:
            print(f"cargo-gate: retrying {target[0]} after an undefined-symbol link error "
                  "with fresh incremental state", file=sys.stderr, flush=True)
            held.clear()
            # Cargo still holds its build admission. Run the same command once,
            # preserving new diagnostics as well as the retry's result.
            with compiler(arguments) as rustc:
                for record in diagnostics(rustc.stderr):
                    if forwarded[record]:
                        forwarded[record] -= 1
                    else:
                        forward(record)
                status = rustc.wait()

    for line in held:
        forward(line)
    if status < 0:
        ctypes.CDLL(None).prctl(4, 0)  # PR_SET_DUMPABLE
        number = -status
        if number not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(number, signal.SIG_DFL)
        os.kill(os.getpid(), number)
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
