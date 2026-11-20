#!/usr/bin/python3
"""Create a verified startup-failing candidate only inside a marked rehearsal."""
import sys

sys.dont_write_bytecode = True

import argparse
import hashlib
from pathlib import Path
import shlex
import shutil
import tarfile
import tempfile

from fixture_home import require_fixture_home


def private_root(root, version):
    root = Path(root).resolve()
    require_fixture_home('HOME', root)
    require_fixture_home('HOME', root / 'home')
    marker = root / '.rehearse-update'
    if marker.is_symlink() or marker.read_text().strip() != str(root):
        raise ValueError('fault injection requires a matching rehearsal root')
    if version in ('', '.', '..') or '/' in version:
        raise ValueError('fault injection requires a single version name')
    require_fixture_home('XDG_DATA_HOME', root / 'home/.local/share/cagents-install')
    for path in (root / 'logs', root / 'logs/daemon-start-failure.txt', root / 'logs/startup-fault.txt'):
        require_fixture_home('HOME', path)
    return root


def wrap_daemon(root, version, candidate, helper=None):
    root = private_root(root, version)
    require_fixture_home('HOME', candidate)
    daemon = candidate / 'bin/cagentsd'
    if daemon.is_symlink() or not daemon.is_file():
        raise ValueError('fault injection requires a regular candidate daemon')
    original = helper or daemon.with_name('cagentsd-rehearsal-real')
    require_fixture_home('HOME', original)
    marker = candidate / '.cagents-daemon-protocol'
    require_fixture_home('HOME', marker)
    shell_helper = root / 'bin/lib/fixture-home.sh'
    require_fixture_home('HOME', shell_helper)
    if helper:
        if original.is_symlink() or not original.is_file():
            raise ValueError('fault introspection requires the exact private daemon helper')
        delegate = shlex.quote(str(original))
    else:
        if original.exists():
            raise ValueError('candidate already has a startup fault')
        daemon.rename(original)
        delegate = '"${0%/*}/cagentsd-rehearsal-real"'
    shell_helper.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(Path(__file__).resolve().parents[1] / 'tests/fixtures/lib/fixture-home.sh', shell_helper)
    receipt = shlex.quote(str(root / 'logs/daemon-start-failure.txt'))
    daemon.write_text(f'''#!/bin/sh
. {shlex.quote(str(shell_helper))}
require_fixture_home HOME {shlex.quote(str(root))}
export HOME={shlex.quote(str(root / 'home'))}
export XDG_RUNTIME_DIR={shlex.quote(str(root / 'rt'))}
export XDG_DATA_HOME={shlex.quote(str(root / 'home/.local/share'))}
export XDG_CONFIG_HOME={shlex.quote(str(root / 'home/.config'))}
export XDG_CACHE_HOME={shlex.quote(str(root / 'home/.cache'))}
export XDG_STATE_HOME={shlex.quote(str(root / 'home/.local/state'))}
export CLAUDE_CONFIG_DIR={shlex.quote(str(root / 'home/.claude'))}
export CODEX_HOME={shlex.quote(str(root / 'home/.codex'))}
require_fixture_home HOME
require_fixture_home XDG_RUNTIME_DIR
require_fixture_home XDG_DATA_HOME
require_fixture_home XDG_CONFIG_HOME
require_fixture_home XDG_CACHE_HOME
require_fixture_home XDG_STATE_HOME
require_fixture_home CLAUDE_CONFIG_DIR
require_fixture_home CODEX_HOME
require_fixture_home HOME {receipt}
# Informational and authenticated source-inspection entrypoints never serve.
case ${{1-}} in
    --help|--version|--stored-schema)
        if [ "$#" -eq 1 ]; then exec {delegate} "$@"; fi ;;
    --downgrade-store|--inspect-upgrade-source|--capture-update-floor|--retain-update-floor)
        if [ "$#" -eq 2 ]; then exec {delegate} "$@"; fi ;;
esac
printf '%s %s\\n' {shlex.quote(version)} \"$$\" > {receipt} || exit 2
printf '%s\\n' 'forced rehearsal daemon startup failure' >&2
exit 1
''')
    daemon.chmod(0o755)
    if marker.exists():
        protocol, _ = marker.read_text().split()
        marker.write_text(f'{protocol} {hashlib.sha256(daemon.read_bytes()).hexdigest()}\n')
    with original.open('rb') as stream:
        original_hash = hashlib.file_digest(stream,'sha256').hexdigest()
    (root / 'logs/startup-fault.txt').write_text(f'candidate={candidate}\noriginal_sha256={original_hash}\nfault_sha256={hashlib.sha256(daemon.read_bytes()).hexdigest()}\n')


def fail_start(root, version):
    root = private_root(root, version)
    prefix = root / 'home/.local/share/cagents-install'
    candidate = prefix / 'versions' / version
    require_fixture_home('HOME', candidate)
    if candidate.parent != prefix / 'versions' or (prefix / 'current').resolve() != candidate:
        raise ValueError('fault injection requires the staged current candidate')
    wrap_daemon(root, version, candidate)


def failing_archive(root, version, archive_dir):
    root = private_root(root, version)
    archive_dir = Path(archive_dir).resolve()
    name = f'cagents-{version}-x86_64-unknown-linux-gnu.tar.gz'
    source = archive_dir / name
    with source.open('rb') as stream:
        source_hash = hashlib.file_digest(stream, 'sha256').hexdigest()
    expected = [line.split()[0] for line in (archive_dir/'SHA256SUMS').read_text().splitlines()
                if len(line.split()) == 2 and line.split()[1].lstrip('*') == name]
    if expected != [source_hash]:
        raise ValueError('startup fault requires the checksum-verified source archive')
    destination = root / 'startup-fault-archive'
    require_fixture_home('HOME', destination)
    destination.mkdir(mode=0o700)
    with tempfile.TemporaryDirectory(prefix='tree-',dir=destination) as temporary:
        tree = Path(temporary)
        top = f'cagents-{version}'
        with tarfile.open(source,'r:gz') as archive:
            members = archive.getmembers()
            if (len(members)>20000 or sum(member.size for member in members)>2*1024**3
                    or any(Path(member.name).parts[0] != top for member in members)):
                raise ValueError('unexpected or oversized candidate archive')
            archive.extractall(tree,filter='data')
        # The installer intentionally accepts an exact archive layout. Keep
        # real introspection companions in this private fault root, never add
        # an extra executable to the release tree or edit a staged candidate.
        helpers = destination/'introspection-bin'
        helpers.mkdir(mode=0o700)
        for product in ('cagents','cagentsd'):
            companion = tree/top/'bin'/product
            if companion.is_symlink() or not companion.is_file():
                raise ValueError('fault introspection requires regular matched companions')
            shutil.copy2(companion,helpers/product)
        wrap_daemon(root,version,tree/top,helper=helpers/'cagentsd')
        target = destination/name
        with tarfile.open(target,'w:gz') as archive:
            archive.add(tree/top,arcname=top)
    with target.open('rb') as stream:
        target_hash = hashlib.file_digest(stream,'sha256').hexdigest()
    (destination/'SHA256SUMS').write_text(f'{target_hash}  {name}\n')
    with (root/'logs/startup-fault.txt').open('a') as stream:
        stream.write(f'source_archive_sha256={source_hash}\nfault_archive_sha256={target_hash}\narchive={target}\n')
    return destination


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root',type=Path)
    parser.add_argument('version')
    parser.add_argument('--archive-dir',type=Path)
    args = parser.parse_args()
    if args.archive_dir:
        print(failing_archive(args.root,args.version,args.archive_dir))
    else:
        fail_start(args.root,args.version)
