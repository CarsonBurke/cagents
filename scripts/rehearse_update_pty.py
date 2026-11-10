#!/usr/bin/python3
"""Run an update on a controlling PTY, answering each observed confirmation."""
import argparse
import errno
import os
import pty
import re
import select
import signal
import sys
import time

# The updater's prompts: an ordinary confirmation, the choice offered while
# agents are mid-turn, the review of what will stop, and the typed destructive
# confirmation. Each ends where the updater starts reading its answer.
PROMPTS = re.compile(
    rb'(?P<confirm>[^\r\n]*(?:\[y/N\]|\[Y/n\])[ \t]*)'
    rb'|(?P<working>[^\r\n]*i: interrupt them now[^\r\n]*n: cancel[ \t]*\r?\n)'
    rb'|(?P<review>[^\r\n]*Enter: review what stops[^\r\n]*n: cancel[ \t]*\r?\n)'
    rb'|(?P<stop>[^\r\n]*Anything else cancels\.[ \t]*\r?\n)')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--answer', default='',
                        help='typed at each [Y/n] or [y/N] confirmation')
    parser.add_argument('--working', default='',
                        help='typed at the working-agents choice; empty waits for them')
    parser.add_argument('--stop', default='',
                        help='typed at the destructive stop list; empty cancels')
    parser.add_argument('--timeout', type=float, default=240)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.timeout <= 0:
        parser.error('a command and positive timeout are required')
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)
    deadline = time.monotonic()+args.timeout
    pending = b''
    timed_out = False
    try:
        while True:
            if time.monotonic() >= deadline:
                timed_out = True
                print('\nrehearse-update: update exceeded PTY timeout', flush=True)
                os.killpg(pid, signal.SIGTERM)
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
                if match.group('confirm'):
                    answer = args.answer
                elif match.group('working'):
                    answer = args.working
                elif match.group('stop'):
                    answer = args.stop
                else:
                    answer = ''
                print(f'\n[rehearsal prompt={match.group().decode(errors="replace").strip()!r} answer={answer!r}]', flush=True)
                os.write(fd, answer.encode()+b'\n')
                pending = pending[match.end():]
            pending = pending[-8192:]
    finally:
        if timed_out:
            for _ in range(30):
                child, status = os.waitpid(pid, os.WNOHANG)
                if child:
                    break
                time.sleep(.1)
            else:
                os.killpg(pid, signal.SIGKILL)
                _, status = os.waitpid(pid, 0)
        else:
            _, status = os.waitpid(pid, 0)
        os.close(fd)
    return 124 if timed_out else os.waitstatus_to_exitcode(status)


if __name__ == '__main__':
    sys.exit(main())
