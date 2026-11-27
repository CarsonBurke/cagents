#!/usr/bin/python3
"""Deliver new captures into a native-evidence task without losing edits made there.

  native-evidence-shots.py deliver TASK INCOMING

Each file under INCOMING is a capture: a new `shot`, or one brought back from
the build host. The tool's own copy of every capture goes to TASK/raw/, and
TASK/shots/ is the person's: a capture replaces a shot there only while that
shot is missing or still exactly what the tool last delivered. A shot changed
since, such as one with an address masked out, is kept, and the new capture
waits in raw/ beside it. Content decides, not modification times, which a
copy or archive can carry along unchanged.

Exit status 0 even when a shot was kept, which is reported on standard error;
1 when a file could not be written.
"""

import filecmp
import os
from pathlib import Path
import sys
import tempfile


def same(first: Path, second: Path) -> bool:
    return first.is_file() and second.is_file() and filecmp.cmp(first, second, shallow=False)


def install(source: Path, target: Path) -> None:
    """Replaces target with a copy of source at once, so a reader never sees
    half a file."""
    target.parent.mkdir(parents=True, exist_ok=True)
    descriptor, partial = tempfile.mkstemp(prefix=f".{target.name}.", dir=target.parent)
    try:
        with os.fdopen(descriptor, "wb") as out, source.open("rb") as data:
            while chunk := data.read(1 << 20):
                out.write(chunk)
        os.chmod(partial, 0o644)
        os.replace(partial, target)
    except BaseException:
        Path(partial).unlink(missing_ok=True)
        raise


def deliver(task: Path, incoming: Path) -> list[str]:
    """Delivers every capture under incoming; returns the shots kept as they were."""
    kept = []
    for capture in sorted(path for path in incoming.rglob("*") if path.is_file() and not path.is_symlink()):
        relative = capture.relative_to(incoming)
        original, shot = task / "raw" / relative, task / "shots" / relative
        if shot.is_symlink() or (shot.exists() and not shot.is_file()):
            kept.append(str(relative))
        elif not shot.exists() or same(shot, original) or same(shot, capture):
            install(capture, shot)
        else:
            # Changed after it was delivered, or present before the tool kept
            # originals and unlike this capture: either way it isn't the
            # tool's to replace.
            kept.append(str(relative))
        install(capture, original)
    return kept


def main(arguments: list[str]) -> int:
    if len(arguments) != 3 or arguments[0] != "deliver":
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    task, incoming = Path(arguments[1]), Path(arguments[2])
    try:
        kept = deliver(task, incoming)
    except OSError as error:
        print(f"native-evidence: could not deliver the captures to {task}: {error}", file=sys.stderr)
        return 1
    for relative in kept:
        print(f"native-evidence: kept {task}/shots/{relative}, which changed after it was delivered; "
              f"the new capture is {task}/raw/{relative}. Copy it over the shot to use it.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
