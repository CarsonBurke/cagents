#!/usr/bin/python3
"""Optional, per-host coordination for gh invocations on one Unix account.

This is an invocation guard, not an HTTP proxy: gh can make several requests
inside one invocation. It adds no retries or requests of its own.
"""
import contextlib
import email.utils
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import tempfile
import time
from urllib.parse import urlsplit

WRITE_INTERVAL = 8.0
VALUE_FLAGS = {"--hostname", "-R", "--repo", "-X", "--method", "-f", "-F", "--field", "--raw-field", "--input", "-H", "--header", "--jq", "-q", "--template", "-t", "--cache", "--body", "-b", "--body-file", "--title", "--head", "--base", "--reviewer", "--assignee", "--label", "--project", "--milestone", "--search", "--author", "--state", "--limit"}


def normalize_authority(value):
    """Canonical API budget authority, retaining nondefault service ports."""
    url = urlsplit(value if "://" in value else "//" + value)
    host = (url.hostname or "").lower()
    if host == "api.github.com":
        host = "github.com"
    try:
        port = url.port
    except ValueError:
        port = None
    defaults = {80, 443} if url.scheme == "http" else {443}
    if ":" in host:
        host = "[" + host + "]"
    return host + (f":{port}" if port is not None and port not in defaults else "")


def enabled(root):
    setting = os.environ.get("CAGENTS_GH_COORDINATE")
    return setting == "1" or (setting != "0" and (root / "coordination.enabled").exists())


def repository_host(value):
    if value.startswith(("http://", "https://")):
        return normalize_authority(value)
    parts = value.split("/")
    return parts[0] if len(parts) >= 3 else None


def host_for(args):
    host = os.environ.get("GH_HOST", "github.com")
    host = repository_host(os.environ.get("GH_REPO", "")) or host
    index = 0
    while index < len(args):
        arg = args[index]
        if arg in {"--hostname", "-R", "--repo"} and index + 1 < len(args):
            value = args[index + 1]
            host = value if arg == "--hostname" else repository_host(value) or host
            index += 2
            continue
        if arg.startswith("--hostname="):
            host = arg.split("=", 1)[1]
        elif arg.startswith("--repo="):
            host = repository_host(arg.split("=", 1)[1]) or host
        elif arg.startswith("-R") and len(arg) > 2:
            host = repository_host(arg[2:]) or host
        index += 2 if arg in VALUE_FLAGS else 1
    # A URL is a target only in the command's endpoint / selector position.
    # Comment text and other option values must never select a budget.
    words = positionals(args)
    target = words[1] if len(words) > 1 and words[0] == "api" else words[2] if len(words) > 2 and words[0] in {"pr", "issue", "repo"} else ""
    if target.startswith(("http://", "https://")):
        host = normalize_authority(target) or host
    return normalize_authority(host)


def query_from(args):
    for index, arg in enumerate(args):
        if arg.startswith(("-fquery=", "-Fquery=")):
            return arg[8:]
        if arg.startswith(("--field=query=", "--raw-field=query=")):
            return arg.split("=", 1)[1][6:]
        if arg.startswith("query=") and index and args[index - 1] in {"-f", "-F", "--field", "--raw-field"}:
            return arg[6:]
    return None


def positionals(args):
    result = []
    skip = False

    for arg in args:
        if skip:
            skip = False
        elif arg in VALUE_FLAGS:
            skip = True
        elif arg.startswith("-"):
            continue
        else:
            result.append(arg)
    return result


class Deferred(Exception):
    def __init__(self, reason, retry_after=None):
        super().__init__(reason)
        self.reason = reason
        self.retry_after = retry_after

    def stderr(self):
        message = "cagents-gh: deferred " + self.reason + "\n"
        if self.retry_after is not None:
            message += f"Retry-After: {self.retry_after}\n"
        return message.encode()


def local_command(args):
    # Option values such as --body "--help" are content, not local options.
    skip = False
    for arg in args:
        if skip:
            skip = False
        elif arg in VALUE_FLAGS:
            skip = True
        elif arg in {"--help", "-h", "--version"}:
            return True
    words = positionals(args)
    return bool(words and (words[0] in {"help", "version", "completion", "config", "alias"} or words[:2] == ["auth", "token"]))


def is_write(args):
    words = positionals(args)
    if not words or local_command(args):
        return False
    command = words[0]
    if command == "api":
        endpoint = words[1] if len(words) > 1 else ""
        if endpoint == "graphql" or urlsplit(endpoint).path in {"/graphql", "/api/graphql"}:
            query = query_from(args)
            # A body supplied through --input may contain a mutation. Do not
            # read stdin, which belongs exclusively to the real command.
            return query is None or bool(re.match(r"\s*(?:#[^\n]*\n\s*)*mutation\b", query))
        method = None
        for index, arg in enumerate(args):
            if arg in {"-X", "--method"} and index + 1 < len(args):
                method = args[index + 1].upper()
            elif arg.startswith("--method="):
                method = arg.split("=", 1)[1].upper()
            elif arg.startswith("-X") and len(arg) > 2:
                method = arg[2:].upper()
        if method is not None:
            return method not in {"GET", "HEAD", "OPTIONS"}
        return any(arg in {"-f", "-F", "--field", "--raw-field", "--input"} or arg.startswith(("-f", "-F", "--field=", "--raw-field=", "--input=")) for arg in args)
    if len(words) < 2:
        return False
    return words[1] in {
        "create", "comment", "edit", "delete", "close", "reopen", "merge",
        "review", "ready", "lock", "unlock", "transfer", "pin", "unpin",
        "upload", "dispatch", "rerun", "cancel", "enable", "disable",
        "add", "remove", "rename", "set", "sync", "archive", "unarchive",
    }


