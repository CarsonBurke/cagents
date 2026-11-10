#!/usr/bin/python3
"""Inject a daemon startup failure into a marked rehearsal's staged candidate."""
import hashlib
from pathlib import Path
import sys


def fail_start(root, version):
    root = Path(root).resolve()
    if (root / '.rehearse-update').read_text().strip() != str(root):
        raise ValueError('fault injection requires a matching rehearsal root')
    prefix = root / 'home/.local/share/cagents-install'
    if version in ('', '.', '..') or '/' in version:
        raise ValueError('fault injection requires a single version name')
    candidate = prefix / 'versions' / version
    if candidate.parent != prefix / 'versions' or (prefix / 'current').resolve() != candidate:
        raise ValueError('fault injection requires the staged current candidate')
    daemon = candidate / 'bin/cagentsd'
    original = daemon.with_name('cagentsd-rehearsal-real')
    if original.exists():
        raise ValueError('candidate already has a startup fault')
    daemon.rename(original)
    daemon.write_text('''#!/bin/sh
# Rehearsal fault: informational, inspection and store maintenance still work.
if [ "$#" -ne 0 ]; then
    exec "${0%/*}/cagentsd-rehearsal-real" "$@"
fi
printf '%s\\n' 'forced rehearsal daemon startup failure' >&2
exit 1
''')
    daemon.chmod(0o755)
    marker = candidate / '.cagents-daemon-protocol'
    if marker.exists():
        protocol, _ = marker.read_text().split()
        marker.write_text(f'{protocol} {hashlib.sha256(daemon.read_bytes()).hexdigest()}\n')
    (root / 'logs/startup-fault.txt').write_text(f'candidate={candidate}\noriginal_sha256={hashlib.sha256(original.read_bytes()).hexdigest()}\nfault_sha256={hashlib.sha256(daemon.read_bytes()).hexdigest()}\n')


if __name__ == '__main__':
    fail_start(*sys.argv[1:])
