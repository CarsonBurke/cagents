#!/usr/bin/env python3
"""Private owning fixture only: record the peer's real user/assistant frames.

The supplied session is parsed by the shell peer from its actual process argv;
this helper neither chooses a provider identity nor fabricates resume authority.
"""
import json
import os
from pathlib import Path
import stat
import sys
import uuid

root = Path(__file__).resolve().parent.parent
home = Path(os.environ["HOME"]).resolve(strict=True)
cwd = Path.cwd().resolve(strict=True)
assert home == root / "home" and cwd == root / "project"
assert root.name.startswith("cagents-owning-close-")
assert root.stat().st_uid == os.getuid() and stat.S_IMODE(root.stat().st_mode) == 0o700
session = sys.argv[1]
assert str(uuid.UUID(session)) == session
user = json.loads(sys.argv[2])
assistant_line = sys.stdin.buffer.readline(1024 * 1024 + 1)
assert 0 < len(assistant_line) <= 1024 * 1024
assistant = json.loads(assistant_line)
assert user["type"] == "user" and assistant["type"] == "assistant"
assert user.get("session_id", session) == session
assert assistant["message"]["role"] == "assistant"
# Match Claude's project-name contract for these bounded ASCII private paths.
project = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(cwd))
assert len(project) <= 200
config = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(home / ".claude"))).resolve()
assert config == home / ".claude"
directory = config / "projects" / project
directory.mkdir(parents=True, mode=0o700, exist_ok=True)
assert directory.resolve() == directory
path = directory / (session + ".jsonl")
fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
try:
    metadata = os.fstat(fd)
    assert stat.S_ISREG(metadata.st_mode) and metadata.st_uid == os.getuid()
    assert stat.S_IMODE(metadata.st_mode) == 0o600 and metadata.st_nlink == 1
    assert metadata.st_size <= 1024 * 1024
    user_id = str(uuid.uuid5(uuid.UUID(session), json.dumps(user, sort_keys=True)))
    records = [dict(user, uuid=user_id, parentUuid=None, sessionId=session, cwd=str(cwd)),
               dict(assistant, parentUuid=user_id, sessionId=session, cwd=str(cwd))]
    payload = b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
    with os.fdopen(fd, "ab", closefd=False) as transcript:
        transcript.write(payload)
        transcript.flush()
        os.fsync(transcript.fileno())
finally:
    os.close(fd)
# Forward the same generated wire frame only after its private history is durable.
sys.stdout.buffer.write(assistant_line)
sys.stdout.buffer.flush()
