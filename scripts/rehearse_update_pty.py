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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--answer', default='')
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
            while match := re.search(rb'[^\r\n]*(?:\[y/N\]|\[Y/n\])\s*', pending):
                print(f'\n[rehearsal prompt={match.group().decode(errors="replace")!r} answer={args.answer!r}]', flush=True)
                os.write(fd, args.answer.encode()+b'\n')
                pending = pending[match.end():]
            pending = pending[-8192:]
    finally:
        os.close(fd)
        if timed_out:
            for _ in range(30):
                child, status = os.waitpid(pid, os.WNOHANG)
                if child:
                    return 124
                time.sleep(.1)
            os.killpg(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
    return 124 if timed_out else os.waitstatus_to_exitcode(status)


if __name__ == '__main__':
    sys.exit(main())
