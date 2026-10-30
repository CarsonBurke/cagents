#!/usr/bin/python3
"""Exact read-only synthetic data for one owned native UI. No network fallback."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import time


def private_file(path):
    info = path.lstat()
    assert stat.S_ISREG(info.st_mode) and not stat.S_ISLNK(info.st_mode)
    assert info.st_uid == os.getuid() and info.st_nlink == 1
    assert stat.S_IMODE(info.st_mode) == 0o600
    return path.read_bytes()


def ancestry():
    result, current = [], os.getpid()
    for _ in range(24):
        process = Path('/proc') / str(current)
        try:
            fields = (process / 'stat').read_text().rsplit(')', 1)[1].split()
            result.append({'pid': current, 'parent': int(fields[1]),
                           'executable': str((process / 'exe').resolve(strict=True))})
        except PermissionError:
            result.append({'pid': current, 'inaccessible': True})
            break
        except (FileNotFoundError, ProcessLookupError):
            break
        if current <= 1:
            break
        current = int(fields[1])
    return result


def main():
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise RuntimeError('Inherited caller authority; synthetic read refused without changes')
    script = Path(sys.argv[0]).absolute()
    root = script.parent.parent
    assert root.resolve() == root and script.resolve() == script
    for directory in (root, script.parent, root / 'gh-data'):
        info = directory.lstat()
        assert stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
        assert stat.S_IMODE(info.st_mode) == 0o700
    config = json.loads(private_file(root / 'gh-data' / 'manifest.json'))
    argv, cwd = sys.argv[1:], str(Path.cwd())
    choices = [case for case in config['allowlist'] if case['argv'] == argv]
    case = choices[0] if len(choices) == 1 else None
    allowed_cwd = cwd in config['allowed_cwd'] and (
        case is None or case['case'] != 'discover-repository' or cwd == str(root / 'project'))
    if not allowed_cwd:
        case = None
    record = {'argv': argv, 'cwd': cwd, 'pid': os.getpid(), 'ppid': os.getppid(),
              'ancestry': ancestry(), 'monotonic': time.monotonic(), 'unix_ns': time.time_ns(),
              'script_sha256': hashlib.sha256(script.read_bytes()).hexdigest(),
              'case': case['case'] if case else 'UNEXPECTED',
              'scope': 'Synthetic local read data; discovery remains unauthenticated.'}
    output = b''
    if case:
        response = root / 'gh-data' / case['response']
        output = private_file(response)
        assert hashlib.sha256(output).hexdigest() == case['response_sha256']
        record.update(response=str(response), response_sha256=case['response_sha256'],
                      stdout_bytes=len(output), exit_code=0)
    else:
        record.update(stdout_bytes=0, exit_code=77)
    calls = root / 'gh-calls.jsonl'
    fd = os.open(calls, os.O_RDWR | os.O_APPEND | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
        assert info.st_nlink == 1 and stat.S_IMODE(info.st_mode) == 0o600
        fcntl.flock(fd, fcntl.LOCK_EX)
        prior = os.read(fd, 1024 * 1024).splitlines()
        assert len(prior) < 256, 'Bounded private read call limit reached'
        assert len(prior) % 2 == 0, 'An earlier private response has no successful delivery receipt'
        record['sequence'] = len(prior) // 2 + 1
        os.write(fd, (json.dumps({'event': 'request', **record}) + '\n').encode())
        if case:
            sys.stdout.buffer.write(output)
            sys.stdout.buffer.flush()
        else:
            sys.stderr.write('UNEXPECTED private native gh command or cwd; no fallback\n')
        os.write(fd, (json.dumps({'event': 'delivered', **record}) + '\n').encode())
    finally:
        os.close(fd)
    return record['exit_code']


if __name__ == '__main__':
    sys.exit(main())
