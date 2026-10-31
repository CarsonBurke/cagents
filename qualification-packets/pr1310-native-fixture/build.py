#!/usr/bin/env python3
"""Create only PR1310's public, disposable Git data; never launch the app."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time


BEFORE = "a3221af83d56663f0dcbef90f67b30ad6d9fe53d"
AFTER = "efa9288b3fd79a7214307a11b1c6d50c6cde3630"
BASE_TIME = 1_700_000_000
TOTAL_SECONDS = 600
COMMAND_SECONDS = 120
MAX_STREAM = 64 * 1024 * 1024
MAX_MARKS = 4 * 1024 * 1024
MAX_COMMAND_OUTPUT = 64 * 1024
OID = re.compile(r"[0-9a-f]{40}")
ROOT_NAME = re.compile(r"pr1310-native-fixture-[A-Za-z0-9][A-Za-z0-9._-]{0,79}")
GIT_FLAGS = [
    "--no-replace-objects", "--no-optional-locks", "--literal-pathspecs",
    "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
    "-c", "core.untrackedCache=false", "-c", "core.attributesFile=/dev/null",
    "-c", "submodule.recurse=false", "-c", "credential.helper=",
    "-c", "gc.auto=0", "-c", "maintenance.auto=false",
    "-c", "core.logAllRefUpdates=false", "-c", "core.autocrlf=false",
]


class Refusal(Exception):
    pass


def bounded_bytes(path: Path, maximum: int) -> bytes:
    if path.stat().st_size > maximum:
        raise Refusal(f"Output exceeds its byte bound: {path.name}")
    with path.open("rb") as stream:
        data = stream.read(maximum + 1)
    if len(data) > maximum:
        raise Refusal(f"Output grew beyond its byte bound: {path.name}")
    return data


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_json(path: Path, value: object) -> None:
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, ensure_ascii=False)
        stream.write("\n")


def private_destination(path: Path) -> tuple[int, int]:
    # An explicit fresh directory is required; never create or reuse a profile.
    if not path.is_absolute() or ".." in path.parts:
        raise Refusal("--output must be an absolute path without '..'")
    if path.parent not in (Path("/tmp"), Path("/var/tmp")) or not ROOT_NAME.fullmatch(path.name):
        raise Refusal("--output must be a direct /tmp or /var/tmp child named pr1310-native-fixture-...")
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or path.resolve(strict=True) != path:
        raise Refusal("--output must be a real directory without symlink components")
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise Refusal("--output must belong to this user and have mode0700")
    home = os.environ.get("HOME")
    if home and Path(home).is_absolute() and path.is_relative_to(Path(os.path.normpath(home))):
        raise Refusal("The fixture destination cannot be HOME or inside HOME")
    if any(path.iterdir()):
        raise Refusal("--output must be new and empty; partial builds are never overwritten")
    for parent in path.parents:
        if (parent / ".git").exists():
            raise Refusal("--output cannot be inside an existing Git worktree")
    return info.st_dev, info.st_ino


class Builder:
    def __init__(self, output: Path, git: Path):
        # Refuse routing/config/executable overrides before the first Git child.
        inherited = sorted(name for name in os.environ if name.startswith("GIT_"))
        if inherited:
            raise Refusal("Unsupported inherited Git variable names: " + ", ".join(inherited))
        self.identity = private_destination(output)
        if not git.is_absolute():
            raise Refusal("--git must be the caller-admitted absolute Git executable")
        self.git = git.resolve(strict=True)
        if not self.git.is_file() or not os.access(self.git, os.X_OK):
            raise Refusal("--git is not an executable regular file")
        self.root = output
        self.deadline = time.monotonic() + TOTAL_SECONDS
        self.commands: list[dict] = []
        # Preserve inherited HOME, CODEX_HOME and all caller authority unchanged.
        # Only Git config/attributes lookup and command output locale are isolated.
        self.environment = os.environ.copy()
        self.environment.update({
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_ATTR_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0", "LC_ALL": "C",
        })
        write_json(self.root / "BUILDING.json", {"kind": "PR1310_PRIVATE_GIT_DATA", "complete": False})
        self.logs = self.root / "command-evidence"
        self.logs.mkdir(mode=0o700)
        self.template = self.root / "empty-template"
        self.template.mkdir(mode=0o700)

    def check_root(self) -> None:
        info = self.root.lstat()
        if (info.st_dev, info.st_ino) != self.identity or not stat.S_ISDIR(info.st_mode):
            raise Refusal("The fixture destination was replaced")
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise Refusal("The fixture destination lost its private ownership")

    def git_command(self, cwd: Path, arguments: list[str], input_path: Path | None = None) -> bytes:
        self.check_root()
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise Refusal("The fixed total fixture-build deadline elapsed")
        timeout = min(COMMAND_SECONDS, remaining)
        number = len(self.commands) + 1
        stem = self.logs / f"{number:03d}"
        argv = [str(self.git), *GIT_FLAGS, *arguments]
        record = {"argv": argv, "cwd": str(cwd), "timeout_seconds": timeout,
                  "stdin": str(input_path) if input_path else None,
                  "stdout": str(stem.with_suffix(".stdout")),
                  "stderr": str(stem.with_suffix(".stderr"))}
        self.commands.append(record)
        if input_path and input_path.stat().st_size > MAX_STREAM:
            raise Refusal("The fast-import stream exceeds its fixed bound")
        started = time.monotonic()
        with contextlib.ExitStack() as stack:
            stdin = stack.enter_context(input_path.open("rb")) if input_path else subprocess.DEVNULL
            stdout = stack.enter_context(stem.with_suffix(".stdout").open("xb"))
            stderr = stack.enter_context(stem.with_suffix(".stderr").open("xb"))
            try:
                result = subprocess.run(argv, cwd=cwd, env=self.environment, stdin=stdin,
                                        stdout=stdout, stderr=stderr, timeout=timeout, check=False)
                record["exit_code"] = result.returncode
                record["timed_out"] = False
            except subprocess.TimeoutExpired:
                record["exit_code"] = None
                record["timed_out"] = True
            record["elapsed_seconds"] = time.monotonic() - started
        self.check_root()
        for kind in ("stdout", "stderr"):
            data = bounded_bytes(Path(record[kind]), MAX_COMMAND_OUTPUT)
            record[kind + "_sha256"] = sha256(data)
        write_json(stem.with_suffix(".json"), record)
        if record["timed_out"] or record["exit_code"] != 0:
            raise Refusal(f"Git command{number} failed; retain its original command-evidence files")
        return bounded_bytes(stem.with_suffix(".stdout"), MAX_COMMAND_OUTPUT)

    def initialize(self, name: str) -> Path:
        self.check_root()
        repo = self.root / name
        repo.mkdir(mode=0o700)
        self.git_command(repo, ["init", "--initial-branch=main", "--object-format=sha1",
                                "--template=" + str(self.template), "."])
        return repo


def hit_sets(count: int) -> dict[str, list[int]]:
    if count == 401:
        message = list(range(1, count + 1))
        author = [1, 201, 401]
        late = []
    else:
        message = [2, 2500, 12500, 18000, 24750, 24900, 24987, 25000]
        author = [3, 2501, 12501, 18001, 24751, 24901, 24988, 24999]
        late = [2, 2500, 12500, 18000]
    return {"message": message, "author": author, "late_only": late,
            "full_sha_literal_or": [count // 2 + 1, count - 2]}


def commit_data(name: str, index: int, hits: dict, sibling: str | None) -> tuple[str, str, str]:
    author = "Sparse Fixture Author" if index in hits["author"] else "Public Fixture Author"
    message = f"{name} commit {index:06d}"
    if index in hits["message"]:
        message += " pager-lantern" if name == "small401" else " sparse-lantern"
    if index in hits["late_only"]:
        message += " buried-needle"
    if index in hits["full_sha_literal_or"]:
        if sibling is None:
            raise Refusal("The real sibling OID must precede literal-OR commits")
        message += " literal sibling " + sibling
    content = f"PR1310 public inspection\nDataset: {name}\nCommit: {index:06d}\n{message}\nCafé / 猫\n"
    return author, message, content


def emit_data(stream, data: bytes) -> None:
    stream.write(f"data {len(data)}\n".encode("ascii") + data + b"\n")


def emit_commit(stream, branch: str, mark: int, parent: int | None, author: str,
                message: str, content: str, first: bool = False, sibling: bool = False) -> None:
    stream.write(f"commit refs/heads/{branch}\nmark :{mark}\n".encode("ascii"))
    timestamp = BASE_TIME + mark * 60
    stream.write(f"author {author} <fixture@example.invalid> {timestamp} +0000\n".encode("utf-8"))
    stream.write(f"committer Public Fixture Builder <builder@example.invalid> {timestamp} +0000\n".encode("ascii"))
    emit_data(stream, (message + "\n").encode("utf-8"))
    if parent is not None:
        stream.write(f"from :{parent}\n".encode("ascii"))
    files = {"history.txt": message + "\n", "inspection.txt": content}
    if first:
        files["README.md"] = "Synthetic public PR1310 Git data. No account, provider or private content.\n"
    if sibling:
        files["sibling-only.txt"] = content
    for path, text in files.items():
        stream.write(f"M 100644 inline {path}\n".encode("ascii"))
        emit_data(stream, text.encode("utf-8"))
    stream.write(b"\n")


def write_import(path: Path, name: str, count: int, first: int, last: int,
                 hits: dict, sibling: str | None, include_sibling: bool) -> None:
    with path.open("xb") as stream:
        for index in range(first, last + 1):
            author, message, content = commit_data(name, index, hits, sibling)
            emit_commit(stream, "main", index, index - 1 if index > 1 else None,
                        author, message, content, first=index == 1)
        if include_sibling:
            emit_commit(stream, "sibling", count + 1, count // 2, "Public Sibling Author",
                        f"{name} sibling inspection", f"{name}: sibling-only immutable content\n",
                        sibling=True)
        stream.write(b"done\n")
    if path.stat().st_size > MAX_STREAM:
        raise Refusal("The generated fast-import stream exceeds its byte bound")


def parse_marks(path: Path, count: int) -> dict[int, str]:
    marks = {}
    for line in bounded_bytes(path, MAX_MARKS).decode("ascii").splitlines():
        number, oid = line.split(" ")
        if not re.fullmatch(r":[1-9][0-9]*", number) or not OID.fullmatch(oid):
            raise Refusal("Malformed original Git export-marks output")
        key = int(number[1:])
        if key in marks:
            raise Refusal("Duplicate Git mark")
        marks[key] = oid
    if set(marks) != set(range(1, count + 2)):
        raise Refusal("The final Git marks do not cover the exact fixture commits")
    return marks


def build_history(builder: Builder, name: str, count: int) -> dict:
    repo = builder.initialize(name)
    evidence = builder.root / (name + "-source")
    evidence.mkdir(mode=0o700)
    hits = hit_sets(count)
    middle = count // 2
    prefix, suffix = evidence / "prefix.fi", evidence / "suffix.fi"
    prefix_marks, final_marks = evidence / "prefix.marks", evidence / "final.marks"
    write_import(prefix, name, count, 1, middle, hits, None, True)
    builder.git_command(repo, ["fast-import", "--quiet", "--date-format=raw",
                              "--export-marks=" + str(prefix_marks)], prefix)
    sibling = builder.git_command(repo, ["rev-parse", "--verify", "refs/heads/sibling"]).decode("ascii").strip()
    if not OID.fullmatch(sibling):
        raise Refusal("Git did not return a full SHA1 sibling identity")
    write_import(suffix, name, count, middle + 1, count, hits, sibling, False)
    builder.git_command(repo, ["fast-import", "--quiet", "--date-format=raw",
                              "--import-marks=" + str(prefix_marks),
                              "--export-marks=" + str(final_marks)], suffix)
    marks = parse_marks(final_marks, count)
    if marks[count + 1] != sibling:
        raise Refusal("Sibling identity changed between original imports")
    builder.git_command(repo, ["reset", "--hard", "refs/heads/main"])
    head = builder.git_command(repo, ["rev-parse", "--verify", "HEAD"]).decode("ascii").strip()
    if head != marks[count]:
        raise Refusal("HEAD is not the exact final mainline commit")
    for revision, expected in [("refs/heads/main", count), ("--all", count + 1),
                               ("main..sibling", 1), ("sibling..main", count - middle)]:
        actual = builder.git_command(repo, ["rev-list", "--count", revision]).decode("ascii").strip()
        if actual != str(expected):
            raise Refusal("Git graph cardinality differs from the generated fixture")
    if builder.git_command(repo, ["status", "--porcelain=v1", "--untracked-files=normal"]):
        raise Refusal("The newly checked-out fixture is not clean")
    records = {}
    index_path = evidence / "commits.jsonl"
    with index_path.open("x", encoding="utf-8") as index_file:
        for index in range(1, count + 1):
            author, message, content = commit_data(name, index, hits, sibling)
            record = {"oldest_first_index1": index, "head_first_index0": count - index,
                      "oid": marks[index], "parent_oid": marks.get(index - 1),
                      "author": author, "message": message,
                      "changed_file": "inspection.txt", "content": content,
                      "content_sha256": sha256(content.encode("utf-8"))}
            index_file.write(json.dumps(record, ensure_ascii=False) + "\n")
            if any(index in values for values in hits.values()) or index == count:
                records[index] = record
    queries = {"message": "pager-lantern" if count == 401 else "sparse-lantern",
               "author": "Sparse Fixture Author", "late_only": "buried-needle",
               "no_match": "absent-needle-8f31", "full_sha_literal_or": sibling,
               "refine_head": f"{name} commit {count:06d}", "clear": ""}
    return {"root": str(repo), "branch": "main", "head_oid": head,
            "mainline_commits": count, "all_commits": count + 1,
            "ordinary_page_sizes_at_200": [200, 200, 1] if count == 401 else None,
            "sibling": {"branch": "sibling", "oid": sibling, "parent_oid": marks[middle],
                        "changed_file": "sibling-only.txt",
                        "content": f"{name}: sibling-only immutable content\n"},
            "queries": queries,
            "known_hits": {kind: [records[index] for index in indices] for kind, indices in hits.items()},
            "head_inspection": records[count], "commit_index": str(index_path),
            "commit_index_sha256": sha256(bounded_bytes(index_path, MAX_STREAM)),
            "fast_import_sha256": {path.name: sha256(bounded_bytes(path, MAX_STREAM)) for path in [prefix, suffix]},
            "marks_sha256": sha256(bounded_bytes(final_marks, MAX_MARKS))}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="new empty private pr1310-native-fixture-* directory")
    parser.add_argument("--git", type=Path, required=True, help="caller-admitted absolute local Git executable")
    args = parser.parse_args()
    try:
        builder = Builder(args.output, args.git)
        small = build_history(builder, "small401", 401)
        large = build_history(builder, "large25000", 25_000)
        unborn = builder.initialize("unborn")
        if builder.git_command(unborn, ["for-each-ref", "--format=%(objectname)", "refs/heads"]):
            raise Refusal("The unborn repository unexpectedly has a branch commit")
        if builder.git_command(unborn, ["symbolic-ref", "HEAD"]).strip() != b"refs/heads/main":
            raise Refusal("The unborn repository has the wrong branch")
        builder.check_root()
        manifest = {"schema": "pr1310-public-git-fixture-v1", "root": str(builder.root),
                    "status": "GENERATED_GIT_DATA_ONLY_NOT_NATIVE_QUALIFICATION",
                    "builder_sha256": sha256(Path(__file__).read_bytes()),
                    "source_pair": {"before": BEFORE, "after": AFTER},
                    "small": small, "large": large,
                    "unborn": {"root": str(unborn), "branch": "main", "head_oid": None, "commits": 0},
                    "bounds": {"total_seconds": TOTAL_SECONDS, "per_command_seconds": COMMAND_SECONDS,
                               "fast_import_bytes": MAX_STREAM, "command_output_bytes": MAX_COMMAND_OUTPUT},
                    "git_commands": builder.commands,
                    "limits": ["No guarantee a query reaches a limit, times out, resolves its OID or exhausts.",
                               "Before has ordinary History but no query controls; do not simulate them.",
                               "No native input, screenshots, stale callback timing, performance or provider evidence.",
                               "The caller separately owns any runtime admission, process-family custody and native application profile."]}
        write_json(builder.root / "fixture.json", manifest)
        print(json.dumps({"fixture": str(builder.root / "fixture.json"),
                          "small_head": small["head_oid"], "large_head": large["head_oid"],
                          "sibling_oid": large["sibling"]["oid"]}))
        return 0
    except (Refusal, OSError, ValueError, UnicodeError) as error:
        print(f"Fixture build refused/incomplete: {error}. Keep partial output; do not reuse it.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
