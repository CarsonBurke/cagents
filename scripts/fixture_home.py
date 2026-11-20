"""Require a positively marked fixture root before accessing fixture state.

A fixture creator places ``.cagents-fixture-root`` at its root. Provider
stand-ins and seeders call ``require_fixture_home`` before any state writes.
No account-home fallback is allowed when a variable is missing or empty.
"""

import os
from pathlib import Path
import sys

FIXTURE_ROOT_MARKER = ".cagents-fixture-root"


def require_fixture_home(variable, value=None):
    """Return the real config path only when it has a marked ancestor.

    ``value`` is for config paths explicitly supplied by the fixture itself;
    otherwise only the named environment variable supplies the path.
    """
    raw = os.environ.get(variable) if value is None else os.fspath(value)
    resolved = None
    if raw:
        try:
            resolved = Path(os.path.realpath(raw))
            if any((ancestor / FIXTURE_ROOT_MARKER).is_file()
                   for ancestor in (resolved, *resolved.parents)):
                return resolved
        except (OSError, ValueError):
            pass
    path = str(resolved) if resolved is not None else (str(raw) if raw else "<unset>")
    # Paths may contain newlines; a refusal is always one stderr line.
    path = path.replace("\r", "\\r").replace("\n", "\\n")
    print(f"fixture-home: refusing {variable}={path}: not inside a marked fixture home",
          file=sys.stderr)
    raise SystemExit(1)