def process_start(pid):
    try:
        stat = (Path("/proc") / str(pid) / "stat").read_text()
        return stat[stat.rfind(")") + 2:].split()[19]
    except (OSError, IndexError):
        return None


def paths(root, host):
    directory = root / "coordination"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    key = hashlib.sha256(host.encode()).hexdigest()[:32]
    return directory, key


@contextlib.contextmanager
def locked(root, host):
    directory, key = paths(root, host)
    fd = os.open(directory / (key + ".lock"), os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        path = directory / (key + ".json")
        try:
            data = json.loads(path.read_text())
        except (OSError, ValueError):
            data = {}
        yield data
        with tempfile.NamedTemporaryFile(dir=directory, prefix=".update-", mode="w", delete=False) as stream:
            json.dump(data, stream)
            temporary = Path(stream.name)
        temporary.replace(path)
    finally:
        os.close(fd)


def retry_deadline(row, now, failures):
    headers = row.get("headers", {})
    empty = headers.get("x-ratelimit-remaining") == "0"
    limited = row.get("rate_limited") or row.get("status") == 429 or empty or (row.get("status") == 403 and "retry-after" in headers)
    if not limited:
        return None
    fallback = min(3600, 60 * 2 ** min(failures, 6))
    retry = headers.get("retry-after")
    deadline = None
    if retry:
        try:
            deadline = now + max(0, float(retry))
        except ValueError:
            try:
                deadline = email.utils.parsedate_to_datetime(retry).timestamp()
            except (TypeError, ValueError, OverflowError):
                pass
    if empty:
        try:
            deadline = max(deadline or 0, float(headers["x-ratelimit-reset"]))
        except (KeyError, ValueError):
            pass
    return deadline if deadline is not None and deadline > now else now + fallback


def observed(root, row, now=None):
    if not enabled(root) or row.get("cached_response"):
        return
    host = normalize_authority(row.get("host") or "github.com")
    now = time.time() if now is None else now
    try:
        with locked(root, host) as state:
            failures = int(state.get("failures", 0))
            deadline = retry_deadline(row, now, failures)
            if deadline is not None:
                state["cooldown_until"] = max(state.get("cooldown_until", 0), deadline)
                state["failures"] = failures + 1
            elif state.get("cooldown_until", 0) <= now and 200 <= row.get("status", 0) < 400 and not row.get("graphql_errors"):
                state["failures"] = 0
    except OSError:
        # Instrumentation must not replace the command's result with an error.
        pass


def before(root, args):
    """Wait before a new invocation; return an exec-inheritable content lock.

    The caller must close this fd in the telemetry child so only real gh owns
    the reservation. FIFO tickets prevent a busy writer from overtaking those
    already waiting, and dead process identities cannot strand the queue.
    """
    if not enabled(root) or local_command(args):
        return None
    nonblocking = os.environ.get("CAGENTS_GH_NONBLOCKING") == "1"
    host = host_for(args)
    write = is_write(args)
    directory, key = paths(root, host)
    ticket = None
    content_fd = None
    try:
        if write:
            ticket = directory / f"{key}.{time.time_ns()}.{os.getpid()}.ticket"
            with locked(root, host):
                ticket.write_text(json.dumps({"pid": os.getpid(), "start": process_start(os.getpid())}))
            content_fd = os.open(directory / (key + ".content"), os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
        while True:
            delay = 0.1
            with locked(root, host) as state:
                now = time.time()
                until = state.get("cooldown_until", 0)
                if nonblocking and until > now:
                    raise Deferred("GitHub requests are paused by the shared rate limit.", math.ceil(until - now))
                if write:
                    candidates = []
                    for path in directory.glob(key + ".*.ticket"):
                        try:
                            owner = json.loads(path.read_text())
                            if owner.get("start") is None or process_start(owner["pid"]) != owner["start"]:
                                path.unlink(missing_ok=True)
                            else:
                                candidates.append(path)
                        except (OSError, ValueError, KeyError):
                            path.unlink(missing_ok=True)
                    first = min(candidates, key=lambda path: int(path.name.split(".")[1])) if candidates else None
                    until = max(until, state.get("next_write", 0))
                    if first == ticket and until <= now:
                        try:
                            fcntl.flock(content_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        except BlockingIOError:
                            pass
                        else:
                            state["next_write"] = now + WRITE_INTERVAL
                            ticket.unlink(missing_ok=True)
                            ticket = None
                            os.set_inheritable(content_fd, True)
                            result = content_fd
                            content_fd = None
                            return result
                elif until <= now:
                    return None
                if nonblocking:
                    raise Deferred("Another GitHub content change is waiting or in progress.")
                if until > now:
                    delay = min(1.0, max(0.1, until - now))
            time.sleep(delay)
    finally:
        if ticket is not None:
            ticket.unlink(missing_ok=True)
        if content_fd is not None:
            os.close(content_fd)
