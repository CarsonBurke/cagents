#!/usr/bin/python3
"""Reserve the exact fresh synthetic window Store, without writing app data.

The native app creates the Store. Its caller supplies that generated path and
current device/inode plus the fresh root marker nonce. Only BEGIN IMMEDIATE
and ROLLBACK are issued; input readiness or the 38-second maximum releases it.
"""
import argparse
import json
import os
from pathlib import Path
import select
import sqlite3
import stat
import time
import uuid


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--device", type=int, required=True)
    parser.add_argument("--inode", type=int, required=True)
    parser.add_argument("--nonce", required=True)
    parser.add_argument("--max-seconds", type=int, default=38, choices=range(1, 39))
    args = parser.parse_args()
    root = args.root
    require(root.is_absolute() and root.resolve(strict=True) == root,
            "root must be an existing canonical absolute path")
    require(root.name.startswith("native1757-"), "root must be a fresh native1757 fixture")
    info = root.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and stat.S_IMODE(info.st_mode) == 0o700, "root ownership or mode mismatch")
    require(str(uuid.UUID(args.nonce)) == args.nonce, "marker nonce must be a canonical UUID")

    marker_path = root / ".native1757-owned.json"
    marker_info = marker_path.lstat()
    require(stat.S_ISREG(marker_info.st_mode) and marker_info.st_uid == os.getuid()
            and stat.S_IMODE(marker_info.st_mode) == 0o600, "owned marker identity or mode mismatch")
    marker = json.loads(marker_path.read_text())
    require(marker.get("schema") == 1 and marker.get("purpose") == "1757-hosted-native"
            and marker.get("root") == str(root) and marker.get("uid") == os.getuid()
            and marker.get("nonce") == args.nonce, "fresh root marker does not match")
    require(marker.get("window") in ("native1757-before", "native1757-after"),
            "marker must name an explicit serial native1757 window")

    database = args.database
    expected = root / "data/cagents-development/windows" / marker["window"] / "sessions.sqlite3"
    require(database == expected, "database must be this fixture's actual named-window Store")
    cursor = root
    components = database.relative_to(root).parts
    for index, component in enumerate(components):
        cursor = cursor / component
        current = cursor.lstat()
        require(not stat.S_ISLNK(current.st_mode) and current.st_uid == os.getuid(),
                "Store path contains a symlink or foreign owner")
        require(stat.S_ISREG(current.st_mode) if index == len(components) - 1
                else stat.S_ISDIR(current.st_mode), "Store path component has the wrong type")
    require(database.resolve(strict=True) == database, "Store path must remain canonical")
    info = database.lstat()
    require((info.st_dev, info.st_ino) == (args.device, args.inode),
            "actual generated Store identity changed")

    connection = sqlite3.connect(database.as_uri() + "?mode=rw", uri=True, timeout=0)
    statements = []
    connection.set_trace_callback(statements.append)
    started = time.monotonic()
    begun = False
    try:
        connection.execute("BEGIN IMMEDIATE")
        begun = True
        current = database.lstat()
        require(stat.S_ISREG(current.st_mode) and current.st_uid == os.getuid()
                and (current.st_dev, current.st_ino) == (args.device, args.inode),
                "Store identity changed before write reservation")
        print(json.dumps({"ready": True, "database": str(database), "device": args.device,
                          "inode": args.inode, "maximum_seconds": args.max_seconds,
                          "nonce": args.nonce, "statements": statements, "data_writes": False}), flush=True)
        select.select([0], [], [], args.max_seconds)
    finally:
        try:
            connection.rollback()
        finally:
            connection.close()
            require(statements == ["BEGIN IMMEDIATE", "ROLLBACK"] if begun
                    else statements in ([], ["BEGIN IMMEDIATE"]),
                    "unexpected SQL outside the transaction-only reservation")
            print(json.dumps({"released": begun, "elapsed_seconds": time.monotonic() - started,
                              "statements": statements, "data_writes": False}), flush=True)


if __name__ == "__main__":
    main()
