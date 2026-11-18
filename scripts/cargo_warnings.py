"""Tell the workspace's own warnings in Cargo's JSON messages from the rest.

Cargo prints warnings from every path package, the vendored `[patch]` crates
included, and stable Cargo has no switch for one dependency's lints. The
workspace's members must compile without warnings; the vendored crates'
warnings are nobody's to fix here. scripts/main-build-sentinel and the lane
tasks' scripts/cargo-lane both judge a Cargo command by this module, so the
rule main is held to and the one an author checks against are the same.

Cargo reports a warning once for each target that compiles the code, such as
a library and its tests, and again when it replays a fresh unit's cached
output; its human output folds those duplicates, and so does this.

This module uses only the standard library.
"""

import glob
import json
from pathlib import Path
import tomllib

# The `reason` of each message Cargo itself writes to standard output. Any
# other line, JSON or not, is a program's own output, such as a test's.
CARGO_REASONS = {"compiler-message", "compiler-artifact", "build-script-executed",
                 "build-finished", "timing-info"}

# What read() makes of a line.
OUTPUT = "output"          # not a Cargo message: a program's or wrapper's output
MESSAGE = "message"        # a Cargo message that is not a diagnostic
DIAGNOSTIC = "diagnostic"  # an error, a note, or a warning not attributed to either side
OWN = "own"                # a workspace crate's warning, the first time it is reported
REPEAT = "repeat"          # a workspace crate's warning reported before
VENDORED = "vendored"      # a warning from a crate outside the workspace


class WorkspaceUnreadable(ValueError):
    pass


def workspace_packages(checkout):
    """Names of the packages in the workspace at `checkout`: its `[workspace]
    members` and its root package. Raises WorkspaceUnreadable when the
    manifests cannot be read; Cargo then reports the problem itself."""
    checkout = Path(checkout)
    try:
        manifest = tomllib.loads((checkout / "Cargo.toml").read_text())
        names = set()
        for member in manifest.get("workspace", {}).get("members", ["."]):
            for directory in glob.glob(member, root_dir=checkout):
                member_manifest = checkout / directory / "Cargo.toml"
                if member_manifest.exists():
                    names.add(tomllib.loads(member_manifest.read_text())["package"]["name"])
        if "package" in manifest:
            names.add(manifest["package"]["name"])
        return names
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise WorkspaceUnreadable(error) from error


def workspace_root(directory):
    """The root of the workspace Cargo uses from `directory`: the nearest
    directory at or above it whose Cargo.toml has a `[workspace]`, or else the
    nearest with a Cargo.toml. None when there is no manifest above it."""
    directory = Path(directory).resolve()
    nearest = None
    for candidate in (directory, *directory.parents):
        manifest = candidate / "Cargo.toml"
        try:
            text = manifest.read_text()
        except (OSError, UnicodeDecodeError):
            continue
        nearest = nearest or candidate
        try:
            if "workspace" in tomllib.loads(text):
                return candidate
        except tomllib.TOMLDecodeError:
            # Cargo stops at a manifest it cannot read; so does this, rather
            # than judge against some workspace further up.
            return candidate
    return nearest


def package_name(package_id):
    """The name in a Cargo package ID specification, such as
    `path+file:///src#cagents@0.1.3` or `path+file:///src/cagents-ui#0.1.3`,
    whose name is the directory's when omitted."""
    source, separator, fragment = package_id.rpartition("#")
    if not separator:
        return package_id.split(" ", 1)[0]
    name, at, _ = fragment.partition("@")
    return name if at else source.rstrip("/").rsplit("/", 1)[-1]


def location(diagnostic):
    """`file:line` of a diagnostic's primary span, or None."""
    for span in diagnostic.get("spans") or ():
        if isinstance(span, dict) and span.get("is_primary"):
            return f"{span.get('file_name')}:{span.get('line_start')}"
    return None


class Warnings:
    """Classifies the lines of a Cargo command's standard output, run with a
    JSON `--message-format`, given the workspace's package names. With
    `packages` None, as when the workspace is unreadable, no warning is
    attributed to either side, so none is hidden and none counted."""

    def __init__(self, packages):
        self.packages = packages
        # The workspace's warnings, once each, in the order first reported:
        # (package ID, rendered) -> (rendered, location).
        self.own = {}
        self.vendored = set()

    def read(self, line):
        """(kind, text) for one line, as str or bytes: what kind it is, and
        the text Cargo's human output would show for it. That is the line
        itself, unchanged, for OUTPUT; a str for a diagnostic; and None for
        a Cargo message that is not a diagnostic."""
        try:
            message = json.loads(line)
        except ValueError:  # Including bytes that are not UTF-8.
            return OUTPUT, line
        reason = message.get("reason") if isinstance(message, dict) else None
        if not isinstance(reason, str) or reason not in CARGO_REASONS:
            return OUTPUT, line
        diagnostic = message.get("message")
        if reason != "compiler-message" or not isinstance(diagnostic, dict):
            return MESSAGE, None
        rendered = diagnostic.get("rendered")
        if not isinstance(rendered, str) or not rendered:
            rendered = f"{diagnostic.get('message', '')}\n"
        if self.packages is None or diagnostic.get("level") != "warning":
            return DIAGNOSTIC, rendered
        package_id = str(message.get("package_id", ""))
        if package_name(package_id) not in self.packages:
            self.vendored.add((package_id, rendered))
            return VENDORED, rendered
        key = (package_id, rendered)
        if key in self.own:
            return REPEAT, rendered
        self.own[key] = (rendered, location(diagnostic))
        return OWN, rendered

    def rendered(self):
        """The workspace's warnings, once each, as Cargo renders them."""
        return "".join(rendered for rendered, _ in self.own.values())

    def locations(self):
        return [where for _, where in self.own.values() if where]
