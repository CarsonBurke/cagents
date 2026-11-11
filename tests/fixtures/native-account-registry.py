#!/usr/bin/python3
"""Fault only a publicly selected synthetic Claude account's private registry.

Prepare creates an empty home, not an account registration. Use the real CLI's
account link/default commands before fault. The caller must verify that its
ordinary new draft selected this home. No provider, daemon or UI is launched.
"""
import argparse
import base64
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import stat
import sys
import tomllib
import uuid

VERSION = 1
NAMESPACE = "55555555-6666-4777-8888-999999999999"
FAULT = b"[fixture-account-registry\n"
LIMIT = 256 * 1024
AUTHORITY = ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN")


def identity(info):
    return dict(device=info.st_dev, inode=info.st_ino, owner=info.st_uid,
                mode=stat.S_IMODE(info.st_mode))


def owned(path, directory=False, private=False):
    info = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError("Expected an owned " + ("directory: " if directory else "file: ") + str(path))
    if not directory and info.st_nlink != 1:
        raise ValueError("Refusing a multiply-linked file: " + str(path))
    if stat.S_IMODE(info.st_mode) & (0o077 if private else 0o022):
        raise ValueError("Unsafe permissions: " + str(path))
    return identity(info)


def fixture_root(operand):
    if any(name in os.environ for name in AUTHORITY):
        raise PermissionError("A genuine native-evidence User environment is required")
    root = Path(operand)
    if not root.is_absolute() or root.resolve() != root or root.name != "fixture":
        raise ValueError("Use the canonical absolute native-evidence fixture path")
    real_home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()
    if root.parent == real_home or any(root.is_relative_to(real_home / name) for name in (".claude", ".codex")):
        raise ValueError("Refusing a real provider home")
    owned(root, directory=True)
    owned(root.parent, directory=True)
    owned(root.parent / ".native-evidence")
    if any((p / ".git").exists() or (p / ".git").is_symlink() for p in root.parents):
        raise ValueError("Native evidence must be outside Git ancestry")
    if "CAGENTS_EVIDENCE_FIXTURE" in os.environ and os.environ["CAGENTS_EVIDENCE_FIXTURE"] != str(root):
        raise ValueError("The selected fixture environment names another task")
    for name in ("home", "data", "config"):
        path = root / name
        owned(path, directory=True)
        if path.resolve() != path:
            raise ValueError("Refusing an aliased fixture directory")
    return root


def paths(root):
    home = root / "data/cagents-development/accounts/claude" / NAMESPACE
    registry = root / "config/cagents-development/accounts.toml"
    return home, registry


def parents_owned(path, root):
    for parent in path.parents:
        if parent == root:
            break
        owned(parent, directory=True)
        if parent.resolve() != parent:
            raise ValueError("Refusing an aliased parent directory")


def create_parents(path, root):
    for parent in reversed(path.parents):
        if parent == root or not parent.is_relative_to(root):
            continue
        if not parent.exists() and not parent.is_symlink():
            parent.mkdir(mode=0o700)
        owned(parent, directory=True)


def prepare(operand):
    root = fixture_root(operand)
    home, _ = paths(root)
    create_parents(home, root)
    home.mkdir(mode=0o700)  # Never adopt or empty an existing home.
    return dict(operation="prepare", home=str(home), home_identity=owned(home, directory=True, private=True),
                next="Use public account link/default, then verify the ordinary draft selected this home")


def selected_account(raw, home):
    registry = tomllib.loads(raw.decode("utf-8"))
    if registry.get("version", 0) > 5:
        raise ValueError("Unknown accounts registry version")
    matches = [row for row in registry.get("accounts", [])
               if row.get("provider") == "claude" and row.get("linked_home") == str(home)]
    if len(matches) != 1:
        raise ValueError("Need one public linked registration for the prepared namespace")
    key = matches[0]["key"]
    if str(uuid.UUID(key)) != key or registry.get("defaults", {}).get("claude") != key:
        raise ValueError("The linked fixture account is not the selected Claude default")
    if key in registry.get("archived", []):
        raise ValueError("The selected fixture account is archived")
    return "claude/" + key


