#!/usr/bin/env python3
"""Reuse a successful explicit rustfmt check only for identical local inputs.

This is optional acceleration. Any source graph, tool, or filesystem input that
cannot be fingerprinted makes us run the ordinary rustfmt command instead.
"""

import hashlib
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile

try:
    import tomllib
except ImportError:
    tomllib = None


ROOT = Path(__file__).resolve().parent.parent
SOURCE_ROOTS = ("src", "tests", "crates")
# The checker runs from ROOT, which also anchors relative Cargo target paths.
CACHE = ROOT / (os.environ.get("CARGO_TARGET_DIR") or "target") / ".check-format-cache"
CONFIG_NAMES = ("rustfmt.toml", ".rustfmt.toml")
MAX_FILES = 20_000
MAX_BYTES = 128 * 1024 * 1024
ATTRIBUTE_START = re.compile(rb"#\s*!?\s*\[")
COMMENTED_ATTRIBUTE = re.compile(rb"#\s*!?\s*(?:/\*|//)")
DIRECT_PATH = re.compile(rb'#\s*\[\s*path\s*=\s*"([^"\\]+)"\s*\]')
PATH_SETTING = re.compile(rb"\bpath\s*=")
CHAR_LITERAL = re.compile(rb"'(?:\\.|[^'\\\r\n])'")
NON_ASCII_WHITESPACE = tuple(char.encode() for char in "\u0085\u200e\u200f\u2028\u2029")


class Uncacheable(Exception):
    """Use rustfmt directly; no cache decision is safe for these inputs."""


class InputHashes:
    """Keep reusable content identity separate from the current race guard."""

    def __init__(self):
        self.content = hashlib.sha256()
        self.guard = hashlib.sha256()

    def update(self, value):
        self.content.update(value)

    def hexdigest(self):
        return self.content.hexdigest()


def add(hasher, value):
    if isinstance(value, str):
        value = os.fsencode(value)
    hasher.update(len(value).to_bytes(8, "big"))
    hasher.update(value)


def file_identity(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def add_identity(hasher, path, info):
    add(hasher.guard, str(path))
    add(hasher.guard, str(file_identity(info)))


def add_directory(hasher, path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode):
        raise Uncacheable("linked or special input directory")
    add(hasher, str(path))
    add_identity(hasher, path, info)


def config_parent_identity(hasher, path):
    # A temporary config or missing config directory can be created/removed
    # during a check while leaving no file behind. Its parent records the edit.
    for parent in path.parents:
        try:
            add_directory(hasher, parent)
            return
        except FileNotFoundError:
            add(hasher, str(parent))
            add(hasher, "absent-directory")


def add_file(hasher, path, name):
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode):
        raise Uncacheable(f"not a regular file: {path}")
    add(hasher, name)
    add_identity(hasher, path, before)
    with path.open("rb") as source:
        if file_identity(os.fstat(source.fileno())) != file_identity(before):
            raise Uncacheable("input replaced while opening")
        data = source.read()
        if file_identity(os.fstat(source.fileno())) != file_identity(before):
            raise Uncacheable("input changed while hashing")
    add(hasher, data)
    return data


def attributes(source):
    """Find attributes with enough lexical care to reject unknown path forms."""
    if any(space in source for space in NON_ASCII_WHITESPACE):
        raise Uncacheable("non-ASCII Rust whitespace")
    if COMMENTED_ATTRIBUTE.search(source):
        raise Uncacheable("comment between attribute delimiters")
    for match in ATTRIBUTE_START.finditer(source):
        start = match.start()
        cursor = match.end()
        depth = 1
        quoted = False
        escaped = False
        while cursor < len(source) and cursor - start < 16_384:
            char = source[cursor]
            if quoted:
                if escaped:
                    escaped = False
                elif char == ord("\\"):
                    escaped = True
                elif char == ord('"'):
                    quoted = False
            elif char == ord('"'):
                prefix = cursor - 1
                while prefix >= start and source[prefix] == ord("#"):
                    prefix -= 1
                if prefix >= start and source[prefix] == ord("r"):
                    raise Uncacheable("raw string in attribute")
                quoted = True
            elif char == ord("'"):
                literal = CHAR_LITERAL.match(source, cursor)
                if literal is None:
                    raise Uncacheable("unknown character literal in attribute")
                cursor = literal.end()
                continue
            elif source[cursor:cursor + 2] in (b"/*", b"//"):
                # Comments and character literals can contain apparent closing
                # brackets. Unknown syntax must never hide a later path token.
                raise Uncacheable("comment or character literal in attribute")
            elif char == ord("["):
                depth += 1
            elif char == ord("]"):
                depth -= 1
                if depth == 0:
                    yield source[start:cursor + 1]
                    break
            cursor += 1
        else:
            raise Uncacheable("attribute cannot be delimited")


