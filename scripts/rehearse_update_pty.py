#!/usr/bin/python3
"""Run an update on a controlling PTY, answering each observed confirmation."""
import argparse
import errno
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import sys
import time
import stat

# The updater's prompts: an ordinary confirmation, the choice offered while
# agents are mid-turn, and the typed destructive confirmation. Each ends where
# the updater starts reading its answer.
PROMPTS = re.compile(
    rb'(?P<confirm>(?:[^\r\n]*(?:\[y/N\]|\[Y/n\])[ \t]*'
    rb'|[^\r\n]*Enter: review what stops[ \t]+n: cancel[ \t]*\r?\n))'
    rb'|(?P<working>[^\r\n]*i: interrupt them now[^\r\n]*n: cancel[ \t]*\r?\n)'
    rb'|(?P<stop>[^\r\n]*Anything else cancels\.[ \t]*\r?\n)')


def capture_review(root, output, index):
    """Retain the real private confirmation before answering its prompt."""
    root = Path(root).resolve()
    marker = root / '.rehearse-update'
    destination = Path(output).resolve()
    if (marker.is_symlink() or marker.read_text().strip() != str(root)
            or not destination.is_relative_to(root / 'retry') or destination.is_symlink()):
        raise ValueError('review capture requires a marked root and its private retry directory')
    destination.mkdir(mode=0o700, exist_ok=True)
    descriptor = os.open(root / 'home/.local/share/cagents-install/.update/status.json', os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise ValueError('review status must be an owned private regular file')
        raw = stream.read(8 * 1024 * 1024 + 1)
    if len(raw) > 8 * 1024 * 1024 or not json.loads(raw).get('confirmation'):
        raise ValueError('prompt has no bounded actual confirmation')
    with (destination / f'prompt-{index}.status.json').open('xb') as stream:
        stream.write(raw)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--answer', default='',
                        help='typed at each [Y/n] or [y/N] confirmation')
    parser.add_argument('--working', default='',
                        help='typed at the working-agents choice; empty waits for them')
    parser.add_argument('--stop', default='',
                        help='typed at the destructive stop list; empty cancels')
    parser.add_argument('--require-working', action='store_true',
                        help='decline and fail if the first review is not the working-agents choice')
    parser.add_argument('--timeout', type=float, default=240)
    parser.add_argument('--review-root', type=Path)
    parser.add_argument('--review-output', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.timeout <= 0:
        parser.error('a command and positive timeout are required')
    if bool(args.review_root) != bool(args.review_output):
        parser.error('--review-root and --review-output must be supplied together')
    if args.require_working and any(arg=='--yes' or arg.startswith('--yes=') for arg in command):
        parser.error('--require-working forbids --yes because it bypasses the actual working choice')
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)
    deadline = time.monotonic()+args.timeout
    pending = b''
    timed_out = False
    prompt_count = 0
    working_observed = False
    case_invalid = False
    try:
        while True:
            if time.monotonic() >= deadline:
                timed_out = True
                print('\nrehearse-update: update exceeded PTY timeout', flush=True)
                try:
                    os.killpg(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                break
            if not select.select([fd], [], [], .2)[0]:
                continue
            try:
                data = os.read(fd, 65536)
            except OSError as exc:
                if exc.errno == errno.EIO:
                    break
                raise
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            pending += data
            # These are the updater's confirmation markers, including prompts
            # arriving across separate PTY reads. Do not queue answers early.
            while match := PROMPTS.search(pending):
                prompt_count += 1
                if args.review_root:
                    capture_review(args.review_root, args.review_output, prompt_count)
                answer = (args.answer if match.group('confirm') else
                          args.working if match.group('working') else args.stop)
                if args.require_working and prompt_count == 1 and not match.group('working'):
                    case_invalid = True
                    print('\nrehearse-update: case-invalid: required working-agents choice was not offered; declining', flush=True)
                working_observed |= bool(match.group('working'))
                if case_invalid:
                    answer = '' if match.group('stop') else 'n'
                print(f'\n[rehearsal prompt={match.group().decode(errors="replace").strip()!r} answer={answer!r}]', flush=True)
                os.write(fd, answer.encode()+b'\n')
                pending = pending[match.end():]
            pending = pending[-8192:]
    except BaseException:
        timed_out = True
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        raise
    finally:
        blocked = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            if timed_out:
                for _ in range(30):
                    child, status = os.waitpid(pid, os.WNOHANG)
                    if child:
                        break
                    time.sleep(.1)
                else:
                    try:
                        os.killpg(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    _, status = os.waitpid(pid, 0)
            else:
                _, status = os.waitpid(pid, 0)
        finally:
            os.close(fd)
            signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
    if args.require_working and not working_observed:
        print('\nrehearse-update: case-invalid: no working-agents choice was observed', flush=True)
        case_invalid = True
    return 124 if timed_out else 2 if case_invalid else os.waitstatus_to_exitcode(status)


if __name__ == '__main__':
    sys.exit(main())
