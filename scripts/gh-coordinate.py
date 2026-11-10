#!/usr/bin/python3
"""Explicit content-only gh admission; never install this as a PATH wrapper.

Usage: gh-coordinate.py --real /usr/bin/gh [--wait 0..30] -- pr comment N -R OWNER/REPO ...
Only pr/issue create/comment and pr review are coordinated. Reads, local commands,
API requests (including mutations), aliases and other commands exec unchanged.
Coordination is local to this Git common directory and the explicit target repo;
it is not an account-wide GitHub rate limiter. No request is retried or timed out.
CAGENTS_GH_NONBLOCKING=1 overrides --wait with zero for pre-exec admission.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
from urllib.parse import unquote, urlsplit

WRITE_INTERVAL = 8.0
ALLOWED = {("pr", "create"), ("pr", "comment"), ("pr", "review"),
           ("issue", "create"), ("issue", "comment")}
VALUE_FLAGS = {
    "-R", "--repo", "-b", "--body", "-F", "--body-file", "-t", "--title",
    "-B", "--base", "-H", "--head", "-r", "--reviewer", "-a", "--assignee",
    "-l", "--label", "-m", "--milestone", "-p", "--project", "-T", "--template",
    "--recover", "--type",
}
BOOL_FLAGS = {
    "--help", "-h", "--editor", "-e", "--web", "-w", "--draft", "-d",
    "--fill", "--fill-first", "--fill-verbose", "--no-maintainer-edit",
    "--edit-last", "--create-if-none", "--delete-last", "--yes",
    "--approve", "-A", "--comment", "-c", "--request-changes", "-r",
}


class Deferred(Exception):
    pass


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise Deferred(message)


def option(args, index, review=False):
    """Consume only known content options; their values are never selectors."""
    word = args[index]
    if word == "-r" and review:
        return word, None, index + 1
    if word in VALUE_FLAGS:
        if index + 1 == len(args):
            raise Deferred(f"missing value for {word}")
        return word, args[index + 1], index + 2
    if word.startswith("--") and "=" in word:
        name, value = word.split("=", 1)
        if name in VALUE_FLAGS:
            return name, value, index + 1
    if word.startswith("-") and not word.startswith("--") and len(word) > 2:
        if word[:2] in VALUE_FLAGS:
            return word[:2], word[2:], index + 1
    if word in BOOL_FLAGS:
        return word, None, index + 1
    raise Deferred(f"unsupported content option {word!r}; invoke gh directly after reviewing it")


def authority(value):
    parsed = urlsplit("//" + value)
    try:
        port = parsed.port
    except ValueError as error:
        raise Deferred("invalid target hostname") from error
    host = parsed.hostname
    if not host or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment:
        raise Deferred("use a hostname without a URL, path or credentials")
    if not re.fullmatch(r"[A-Za-z0-9.-]+", host):
        raise Deferred("invalid target hostname")
    return host.lower() + (f":{port}" if port is not None and port != 443 else "")


def repository(value):
    parts = value.split("/")
    if len(parts) == 2:
        host = authority(os.environ.get("GH_HOST") or "github.com")
    elif len(parts) == 3:
        host = authority(parts.pop(0))
    else:
        raise Deferred("content commands require -R OWNER/REPO or -R HOST/OWNER/REPO")
    owner, name = parts
    name = name.removesuffix(".git")
    if any(part in {"", ".", ".."} or not re.fullmatch(r"[A-Za-z0-9_.-]+", part)
           for part in (owner, name)):
        raise Deferred("invalid target repository")
    return host, owner.lower(), name.lower()


def content_target(args):
    if not args or args[0] not in {"pr", "issue"}:
        return None
    # gh accepts the group's persistent repository option before its verb.
    index = 1
    while index < len(args) and (args[index] in {"-R", "--repo"}
                                 or args[index].startswith("--repo=")
                                 or args[index].startswith("-R") and len(args[index]) > 2):
        try:
            _, _, index = option(args, index)
        except Deferred:
            return None
    if index == len(args) or (args[0], args[index]) not in ALLOWED:
        return None
    family, verb = args[0], args[index]
    remaining = args[1:index] + args[index + 1:]
    selectors, targets = [], []
    index = 0
    while index < len(remaining):
        word = remaining[index]
        if word == "--":
            selectors.extend(remaining[index + 1:])
            break
        if word.startswith("-"):
            flag, value, index = option(remaining, index, review=(family, verb) == ("pr", "review"))
            if flag in {"--help", "-h"}:
                return None
            if flag in {"-R", "--repo"}:
                targets.append(repository(value))
        else:
            selectors.append(word)
            index += 1
    if not targets or len(set(targets)) != 1:
        raise Deferred("choose one explicit target with -R OWNER/REPO")
    if len(selectors) > (0 if verb == "create" else 1):
        raise Deferred("unexpected content command selectors")
    target = targets[0]
    if selectors and "://" in selectors[0]:
        url = urlsplit(selectors[0])
        expected = "pull" if family == "pr" else "issues"
        parts = [unquote(part) for part in url.path.strip("/").split("/")]
        if (url.scheme != "https" or url.username or url.password or url.query or url.fragment
                or len(parts) != 4 or parts[2] != expected
                or not parts[3].isascii() or not parts[3].isdigit() or int(parts[3]) == 0
                or repository(f"{url.netloc}/{parts[0]}/{parts[1]}") != target):
            raise Deferred("the selector URL must match the explicit target repository")
    return target


def git_common_dir():
    result = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=2, check=True)
    path = Path(os.fsdecode(result.stdout.rstrip(b"\n"))).resolve(strict=True)
    if not path.is_dir():
        raise Deferred("the Git common directory is unavailable")
    return path


def diagnostic(message):
    """Best effort only: a full stderr pipe must not become another queue."""
    message = (message + "\n").encode("utf-8", "backslashreplace")
    fd = None
    try:
        # Reopening gives pipes/terminals independent NONBLOCK flags. dup()
        # would change the caller's shared open description if its flags changed.
        fd = os.open("/proc/self/fd/2", os.O_WRONLY | os.O_NONBLOCK | os.O_CLOEXEC)
        if stat.S_ISREG(os.fstat(fd).st_mode):
            # Keep the inherited regular file's offset aligned with real gh.
            os.write(2, message)
        else:
            os.write(fd, message)
    except OSError:
        pass
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass


def waiting_notice(wait):
    diagnostic(f"cagents-gh: waiting for content admission (at most {wait:g}s total).")


def admit(target, wait):
    deadline = time.monotonic() + wait
    announced = False

    def announce():
        nonlocal announced
        if not announced:
            announced = True
            waiting_notice(wait)

    directory = git_common_dir() / "cagents-gh-content"
    directory.mkdir(mode=0o700, exist_ok=True)
    metadata = directory.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
        raise Deferred("content coordination directory must be private and owned by this user")
    key = hashlib.sha256("/".join(target).encode()).hexdigest()
    fd = os.open(directory / (key + ".lock"),
                 os.O_RDWR | os.O_CREAT | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise Deferred("content coordination lock is not an owned regular file")
        while True:
            if wait and time.monotonic() > deadline:
                raise Deferred("content admission deadline expired; nothing was executed")
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise Deferred("another content command is still running; nothing was executed")
                announce()
                time.sleep(min(0.025, remaining))
        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        previous = os.pread(fd, 1024, 0)
        state = json.loads(previous) if previous else {}
        now = time.monotonic()
        last = state.get("started") if state.get("boot") == boot else None
        if last is not None and (not isinstance(last, (int, float)) or not math.isfinite(last) or not 0 <= last <= now):
            raise Deferred("content spacing record is invalid; nothing was executed")
        delay = max(0, last + WRITE_INTERVAL - now) if last is not None else 0
        if delay > 0:
            if delay > deadline - now:
                raise Deferred("content spacing exceeds the wait budget; nothing was executed")
            announce()
            time.sleep(delay)
        if wait and time.monotonic() > deadline:
            raise Deferred("content admission deadline expired; nothing was executed")
        started = json.dumps({"boot": boot, "started": time.monotonic()}).encode()
        if os.pwrite(fd, started, 0) != len(started):
            raise Deferred("could not record content spacing; nothing was executed")
        os.ftruncate(fd, len(started))
        os.set_inheritable(fd, True)
        return fd
    except BaseException:
        os.close(fd)
        raise


def main():
    parser = Parser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--real", required=True, type=Path, help="absolute executable for the real gh")
    parser.add_argument("--wait", default=30.0, type=float, help="maximum pre-exec wait, 0 through 30 seconds")
    parser.add_argument("arguments", nargs=argparse.REMAINDER, help="-- followed by unchanged gh arguments")
    lease = None
    try:
        options = parser.parse_args()
        if not options.real.is_absolute() or not options.real.is_file() or not os.access(options.real, os.X_OK):
            raise Deferred("--real must name an absolute executable")
        if options.real.resolve() == Path(__file__).resolve():
            raise Deferred("--real cannot name this coordinator")
        if not math.isfinite(options.wait) or not 0 <= options.wait <= 30:
            raise Deferred("--wait must be between 0 and 30 seconds")
        args = options.arguments
        if args[:1] == ["--"]:
            args = args[1:]
        target = content_target(args)
        if target is not None:
            wait = 0 if os.environ.get("CAGENTS_GH_NONBLOCKING") == "1" else options.wait
            lease = admit(target, wait)
        os.execv(str(options.real), [str(options.real), *args])
    except (Deferred, OSError, ValueError, subprocess.SubprocessError) as error:
        if lease is not None:
            try:
                os.close(lease)
            except OSError:
                pass
        diagnostic(f"cagents-gh: deferred {error}")
        return 75


if __name__ == "__main__":
    sys.exit(main())
