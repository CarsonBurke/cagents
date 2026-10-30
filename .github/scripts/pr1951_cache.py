"""Fail before Cargo when the declared trusted warm cache is absent."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

matched = os.environ.get('MATCHED_CACHE_KEY', '')
expected = os.environ['EXPECTED_CACHE_PREFIX']
target = Path(os.environ['CARGO_TARGET_DIR'])
logs = Path(os.environ['EVIDENCE_DIR'])
record = {'matched_key': matched, 'expected_prefix': expected, 'target': str(target),
          'policy': 'FAIL_BEFORE_BUILD_ON_CACHE_MISS', 'qualification': False}
try:
    assert matched and matched.startswith(expected), 'No successful matched trusted cache restore'
    assert target.is_absolute() and target.is_dir() and not target.is_symlink()
    assert target.resolve(strict=True) == target
    deps = target / 'debug/deps'
    fingerprints = target / 'debug/.fingerprint'
    info = target / '.rustc_info.json'
    assert deps.is_dir() and fingerprints.is_dir() and info.is_file()
    assert next(deps.iterdir(), None) is not None
    assert next(fingerprints.iterdir(), None) is not None
    rustc = subprocess.check_output(['rustc', '-vV'], text=True)
    assert 'release: 1.93.0\n' in rustc
    expected_host = 'aarch64-apple-darwin' if sys.platform == 'darwin' else 'x86_64-unknown-linux-gnu'
    assert 'host: ' + expected_host + '\n' in rustc
    record.update(rustc=rustc, cargo=subprocess.check_output(['cargo', '--version'], text=True),
                  rustc_info_sha256=hashlib.sha256(info.read_bytes()).hexdigest(),
                  target_identity={'device': target.stat().st_dev, 'inode': target.stat().st_ino},
                  qualification=True,
                  scope='Trusted key and existing Cargo cache contents; not a guarantee every affected artifact is fresh or every future operation is a cache hit')
finally:
    (logs / 'warm-cache-admission.json').write_text(json.dumps(record, indent=2) + '\n')
