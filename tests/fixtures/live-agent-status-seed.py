#!/usr/bin/python3
"""Seed an invented Claude status transcript inside native-evidence's fixture.

Usage: live-agent-status-seed.py ROOT
The seed contains twelve historical background launches with no end receipt,
four completed agents, and a failed agent that the live phase resumes.
The next prompt replays live-agent-status-live.jsonl via the synthetic provider.
"""
import argparse
import json
import os
from pathlib import Path
import pwd
import re
import stat


def seed(root):
    root = Path(root).resolve(strict=True)
    real_home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()
    home = root / "home"
    if home.resolve() == real_home or Path(os.environ["HOME"]).resolve() != home.resolve():
        raise ValueError("run through native-evidence env with its private fixture HOME")
    if root.stat().st_uid != os.getuid() or stat.S_IMODE(root.stat().st_mode) & 0o077:
        raise ValueError("the fixture root must be owned by this user and private")
    project = (root / "project").resolve(strict=True)
    config = home / ".claude"
    if config.is_symlink():
        raise ValueError("the fixture Claude home cannot be a symlink")
    directory = config / "projects" / re.sub(r"[^A-Za-z0-9]", "-", str(project))
    directory.mkdir(parents=True, exist_ok=True)
    here = Path(__file__).resolve().parent
    rows = [json.loads(line)
            for line in (here / "live-agent-status-history.jsonl").read_text().splitlines()]
    for row in rows:
        if row.get("cwd") == "@PROJECT@":
            row["cwd"] = str(project)
    session = rows[0]["sessionId"]
    path = directory / f"{session}.jsonl"
    with path.open("x") as output:
        for row in rows:
            output.write(json.dumps(row, ensure_ascii=False) + "\n")
    (config / "fixture-frames.jsonl").write_bytes((here / "live-agent-status-live.jsonl").read_bytes())
    (config / "fixture-reply.md").write_text("")
    print(json.dumps({"session": session, "transcript": str(path), "project": str(project)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("root")
    args = parser.parse_args()
    seed(args.root)
