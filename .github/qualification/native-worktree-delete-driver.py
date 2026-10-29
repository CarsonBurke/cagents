"""PR1984 design-stage source/fixture/forensic controller. Never merge.

Only --source-only is ready. Native entry refuses before imports, fixture
creation, desktop setup or build. The manifest names the remaining flow gaps.
Definitions below are reviewable preparation, not executed native evidence.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import stat
import subprocess
import tempfile
import time


def digest_bytes(value):
    return hashlib.sha256(value).hexdigest()


def git(root, *args):
    return subprocess.check_output(
        ['git', '--no-optional-locks', '-C', str(root), *args], text=True).strip()


def tree_entries(root, revision):
    output = subprocess.check_output(
        ['git', '--no-optional-locks', '-C', str(root), 'ls-tree', '-rz', revision])
    return dict(row.split(b'\t', 1)[::-1] for row in output.split(b'\0') if row)


def source_report(manifest, before, after, control):
    expected_paths = set(manifest['feature_paths'])
    main = manifest['current_main']['head']
    before_entries = tree_entries(before, manifest['before']['head'])
    after_entries = tree_entries(after, manifest['after']['head'])
    main_entries = tree_entries(after, main)
    records = {}
    for name, root in [('before', before), ('after', after)]:
        pin = manifest[name]
        record = dict(head=git(root, 'rev-parse', 'HEAD'),
                      tree=git(root, 'rev-parse', 'HEAD^{tree}'),
                      status=git(root, 'status', '--porcelain=v1', '--untracked-files=all'))
        assert record['head'] == pin['head'] and record['tree'] == pin['tree']
        assert record['status'] == '', 'Product checkout changed'
        record['source_sha256'] = {
            path: digest_bytes((root / path).read_bytes()) for path in manifest['source_paths']}
        assert record['source_sha256'] == manifest['source_sha256'][name]
        records[name] = record
    assert git(before, 'show', '-s', '--format=%P', 'HEAD').split() == [
        manifest['original_pr_head'], main], 'Before must be the ordinary two-parent merge'
    changes = {}
    for name, entries in [('before', before_entries), ('after', after_entries)]:
        changed = {p.decode() for p in entries.keys() | main_entries.keys()
                   if entries.get(p) != main_entries.get(p)}
        assert changed <= expected_paths, 'Unowned product change in composition'
        changes[name] = sorted(changed)
    assert changes == manifest['changed_paths_from_main']
    correction = sorted(p.decode() for p in before_entries.keys() | after_entries.keys()
                        if before_entries.get(p) != after_entries.get(p))
    assert correction == manifest['correction_paths']
    for path in manifest['current_main_guard_paths']:
        key = path.encode()
        assert main_entries[key] == before_entries[key] == after_entries[key], path
    # Existing renderer/UI source, quarantine/authority and whole-terminal
    # teardown guards are current-main bytes, not donor runtime qualification.
    return dict(status='SOURCE_ONLY_NATIVE_UNRUN', products=records,
                before_parents=[manifest['original_pr_head'], main],
                changed_paths_from_main=changes, correction_paths=correction,
                conserved_current_main_entries={
                    name: sum(entries.get(p) == value for p, value in main_entries.items())
                    for name, entries in [('before', before_entries), ('after', after_entries)]},
                guard_paths=manifest['current_main_guard_paths'],
                control_head=git(control, 'rev-parse', 'HEAD'),
                native_gaps=manifest['native_gaps'])


def require_disposable(root):
    root = Path(root)
    assert root.is_absolute() and root == root.resolve(strict=True)
    assert root.parent == Path('/tmp') and root.name.startswith('worktree-delete-native-')
    info = root.lstat()
    assert stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
    assert stat.S_IMODE(info.st_mode) == 0o700
    marker_path = root / 'fixture-owner.json'
    assert not marker_path.is_symlink()
    marker = json.loads(marker_path.read_text())
    assert marker['uid'] == os.getuid() and marker['root'] == str(root)
    assert marker['identity'] == [info.st_dev, info.st_ino]
    assert len(marker['nonce']) == 64 and marker['purpose'] == 'PR1984-disposable-only'
    return marker


def fixture_path(root, relative, present=True):
    require_disposable(root)
    path = root / relative
    assert path.is_relative_to(root) and '..' not in Path(relative).parts
    assert path != root and not path.is_symlink()
    if present:
        assert path.exists()
        assert path.resolve(strict=True).is_relative_to(root)
    else:
        assert not path.exists() and not path.is_symlink()
        assert path.parent.resolve(strict=True).is_relative_to(root)
    return path


def create_fixture(native):
    """Future granted hosted execution only; no caller-chosen existing root."""
    native.guard()
    root = Path(tempfile.mkdtemp(prefix='worktree-delete-native-', dir='/tmp'))
    root.chmod(0o700)
    info = root.stat()
    (root / 'fixture-owner.json').write_text(json.dumps(dict(
        root=str(root), uid=os.getuid(), identity=[info.st_dev, info.st_ino],
        nonce=secrets.token_hex(32), purpose='PR1984-disposable-only')) + '\n')
    native.put(native.OUT / 'disposable-root.json', require_disposable(root))

    def fixture_git(path, *args):
        require_disposable(root)
        assert path.resolve(strict=True).is_relative_to(root)
        # Only these synthetic repositories ignore global/system Git config.
        return native.git(path, *args, fixture=True)

    for role in ['A', 'B']:
        repo = root / role
        repo.mkdir(mode=0o700)
        fixture_git(repo, 'init', '--quiet', '-b', 'fixture-primary-' + role.lower())
        fixture_git(repo, 'config', 'user.name', 'Disposable native fixture')
        fixture_git(repo, 'config', 'user.email', 'fixture@example.invalid')
        (repo / '.gitignore').write_bytes(b'/.worktrees/\n/ignored-native.txt\n')
        (repo / 'editorproof.txt').write_bytes(b'native-saved-proof\n')
        fixture_git(repo, 'add', '.gitignore', 'editorproof.txt')
        fixture_git(repo, 'commit', '--quiet', '-m', 'Private disposable fixture')
    rows = [('A', 'outer', 'outer'), ('A', 'own', 'own'),
            ('A', 'same-parent', 'same-parent'),
            ('A', 'same-child', 'same-parent/.worktrees/same-child'),
            ('A', 'absent', 'absent'),
            ('B', 'foreign', 'outer/.worktrees/foreign')]
    for repository, branch, relative in rows:
        path = root / relative
        fixture_git(root / repository, 'worktree', 'add', '--quiet', '-b', branch, str(path))
    for relative in ['outer/.worktrees/foreign', 'same-parent/.worktrees/same-child']:
        path = fixture_path(root, relative)
        (path / 'editorproof.txt').write_bytes(b'native-uncommitted-proof\n')
        (path / 'ignored-native.txt').write_bytes(b'native-ignored-proof\x00\xff\n')
        (path / 'untracked-native.txt').write_bytes(b'native-untracked-proof\n')
    (root / 'parked').mkdir(mode=0o700)
    # No deletion, pruning, registry repair, provider or app command occurs here.
    return root


def retain_bytes(path, destination):
    """No-follow raw forensic copy, with concurrent-write observation retained."""
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
    descriptor = os.open(path, flags)
    try:
        before = os.fstat(descriptor)
        assert stat.S_ISREG(before.st_mode)
        with os.fdopen(os.dup(descriptor), 'rb') as stream:
            data = stream.read()
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with destination.open('xb') as stream:
        stream.write(data)
    changed = (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
        after.st_size, after.st_mtime_ns, after.st_ctime_ns)
    return dict(path=str(path), retained=str(destination), sha256=digest_bytes(data),
                bytes=len(data), identity=[before.st_dev, before.st_ino],
                source_changed_during_read=changed,
                consistency='Raw observed bytes; not a transaction-consistent database snapshot')


def retain_fixture_and_databases(native, root, app_root, label):
    native.guard()
    require_disposable(root)
    assert app_root.is_relative_to(root) and not app_root.is_symlink()
    destination = native.OUT / label
    destination.mkdir(mode=0o700)
    files = {}
    for relative in ['outer', 'own', 'same-parent', 'absent', 'B']:
        path = root / relative
        files[relative] = {}
        if not path.exists():
            continue
        for directory, names, leaves in os.walk(path, followlinks=False):
            names[:] = sorted(n for n in names if n != '.git' and not (Path(directory) / n).is_symlink())
            for leaf in sorted(leaves):
                source = Path(directory) / leaf
                assert not source.is_symlink(), 'Unexpected fixture symlink: do not dereference'
                key = str(source.relative_to(path))
                files[relative][key] = retain_bytes(source, destination / 'files' / relative / key)
    databases = {}
    for source in sorted((app_root / 'data').rglob('*')):
        if source.is_file() and any(source.name.endswith(suffix) for suffix in (
                '.sqlite3', '.sqlite3-wal', '.sqlite3-shm')):
            key = str(source.relative_to(app_root))
            databases[key] = retain_bytes(source, destination / 'source-db-bytes' / key)
    registrations = {}
    for role in ['A', 'B']:
        registrations[role] = {
            operation: native.git(root / role, *argv, fixture=True)
            for operation, argv in [
                ('registrations', ['worktree', 'list', '--porcelain', '-z']),
                ('refs', ['show-ref', '--head']), ('status', ['status', '--porcelain=v1'])]}
    value = dict(files=files, source_db_bytes=databases, git=registrations,
                 retained_root=str(root), monotonic=time.monotonic(),
                 limits='Live raw DB copies preserve observed bytes; use actual native/CLI state for semantic assertions.')
    native.put(destination / 'conservation.json', value)
    return value


def assert_foreign_bytes_and_registration_kept(before, after):
    for role in ['outer/.worktrees/foreign', 'same-parent/.worktrees/same-child']:
        parent, prefix = ('outer', '.worktrees/foreign/') if role.startswith('outer') else (
            'same-parent', '.worktrees/same-child/')
        old = {key: value['sha256'] for key, value in before['files'][parent].items()
               if key.startswith(prefix)}
        new = {key: value['sha256'] for key, value in after['files'][parent].items()
               if key.startswith(prefix)}
        assert old and old == new, 'Nested saved/uncommitted/ignored/untracked bytes changed'
    assert before['git']['B'] == after['git']['B'], 'Foreign registrations/refs changed'


def native_target(native, pid, label, prefix=False, enabled=True):
    """Actual PID-filtered AX node only; absence/ambiguity refuses input."""
    found = [node for node in native.tree(pid)
             if (node.name.startswith(label) if prefix else node.name == label)
             and native.visible(node)
             and (not enabled or node.getState().contains(native.pyatspi.STATE_ENABLED))]
    buttons = [node for node in found if node.getRole() == native.pyatspi.ROLE_PUSH_BUTTON]
    found = buttons or found
    assert len(found) <= 1, 'Ambiguous actual native target'
    return found[0] if found else None


def native_checkpoint(native, pid, xid, executable, env, size, label):
    native.guard()
    owner = native.identity(pid, executable, env)
    assert int(native.query(['getwindowpid', xid])) == pid
    native.CLIENT_RECT = native.geometry(xid)
    pixels = native.capture(xid, label, tuple(size))
    native.snapshot(pid, label)
    assert owner == native.identity(pid, executable, env)
    native.put(native.OUT / (label + '-owner.json'), dict(owner=owner, xid=xid,
        pixels=pixels, qualification='Native pixels/AX observation only; independent visual/UX review required'))
    return pixels


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('control', type=Path)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    parser.add_argument('--source-only', action='store_true')
    parser.add_argument('--receipt', type=Path)
    args = parser.parse_args()
    manifest = json.loads((args.control / 'research/worktree-delete-native/manifest.json').read_text())
    if not args.source_only:
        # Deliberately before fixture/native imports, environment changes,
        # subprocesses, build or process ownership. A design packet is no grant.
        raise SystemExit('UNQUALIFIED_DESIGN: native action sequence and absent/replacement '
                         'UI reachability require exact controller review and root/EC native grant; '
                         'no effect executed. See manifest native_gaps.')
    report = source_report(manifest, args.before, args.after, args.control)
    if args.receipt:
        with args.receipt.open('x') as stream:
            json.dump(report, stream, indent=2)
            stream.write('\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