def check_module_paths(source):
    for attribute in attributes(source):
        # Rustfmt recursively visits path settings in every attribute, including
        # arbitrary wrappers. Search all bytes; masking literals can hide one.
        if not PATH_SETTING.search(attribute):
            continue
        match = DIRECT_PATH.fullmatch(attribute)
        if match is None:
            raise Uncacheable("complex path attribute")
        value = os.fsdecode(match.group(1))
        if (not value or Path(value).is_absolute() or ".." in Path(value).parts
                or "\\" in value or Path(value).suffix != ".rs"):
            raise Uncacheable("path attribute may leave source roots")


def source_files(hasher):
    count = 0
    size = 0
    for name in SOURCE_ROOTS:
        base = ROOT / name
        if not stat.S_ISDIR(base.lstat().st_mode):
            raise Uncacheable(f"missing source root: {name}")
        def walk_error(error):
            raise error

        for directory, dirs, files in os.walk(base, followlinks=False, onerror=walk_error):
            dirs.sort()
            files.sort()
            current = Path(directory)
            add_directory(hasher, current)
            for entry in dirs:
                if not stat.S_ISDIR((current / entry).lstat().st_mode):
                    raise Uncacheable("linked or special source directory")
            for entry in files:
                path = current / entry
                count += 1
                size += path.lstat().st_size
                if count > MAX_FILES or size > MAX_BYTES:
                    raise Uncacheable("source fingerprint budget exceeded")
                data = add_file(hasher, path, str(path.relative_to(ROOT)))
                if entry in CONFIG_NAMES:
                    config_file(hasher, path)
                if path.suffix == ".rs":
                    check_module_paths(data)


def reject_linked_parents(path):
    for parent in path.parents:
        try:
            if stat.S_ISLNK(parent.lstat().st_mode):
                raise Uncacheable("linked configuration directory")
        except FileNotFoundError:
            continue


def config_file(hasher, path):
    reject_linked_parents(path)
    config_parent_identity(hasher, path)
    add(hasher, str(path))
    try:
        contents = add_file(hasher, path, "config")
    except FileNotFoundError:
        add(hasher, "absent")
        return
    if tomllib is None:
        raise Uncacheable("TOML config inputs cannot be verified")
    config = tomllib.loads(contents.decode("utf-8"))
    if "license_template_path" in config:
        # This option can read a file outside the source/config graph. Reject
        # parsed keys so quoted or escaped TOML spellings cannot evade it.
        raise Uncacheable("external rustfmt license template")


def ancestor_configs(hasher):
    # Rustfmt can search upward from each explicit root. Include absence too,
    # so a newly added config invalidates an earlier successful check.
    home = Path.home()
    xdg = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config")
    directories = {ROOT, home}
    for name in SOURCE_ROOTS:
        directory = ROOT / name
        while True:
            directories.add(directory)
            if directory == directory.parent:
                break
            directory = directory.parent
    for directory in sorted(directories):
        for name in CONFIG_NAMES:
            config_file(hasher, directory / name)
    # Rustfmt also reads its XDG config directory, which need not be an
    # ancestor of this checkout. A changed file there must invalidate a hit.
    for directory in sorted({xdg, xdg / "rustfmt", home / ".config", home / ".config/rustfmt"}):
        for name in CONFIG_NAMES:
            config_file(hasher, directory / name)


def tool_file(hasher, path):
    """Hash large formatter libraries without retaining them in memory."""
    path = path.resolve(strict=True)
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise Uncacheable("special formatter input")
        add(hasher, str(path))
        add_identity(hasher, path, before)
        hasher.update(before.st_size.to_bytes(8, "big"))
        size = 0
        while block := source.read(1024 * 1024):
            size += len(block)
            hasher.update(block)
        after = os.fstat(source.fileno())
        if size != before.st_size or file_identity(after) != file_identity(before):
            raise Uncacheable("formatter input changed while hashing")


def elf_tool(hasher, path):
    with path.open("rb") as executable:
        if executable.read(4) != b"\x7fELF":
            raise Uncacheable("unknown formatter wrapper")
    tool_file(hasher, path)


def formatter_libraries(hasher, executable):
    # Ask the ELF loader for this exact invocation's transitive dependency
    # graph. Do not execute an ldd wrapper or infer libraries from filenames.
    traced = subprocess.run(
        [str(executable)], cwd=ROOT, timeout=3,
        env=os.environ | {"LD_TRACE_LOADED_OBJECTS": "1", "LC_ALL": "C"},
        capture_output=True, check=True,
    )
    if traced.stderr:
        raise Uncacheable("formatter loader diagnostics")
    output = traced.stdout.decode()
    libraries = set()
    for line in output.splitlines():
        if re.fullmatch(r"\s*linux-vdso\.so\.\d+ \(0x[0-9a-fA-F]+\)\s*", line):
            continue
        match = re.fullmatch(
            r"\s*(?:[^\s]+\s+=>\s+)?(/[^\s]+)\s+\(0x[0-9a-fA-F]+\)\s*", line,
        )
        if match is None:
            raise Uncacheable("unknown formatter library graph")
        libraries.add(Path(match.group(1)).resolve(strict=True))
    if not libraries:
        raise Uncacheable("formatter library graph unavailable")
    for library in sorted(libraries):
        tool_file(hasher, library)


