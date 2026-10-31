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
    # Current parentage and a freshly opened pidfd do not establish original
    # custody or permission to stop an adopted provider descendant. Leave
    # unproven survivors unsignaled; the caller must retain its C/HOLD state.
    return {'clean': False, 'forced': forced, 'observed': remaining, 'remaining': remaining}
