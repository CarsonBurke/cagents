#!/usr/bin/python3
"""Serial exact-source native arms in a genuine disposable hosted User job."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
BASE = 'bcd82ef6cce2033da86dfb6c4fad1dfcda62f48a'
PRODUCTS = [
    ('before', '3c3b1fef5bbddbba360b125df4658a8c9b8112eb', '077d3edf6e253594b90ff1416f3a9dfd485a75f6'),
    ('after', 'd4de5ce658c9f5524628891e1d3849e5e3d47895', '5616effbb028c40fafda8d7f5ca087cff8b4fbd5'),
]
OBSERVER = 'bb1e8fd0d14444697198d6c495425ab18038ff2d389d7eee2e29f3f463fe3a6a'
SOURCE_FILES = ['Cargo.toml', 'Cargo.lock', 'build.rs', 'scripts/run-development',
                'src/app/chat_ui.rs', 'src/app/chat_ui/first_start_checkpoint_tests.rs',
                'src/app/shared_views.rs', 'src/host/chat.rs', 'src/chat/protocol/claude.rs']


def natural_user():
    if any(name.startswith('CAGENTS') or name in ('AUTH', 'AGENT_TOKEN', 'SESSION_TOKEN')
           for name in os.environ):
        raise RuntimeError('Inherited role/context is incompatible with genuine User qualification')
    assert os.environ.get('GITHUB_ACTIONS') == 'true'
    assert os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'


def sha(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def git(*args):
    return subprocess.check_output(['git', *args], text=True, timeout=30).strip()


def main():
    natural_user()  # Before any fixture/bootstrap/path isolation.
    if sys.flags.optimize:
        raise RuntimeError('Optimized Python is forbidden')
    evidence = Path(os.environ['NATIVE1757_EVIDENCE']).resolve(strict=True)
    control = json.loads((evidence / 'control.json').read_text())
    assert git('rev-parse', 'HEAD') == control['head']
    assert git('status', '--porcelain') == ''
    assert control['base'] == BASE
    assert sha(HERE / 'observe-command-owned.py') == OBSERVER
    for name, expected in control['script_sha256'].items():
        assert sha(HERE / name) == expected
    # Only a single ordinary CI job writes this private target; never the
    # workstation's shared C target or daemon socket.
    target = Path(os.environ['NATIVE1757_TARGET']).resolve()
    target.mkdir(mode=0o700)
    summary = {'scope': 'Actual hosted X11 native; offline SDK, synthetic state only',
               'control': control, 'arms': [], 'status': 'RUNNING', 'native_review': 'PENDING'}
    try:
        for arm, head, tree in PRODUCTS:
            natural_user()
            assert git('status', '--porcelain') == ''
            subprocess.run(['git', 'checkout', '--detach', head], check=True, timeout=30)
            assert git('rev-parse', 'HEAD') == head and git('rev-parse', 'HEAD^{tree}') == tree
            assert git('status', '--porcelain') == ''
            output = evidence / arm
            output.mkdir(mode=0o700)
            hashes = {}
            for name in SOURCE_FILES:
                data = subprocess.check_output(['git', 'show', head + ':' + name], timeout=15)
                assert hashlib.sha256(data).hexdigest() == sha(name)
                destination = output / 'source' / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                hashes[name] = hashlib.sha256(data).hexdigest()
            save(output / 'source-pins.json', {'head': head, 'tree': tree, 'files': hashes})
            for name, expected in control['script_sha256'].items():
                assert sha(HERE / name) == expected
            result = subprocess.run(['/usr/bin/timeout', '--signal=TERM', '--kill-after=15s', '3300s',
                         '/usr/bin/python3', str(HERE / 'observe-command-owned.py'),
                         '--output', str(output / 'guardian'), '--timeout', '3270', '--',
                         '/usr/bin/python3', str(HERE / 'native-arm.py'), '--arm', arm,
                         '--source', head, '--tree', tree, '--checkout', str(Path.cwd()),
                         '--output', str(output), '--target', str(target)], timeout=3325)
            record = {'arm': arm, 'source': head, 'tree': tree, 'outer_exit': result.returncode}
            metadata_path = output / 'guardian/COMMAND.json'
            assert metadata_path.is_file(), 'No guardian packet: fail and retain custody uncertainty'
            metadata = json.loads(metadata_path.read_text())
            cleanup = metadata.get('cleanup', {})
            record['guardian'] = metadata
            record['qualified'] = (
                result.returncode == metadata['actual_exit'] == metadata['command_exit'] == 0
                and metadata['exit_reason'] == 'exited' and not metadata['timed_out']
                and cleanup.get('complete') is True and cleanup.get('leader_reaped') is True
                and cleanup.get('remaining') == [] and cleanup.get('membership_uncertain') == []
                and not [r for r in cleanup.get('initial_census', []) if r['state'] != 'Z']
                and metadata.get('sampler_closed') is True)
            summary['arms'].append(record)
            save(evidence / 'SUMMARY.json', summary)
            assert record['qualified'], 'Stop at first failure or unsettled family; do not run the next arm'
            assert json.loads((output / 'RESULT.json').read_text())['status'] == 'PASS'
            assert git('rev-parse', 'HEAD') == head and git('rev-parse', 'HEAD^{tree}') == tree
            assert git('status', '--porcelain') == ''
        summary['status'] = 'EXECUTION_PASS_REQUIRES_INDEPENDENT_NATIVE_UX_REVIEW'
    finally:
        save(evidence / 'SUMMARY.json', summary)
        # Raw failures, failed first invocations and all earlier states remain.
        inventory = {str(p.relative_to(evidence)): sha(p) for p in sorted(evidence.rglob('*'))
                     if p.is_file() and p.name != 'SHA256SUMS.json'}
        save(evidence / 'SHA256SUMS.json', inventory)


if __name__ == '__main__':
    main()
