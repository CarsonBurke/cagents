#!/usr/bin/python3
"""Failure-safe retention of declared synthetic SDK receipts, never app secrets."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat

evidence = Path(os.environ['NATIVE1757_EVIDENCE']).resolve(strict=True)
for arm in ('before', 'after'):
    output = evidence / arm
    profile = output / 'profile.json'
    if not profile.is_file():
        continue
    description = json.loads(profile.read_text())
    root = Path(description['root'])
    assert root.parent == Path('/tmp') and root.name.startswith('native1757-')
    if not root.exists():
        continue
    assert root.resolve(strict=True) == root and root.stat().st_uid == os.getuid()
    assert stat.S_IMODE(root.stat().st_mode) == 0o700
    marker = json.loads((root / '.native1757-owned.json').read_text())
    assert marker['purpose'] == '1757-hosted-native' and marker['root'] == str(root)
    assert marker['nonce'] == description['nonce'] and marker['window'] == description['window']
    for name in ('provider-starts.jsonl', 'protocol-states.jsonl', 'probe-attempts.jsonl',
                 'accepted-first-send.json', 'accepted-recovery.json'):
        path = root / name
        if path.exists():
            info = path.lstat()
            assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_size <= 32768
            shutil.copy2(path, output / name)
    # The only history writer is the pinned offline fixture. Limit traversal
    # and bytes; do not copy app data/config, provider credentials or cookies.
    paths = sorted((root / 'account').rglob('*.jsonl'))
    assert len(paths) <= 2
    for path in paths:
        assert path.resolve(strict=True).is_relative_to(root / 'account')
        info = path.lstat()
        assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_size <= 4 * 1024 * 1024
        destination = output / 'synthetic-provider-history' / path.relative_to(root / 'account')
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, destination)
files = {}
for path in sorted(evidence.rglob('*')):
    if path.is_file() and path.name != 'SHA256SUMS.json':
        result = hashlib.sha256()
        with path.open('rb') as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                result.update(chunk)
        files[str(path.relative_to(evidence))] = result.hexdigest()
(evidence / 'SHA256SUMS.json').write_text(json.dumps(files, indent=2) + '\n')
