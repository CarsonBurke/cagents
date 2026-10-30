"""Research source and frozen-helper observations; no product behavior."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root, text=True).strip()


def helper_observation(logs, label, expected=None, required=True):
    metadata = logs / 'frozen-helper.json'
    if expected is None:
        assert not required, 'Original frozen helper baseline is required'
        assert not metadata.exists(), 'Helper metadata exists without original published baseline'
        value = {'state': 'no-qualified-frozen-helper',
                 'scope': 'Qualification was skipped or failed before frozen-helper publication; no claim whether Cargo produced an executable'}
    else:
        assert metadata.exists(), 'Expected frozen helper metadata disappeared'
        assert json.loads(metadata.read_text())['frozen'] == expected, 'Frozen helper metadata changed from its original baseline'
        path = Path(expected['path'])
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        with os.fdopen(fd, 'rb') as stream:
            before = os.fstat(stream.fileno())
            assert stat.S_ISREG(before.st_mode)
            assert stat.S_IMODE(before.st_mode) == 0o500
            assert before.st_uid == os.geteuid() and before.st_nlink == 1
            identity = lambda value: (value.st_dev, value.st_ino, value.st_uid,
                                      value.st_mode, value.st_nlink, value.st_size,
                                      value.st_mtime_ns, value.st_ctime_ns)
            value = dict(path=str(path), device=before.st_dev, inode=before.st_ino,
                         uid=before.st_uid, mode=oct(stat.S_IMODE(before.st_mode)),
                         type='regular', nlink=before.st_nlink, size=before.st_size,
                         mtime_ns=before.st_mtime_ns, ctime_ns=before.st_ctime_ns,
                         sha256=None)
            checksum = hashlib.sha256()
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                checksum.update(block)
            value['sha256'] = checksum.hexdigest()
            assert identity(os.fstat(stream.fileno())) == identity(before)
        named = path.lstat()
        assert identity(named) == identity(before)
        for name in ['device', 'inode', 'uid', 'mode', 'type', 'nlink', 'size',
                     'mtime_ns', 'ctime_ns', 'sha256']:
            assert value[name] == expected[name], (name, value, expected)
        value['state'] = 'original frozen helper unchanged'
    (logs / ('helper-' + label + '.json')).write_text(json.dumps(value, indent=2) + '\n')
    return value


def observe(stage):
    root = Path.cwd()
    logs = Path(os.environ['EVIDENCE_DIR'])
    research = Path(os.environ['GITHUB_WORKSPACE']) / 'research'
    manifest_path = research / '.github/scripts/pr1951-product-source.json'
    manifest = json.loads(manifest_path.read_text())
    state = {'head': git(root, 'rev-parse', 'HEAD'),
             'tree': git(root, 'rev-parse', 'HEAD^{tree}'),
             'research_head': git(research, 'rev-parse', 'HEAD'),
             'research_tree': git(research, 'rev-parse', 'HEAD^{tree}'),
             'product_status': git(root, 'status', '--porcelain', '--untracked-files=all'),
             'research_status': git(research, 'status', '--porcelain', '--untracked-files=all'),
             'files': {}, 'research_files': {}}
    try:
        assert state['head'] == manifest['product_head'] == os.environ['EXPECTED_PRODUCT_SHA']
        assert state['tree'] == manifest['product_tree'] == os.environ['EXPECTED_PRODUCT_TREE']
        assert state['research_head'] == os.environ['EXPECTED_RESEARCH_SHA']
        assert not state['product_status'] and not state['research_status']
        for name, expected in manifest['files'].items():
            raw = (root / name).read_bytes()
            entry = {'sha256': digest(raw), 'bytes': len(raw),
                     'git_entry': git(root, 'ls-tree', 'HEAD', '--', name)}
            state['files'][name] = entry
            assert entry == expected, name
        for path in sorted((research / '.github/scripts').glob('pr1951*')):
            assert path.is_file() and not path.is_symlink()
            state['research_files'][str(path.relative_to(research))] = digest(path.read_bytes())
        path = research / '.github/workflows/research-pr1853-bd88.yml'
        state['research_files'][str(path.relative_to(research))] = digest(path.read_bytes())
        if sys.platform == 'linux':
            setup = root.parent / 'setup-main'
            assert git(setup, 'rev-parse', 'HEAD') == 'c727f886628897c1749290213e9d65037253095a'
            assert git(setup, 'rev-parse', 'HEAD^{tree}') == '6d4f36e2b4dd48fc9744305d34c78469384edcc5'
            assert not git(setup, 'status', '--porcelain', '--untracked-files=all')
            assert digest((setup / '.github/actions/setup-linux-build/action.yml').read_bytes()) == '82a5284e0640b3c6648e3b9b3d6e0e0b323ada03eb087cbf95b9b63ca3d0c052'
        if stage != 'before':
            before = json.loads((logs / 'source-before.json').read_text())
            assert all(state[name] == before[name] for name in state)
    finally:
        (logs / ('source-' + stage + '.json')).write_text(json.dumps(state, indent=2) + '\n')
        if sys.platform == 'linux' and stage != 'before':
            baseline = os.environ.get('FROZEN_HELPER_BASELINE', '')
            expected = json.loads(baseline) if baseline else None
            helper_observation(logs, stage, expected,
                               required=os.environ.get('QUALIFICATION_OUTCOME') == 'success' or expected is not None)


if __name__ == '__main__':
    assert len(sys.argv) == 2 and sys.argv[1] in ['before', 'after-always']
    observe(sys.argv[1])
