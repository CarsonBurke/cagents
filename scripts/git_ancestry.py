"""Repository markers used to protect paths that must be outside Git."""

import os
import stat


def _is_git_marker(marker):
    try:
        mode = os.lstat(marker).st_mode
        if stat.S_ISDIR(mode):
            # Sandbox placeholders have no HEAD.
            os.lstat(os.path.join(marker, "HEAD"))
            return True
        return True
    except (FileNotFoundError, NotADirectoryError):
        return False
    except OSError:
        # An unreadable entry cannot establish that the path is outside Git.
        return True


def enclosing_checkout(path):
    """Return the nearest ancestor's Git marker, excluding path itself."""
    path = os.path.abspath(path)
    while path != os.path.dirname(path):
        path = os.path.dirname(path)
        marker = os.path.join(path, ".git")
        if _is_git_marker(marker):
            return marker
    return None
