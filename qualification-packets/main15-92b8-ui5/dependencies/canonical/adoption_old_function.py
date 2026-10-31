def settle_children():
    forced = False
    observed = []
    deadline = time.monotonic() + 3
    while True:
        while time.monotonic() < deadline:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
                if pid == 0:
                    break
            except ChildProcessError:
                break
        remaining = children()
        if not remaining:
            return {'clean': True, 'forced': forced, 'observed': observed}
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    observed = remaining
    deadline = time.monotonic() + 10
    while remaining and time.monotonic() < deadline:
        forced = True
        for child in remaining:
            if time.monotonic() >= deadline:
                break
            try:
                fd = os.pidfd_open(child['pid'])
                try:
                    if any(c['pid'] == child['pid'] and c['start'] == child['start'] for c in children()):
                        signal_pidfd(fd, signal.SIGKILL)
                finally:
                    os.close(fd)
            except OSError as error:
                if error.errno != errno.ESRCH:
                    raise
        time.sleep(0.05)
        while time.monotonic() < deadline:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
                if pid == 0:
                    break
            except ChildProcessError:
                break
        remaining = children()
    return {'clean': not remaining, 'forced': forced, 'observed': observed, 'remaining': remaining}