def rustfmt_binary(hasher, executable):
    proxy = Path(executable).resolve(strict=True)
    elf_tool(hasher, proxy)
    actual = proxy
    rustup = shutil.which("rustup")
    matches_rustup = rustup is not None and proxy.samefile(rustup)
    if proxy.name == "rustup" or matches_rustup:
        if not matches_rustup:
            raise Uncacheable("unverified rustup proxy")
        selected = subprocess.check_output(
            [rustup, "which", "rustfmt"], cwd=ROOT, timeout=3,
        ).decode().strip()
        actual = Path(selected)
        if not actual.is_absolute() or actual.samefile(proxy):
            raise Uncacheable("rustup did not select a rustfmt binary")
        actual = actual.resolve(strict=True)
        elf_tool(hasher, actual)
        # Overrides/default toolchains are rustup settings outside the source
        # tree. Include their presence and bytes as well as the selected binary.
        rustup_home = Path(os.environ.get("RUSTUP_HOME") or Path.home() / ".rustup")
        settings = rustup_home / "settings.toml"
        reject_linked_parents(settings)
        config_parent_identity(hasher, settings)
        add(hasher, str(settings))
        try:
            add_file(hasher, settings, "rustup-settings")
        except FileNotFoundError:
            add(hasher, "absent")
        for directory in (ROOT, *ROOT.parents):
            for name in ("rust-toolchain", "rust-toolchain.toml"):
                path = directory / name
                reject_linked_parents(path)
                config_parent_identity(hasher, path)
                add(hasher, str(path))
                try:
                    add_file(hasher, path, "toolchain-override")
                except FileNotFoundError:
                    add(hasher, "absent")
    elif proxy.stat().st_nlink > 1:
        # A hardlinked multicall proxy keeps the rustfmt basename. Without a
        # matching rustup executable its selected backend cannot be verified.
        raise Uncacheable("unverified hardlinked formatter")
    formatter_libraries(hasher, actual)


def fingerprint(command):
    if not hasattr(os, "environb"):
        raise Uncacheable("byte environment unavailable")
    if command[:3] != ["--edition", "2024", "--check"] or not command[3:]:
        raise Uncacheable("unknown rustfmt invocation")
    for arg in command[3:]:
        path = Path(arg)
        if (path.is_absolute() or path.suffix != ".rs" or ".." in path.parts
                or path.parts[0] not in SOURCE_ROOTS):
            raise Uncacheable("rustfmt root leaves source roots")
    if not all((ROOT / name).is_dir() for name in SOURCE_ROOTS):
        raise Uncacheable("source roots incomplete")
    if any(name.startswith("RUSTFMT_CONFIG") for name in os.environ):
        raise Uncacheable("external rustfmt config selection")
    if any(os.environ.get(name) for name in ("LD_AUDIT", "LD_PRELOAD")):
        raise Uncacheable("external formatter loader hooks")
    executable = shutil.which("rustfmt")
    if executable is None:
        raise Uncacheable("rustfmt unavailable")
    hasher = InputHashes()
    add(hasher, "check-format-cache-v1")
    add(hasher, str(ROOT))
    for arg in command:
        add(hasher, arg)
    for key, value in sorted(os.environb.items()):
        add(hasher, key)
        add(hasher, value)
    add_file(hasher, ROOT / "scripts" / "check-format", "checker")
    add_file(hasher, Path(__file__), "cache-helper")
    rustfmt_binary(hasher, executable)
    version = subprocess.check_output([executable, "--version"], cwd=ROOT, timeout=3)
    add(hasher, version)
    head = subprocess.check_output(
        ["git", "rev-parse", "--verify", "HEAD"], cwd=ROOT,
        stderr=subprocess.DEVNULL, timeout=3,
    )
    add(hasher, head)
    source_files(hasher)
    ancestor_configs(hasher)
    return hasher.hexdigest(), hasher.guard.hexdigest()


def cached(key):
    marker = CACHE / key
    return not marker.is_symlink() and marker.read_bytes() == b"ok\n"


def record(key):
    CACHE.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=CACHE, prefix=".pending-", delete=False) as output:
            temporary = Path(output.name)
            output.write(b"ok\n")
        os.replace(temporary, CACHE / key)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def run(command):
    return subprocess.run(["rustfmt", *command], cwd=ROOT, check=False).returncode


def main(command):
    try:
        # Initialize our own directory before source/config directory metadata
        # is captured, so creating target/ cannot invalidate the first marker.
        CACHE.mkdir(parents=True, exist_ok=True)
        before = fingerprint(command)
        try:
            if cached(before[0]) and fingerprint(command) == before:
                return 0
        except FileNotFoundError:
            pass
    except (Uncacheable, OSError, subprocess.SubprocessError, ValueError):
        return run(command)
    result = run(command)
    if result == 0:
        try:
            if fingerprint(command) == before:
                record(before[0])
        except (Uncacheable, OSError, subprocess.SubprocessError, ValueError):
            pass
    return result


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except OSError as error:
        sys.exit(f"could not run rustfmt: {error}")
