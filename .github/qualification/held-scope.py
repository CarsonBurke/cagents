"""Temporary User-scoped qualification of one immutable product revision."""
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time

product, control = map(Path, sys.argv[1:3])
evidence = Path(os.environ['QUALIFICATION_EVIDENCE'])
evidence.mkdir(parents=True, exist_ok=True)
receipts = json.loads((evidence / "commands.json").read_text()) if (evidence / "commands.json").exists() else []
failures = []


def cancelled(signum, frame):
    raise RuntimeError('Qualification cancelled; closing the active command')


signal.signal(signal.SIGTERM, cancelled)


def user_context():
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise RuntimeError('Inherited caller authority is present; qualification refused')
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('A GitHub-hosted User process is required')


def git(root, *arguments):
    return subprocess.check_output(['git', '-C', str(root), *arguments], text=True).strip()


def unchanged():
    assert git(product, 'rev-parse', 'HEAD') == os.environ['PRODUCT_SHA']
    assert git(product, 'rev-parse', 'HEAD^{tree}') == os.environ['PRODUCT_TREE']
    assert not git(product, 'status', '--porcelain=v1')


def run(name, arguments, cargo_json=False, required=True):
    user_context()
    unchanged()
    started = time.monotonic()
    output = evidence / (name + ('.jsonl' if cargo_json else '.log'))
    errors = evidence / (name + '.stderr.log')
    with output.open('wb') as stdout, errors.open('wb') as stderr:
        process = subprocess.Popen(arguments, cwd=product, stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            code = process.wait(timeout=4200)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            raise RuntimeError(f'{name} timed out; qualification aborted before any later build')
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=10)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                pass
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
            raise
        result = subprocess.CompletedProcess(arguments, code)
    receipts.append({'name': name, 'argv': arguments, 'exit_code': result.returncode,
                     'elapsed_seconds': time.monotonic() - started})
    unchanged()
    if result.returncode:
        print(output.read_text(errors='replace')[-6000:])
        print(errors.read_text(errors='replace')[-6000:])
        failures.append(name)
        if required:
            raise RuntimeError(f'{name} failed with exit {result.returncode}')
    return output


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


try:
    user_context()
    unchanged()
    private_tmp = Path(tempfile.mkdtemp(prefix='h1922.', dir='/tmp')).resolve()
    os.chmod(private_tmp, 0o700)
    assert len(str(private_tmp)) < 30 and all(not (parent / '.git').exists() for parent in [private_tmp, *private_tmp.parents])
    os.environ['TMPDIR'] = str(private_tmp)
    (evidence / 'source.json').write_text(json.dumps({
        'product_sha': git(product, 'rev-parse', 'HEAD'),
        'product_tree': git(product, 'rev-parse', 'HEAD^{tree}'),
        'control_sha': git(control, 'rev-parse', 'HEAD'),
        'workflow_sha256': digest(control / '.github/workflows/qualify-held-scope.yml'),
        'setup_sha256': digest(control / '.github/actions/setup-linux/action.yml'),
        'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'cargo': subprocess.check_output(['cargo', '--version'], text=True).strip(),
        'zig': subprocess.check_output(['zig', 'version'], text=True).strip(),
        'run_id': os.environ['GITHUB_RUN_ID'], 'run_attempt': os.environ['GITHUB_RUN_ATTEMPT'],
        'physical_tmpdir': str(private_tmp),
    }, indent=2) + '\n')
    lane = ['--locked', '--no-default-features', '--features', 'ui']
    if '--suites-only' not in sys.argv[3:]:
        run('ui-check', ['cargo', 'check', *lane, '--bin', 'cagents-ui'])
        artifacts = run('ui-all-targets-compile', ['cargo', 'test', *lane, '--all-targets', '--no-run', '--message-format=json'], True)
        rows = [json.loads(line) for line in artifacts.read_text().splitlines() if line.startswith('{')]
        candidates = [row for row in rows if row.get('reason') == 'compiler-artifact'
                      and row.get('target', {}).get('name') == 'cagents'
                      and 'lib' in row.get('target', {}).get('kind', [])
                      and row.get('profile', {}).get('test') and row.get('executable')]
        assert len(candidates) == 1
        selected = candidates[0]
        assert 'ui' in selected['features']
        assert not {'host', 'integrated-browser'} & set(selected['features'])
        binary = selected['executable']
        (evidence / 'selected-test.json').write_text(json.dumps({
            'executable': binary, 'features': selected['features'],
            'sha256': digest(Path(binary)),
        }, indent=2) + '\n')
        for name in ['active_mixed_managed_saved_scope_recovers_healthy_panes_only',
                     'hidden_mixed_managed_saved_scope_recovers_healthy_panes_only',
                     'quarantined_chat_remove_routes_never_contact_or_close_the_inventory_owner']:
            test = 'app::shared_views::tests::startup_admission::' + name
            output = run(name, [binary, '--exact', test, '--nocapture', '--test-threads=1', '--color=never'])
            text = output.read_text()
            assert test in text and '1 passed; 0 failed' in text
    if '--focused-only' in sys.argv[3:]:
        sys.exit(0)
    run('cli-check', ['cargo', 'check', '--locked', '--no-default-features', '--bin', 'cagents'])
    host = ['--locked', '--no-default-features', '--features', 'host']
    run('host-check', ['cargo', 'check', *host, '--bin', 'cagentsd'])
    run('cli-all-targets-suite', ['cargo', 'test', '--locked', '--no-default-features', '--all-targets', '--', '--test-threads=1'], required=False)
    host_artifacts = run('host-helper-build', ['cargo', 'build', *host, '--bin', 'cagentsd', '--message-format=json'], True)
    rows = [json.loads(line) for line in host_artifacts.read_text().splitlines() if line.startswith('{')]
    helpers = [row['executable'] for row in rows if row.get('reason') == 'compiler-artifact'
               and row.get('target', {}).get('name') == 'cagentsd' and row.get('executable')
               and set(row.get('features', [])) == {'host'}]
    assert len(helpers) == 1
    os.environ['CAGENTS_TEST_RETAINED_ACTOR_HELPER'] = helpers[0]
    helper_hash = digest(Path(helpers[0]))
    (evidence / 'host-helper.json').write_text(json.dumps({'executable': helpers[0], 'sha256': helper_hash}, indent=2) + '\n')
    run('host-all-targets-suite', ['cargo', 'test', *host, '--all-targets', '--', '--test-threads=1'], required=False)
    assert digest(Path(helpers[0])) == helper_hash, 'HOST helper changed during suite execution'
    run('ui-all-targets-suite', ['cargo', 'test', *lane, '--all-targets', '--', '--test-threads=1'], required=False)
    if failures:
        raise RuntimeError('Qualification failures: ' + ', '.join(failures))
finally:
    (evidence / 'commands.json').write_text(json.dumps(receipts, indent=2) + '\n')
    files = sorted(path for path in evidence.iterdir() if path.is_file() and path.name != 'sha256.txt')
    (evidence / 'sha256.txt').write_text(''.join(f'{digest(path)}  {path.name}\n' for path in files))
