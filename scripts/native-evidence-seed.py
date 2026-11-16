#!/usr/bin/python3
"""Restore a native fixture without replacing shared files or directories."""
from pathlib import Path
import os
import stat
import subprocess
import sys


def restore(seed: Path, fixture: Path) -> int:
    changed_modes = []
    result = None
    try:
        # Older rsync versions cannot update an in-place read-only file, even
        # when its owner can change its mode. Keep the file's inode: transcripts
        # and provider homes record identities, so replacing them is not a reset.
        for directory, _, files in os.walk(fixture, followlinks=False):
            for name in files:
                target = Path(directory) / name
                previous = target.lstat()
                if not stat.S_ISREG(previous.st_mode) or previous.st_mode & stat.S_IWUSR:
                    continue
                source = seed / target.relative_to(fixture)
                try:
                    source_mode = source.lstat().st_mode
                except FileNotFoundError:
                    continue
                if not stat.S_ISREG(source_mode):
                    continue
                changed_modes.append((target, previous, stat.S_IMODE(source_mode)))
                target.chmod(stat.S_IMODE(previous.st_mode) | stat.S_IWUSR)
        result = subprocess.run([
            "rsync", "-aH", "--inplace", "--delete", "--force", "--",
            str(seed) + "/", str(fixture) + "/",
        ]).returncode
        return result
    finally:
        for target, previous, source_mode in changed_modes:
            try:
                current = target.lstat()
            except FileNotFoundError:
                continue
            if (current.st_dev, current.st_ino) == (previous.st_dev, previous.st_ino):
                mode = source_mode if result == 0 else stat.S_IMODE(previous.st_mode)
                if stat.S_IMODE(current.st_mode) != mode:
                    target.chmod(mode)


if __name__ == "__main__":
    try:
        raise SystemExit(restore(Path(sys.argv[1]), Path(sys.argv[2])))
    except OSError as error:
        print(f"could not restore fixture seed: {error}", file=sys.stderr)
        raise SystemExit(1)