@contextmanager
def locked_registry(root):
    home, path = paths(root)
    parents_owned(home, root)
    home_identity = owned(home, directory=True, private=True)
    parents_owned(path, root)
    lock_path = path.with_name("accounts.toml.lock")
    fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        if owned(lock_path, private=True) != identity(os.fstat(fd)):
            raise ValueError("Registry lock changed")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before = owned(path, private=True)
        with os.fdopen(os.open(path, os.O_RDWR | os.O_NOFOLLOW), "r+b") as stream:
            if before != identity(os.fstat(stream.fileno())):
                raise ValueError("Registry changed while opening")
            raw = stream.read(LIMIT + 1)
            if len(raw) > LIMIT:
                raise ValueError("Registry is too large")
            yield home, home_identity, path, before, stream, raw
    finally:
        os.close(fd)


def write_private(path, value):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def replace_exact(path, stream, before, expected, replacement):
    if owned(path, private=True) != before or identity(os.fstat(stream.fileno())) != before:
        raise ValueError("Registry identity changed; refusing a stale restore")
    stream.seek(0)
    if stream.read(LIMIT + 1) != expected:
        raise ValueError("Registry bytes changed; refusing an unknown replacement")
    stream.seek(0)
    stream.write(replacement)
    stream.truncate()
    stream.flush()
    os.fsync(stream.fileno())


def helper_sha():
    return hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def fault(operand):
    root = fixture_root(operand)
    with locked_registry(root) as (home, home_id, path, before, stream, original):
        account = selected_account(original, home)
        record = dict(version=VERSION, helper_sha256=helper_sha(), root=str(root),
                      root_identity=owned(root, directory=True), home_identity=home_id,
                      registry_identity=before, account=account,
                      original=base64.b64encode(original).decode("ascii"))
        write_private(root / "original-account-registry.json", record)
        replace_exact(path, stream, before, original, FAULT)
    return dict(operation="fault", account=account, registry=str(path),
                original_sha256=hashlib.sha256(original).hexdigest(), fault_sha256=hashlib.sha256(FAULT).hexdigest(),
                cause="Invalid TOML in the privately selected managed namespace; no provider start")


def restore(operand):
    root = fixture_root(operand)
    backup = root / "original-account-registry.json"
    owned(backup, private=True)
    with os.fdopen(os.open(backup, os.O_RDONLY | os.O_NOFOLLOW)) as source:
        if identity(os.fstat(source.fileno())) != owned(backup, private=True):
            raise ValueError("Fault backup changed")
        record = json.load(source)
    if record["version"] != VERSION or record["helper_sha256"] != helper_sha() or record["root"] != str(root):
        raise ValueError("Fault belongs to another helper or fixture")
    if record["root_identity"] != owned(root, directory=True):
        raise ValueError("Fixture identity changed")
    report = root / "restored-account-registry.json"
    if report.exists() or report.is_symlink():
        raise ValueError("This fault already has a restore receipt")
    original = base64.b64decode(record["original"], validate=True)
    with locked_registry(root) as (home, home_id, path, before, stream, raw):
        if home_id != record["home_identity"] or before != record["registry_identity"]:
            raise ValueError("Recorded home or registry identity changed")
        if selected_account(original, home) != record["account"]:
            raise ValueError("Recorded selected account changed")
        replace_exact(path, stream, before, FAULT, original)
    result = dict(operation="restore", account=record["account"], registry=str(path),
                  restored_sha256=hashlib.sha256(original).hexdigest())
    write_private(report, result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "fault", "restore"))
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    try:
        result = globals()[args.operation](args.root)
    except PermissionError as error:
        print(str(error), file=sys.stderr)
        return 77
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(str(error) + "; inspect the backup and exact current registry before further action", file=sys.stderr)
        return 1
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
