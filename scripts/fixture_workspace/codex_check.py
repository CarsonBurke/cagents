"""Reads every generated rollout back through a real codex, as cagents does.

`codex app-server` runs with CODEX_HOME at the fixture's .codex and a minimal
environment of its own, so it never sees the person's ~/.codex or
credentials. thread/read must return one turn per task_started; a rollout
codex cannot read, or reads with the wrong number of turns, is reported.
"""

import glob
import json
import os
import re
import select
import shutil
import subprocess
import time


def check(home, cwd, codex="codex", limit=20, log=print):
    executable = shutil.which(codex)
    if executable is None:
        log("No codex executable on PATH; skipping the Codex read-back check.")
        return None
    codex_home = os.path.join(home, ".codex")
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "CODEX_HOME": codex_home,
           "LANG": "C.UTF-8"}
    process = subprocess.Popen([executable, "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env=env, cwd=cwd, text=True)

    def send(message):
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()

    def wait(request):
        deadline = time.monotonic() + limit
        while time.monotonic() < deadline:
            ready, _, _ = select.select([process.stdout], [], [], 1)
            if ready:
                line = process.stdout.readline()
                if not line:
                    break
                message = json.loads(line)
                if message.get("id") == request:
                    return message
        return {"error": {"message": "no answer"}}

    try:
        send({"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "fixture-workspace", "version": "1"}}})
        wait(0)
        send({"method": "initialized"})
        files = sorted(glob.glob(os.path.join(codex_home, "sessions", "*", "*", "*", "*.jsonl")))
        mismatched = []
        for number, path in enumerate(files, 1):
            thread = re.search(r"([0-9a-f-]{36})\.jsonl$", path).group(1)
            with open(path) as file:
                expected = sum('"type":"task_started"' in line for line in file)
            send({"id": number, "method": "thread/read", "params": {"threadId": thread, "includeTurns": True}})
            answer = wait(number)
            turns = len(((answer.get("result") or {}).get("thread") or {}).get("turns") or [])
            if "error" in answer or turns != expected:
                mismatched.append((thread, expected, turns, (answer.get("error") or {}).get("message", "")))
        version = subprocess.run([executable, "--version"], capture_output=True, text=True, env=env).stdout.strip()
        log(f"{version}: read {len(files)} rollouts, {len(mismatched)} mismatched.")
        for thread, expected, turns, error in mismatched[:10]:
            log(f"  {thread}: expected {expected} turns, read {turns}. {error[:200]}")
        return mismatched
    finally:
        process.kill()
        process.wait()
