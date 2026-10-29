"""Temporary User-scoped qualification of one immutable product revision."""
import hashlib
import json
import os
import signal
import stat
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


def file_identity(info):
    return dict(device=info.st_dev, inode=info.st_ino, size=info.st_size,
                mode=stat.S_IMODE(info.st_mode), uid=info.st_uid, links=info.st_nlink,
                mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns)


def observe_helper(path):
    before = path.lstat()
    assert stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid()
    observation = dict(path=str(path), identity=file_identity(before), sha256=digest(path))
    assert file_identity(path.lstat()) == observation['identity'], 'Helper changed during observation'
    return observation


def freeze_helper(source, artifact, directory):
    source = Path(source).resolve(strict=True)
    target = Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True)
    assert source.is_relative_to(target)
    assert not directory.is_relative_to(target)
    directory.mkdir(mode=0o700)
    frozen = directory / 'cagentsd'
    copied_digest = hashlib.sha256()
    with os.fdopen(os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC), 'rb') as original:
        before = os.fstat(original.fileno())
        assert stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid() and before.st_mode & 0o111
        descriptor = os.open(frozen, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
        with os.fdopen(descriptor, 'wb') as output:
            for block in iter(lambda: original.read(1024 * 1024), b''):
                copied_digest.update(block)
                output.write(block)
            output.flush()
            os.fchmod(output.fileno(), 0o500)
            os.fsync(output.fileno())
        assert file_identity(os.fstat(original.fileno())) == file_identity(before)
    observed = observe_helper(frozen)
    assert observed['sha256'] == copied_digest.hexdigest()
    assert observed['identity']['links'] == 1 and observed['identity']['mode'] == 0o500
    assert (observed['identity']['device'], observed['identity']['inode']) != (before.st_dev, before.st_ino)
    record = dict(head=os.environ['PRODUCT_SHA'], tree=os.environ['PRODUCT_TREE'],
                  artifact=artifact, source=dict(path=str(source), identity=file_identity(before),
                                                 sha256=copied_digest.hexdigest()), frozen=observed)
    (evidence / 'host-helper.json').write_text(json.dumps(record, indent=2) + '\n')
    return frozen, observed


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
        version = subprocess.run(['systemd-run', '--version'], stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, timeout=5)
        (evidence / 'systemd-run-version.log').write_bytes(version.stdout)
        state_output = run('atspi-disabled-state-tests', ['cargo', 'test', '--locked',
                           '-p', 'accesskit_atspi_common', '--lib',
                           'node::disabled_state_tests::', '--', '--test-threads=1', '--color=never'])
        state_text = state_output.read_text()
        for selector in ['disabled_action_controls_do_not_advertise_enabled_or_sensitive',
                         'read_only_text_state_behavior_is_preserved']:
            assert state_text.splitlines().count('test node::disabled_state_tests::' + selector + ' ... ok') == 1
        assert 'test result: ok. 2 passed; 0 failed; 0 ignored;' in state_text
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
                     'quarantined_chat_remove_routes_never_contact_or_close_the_inventory_owner',
                     'ordinary_closed_retained_chat_history_and_same_process_reopen_dispatch_only_the_explicit_resume',
                     'keyboard_saved_draft_copy_preserves_the_first_attachment_locations_click',
                     'saved_pane_check_results_survive_transport_recovery_and_follow_the_current_source',
                     'quarantined_pane_removal_restores_the_surviving_native_control']:
            test = 'app::shared_views::tests::startup_admission::' + name
            output = run(name, [binary, '--exact', test, '--nocapture', '--test-threads=1', '--color=never'], required=False)
            text = output.read_text()
            if name not in failures:
                assert test in text and '1 passed; 0 failed' in text
        for test in [
            'app::notification_ui::tests::notification_snapshot_before_model_reprojects_canonical_unread_without_another_snapshot',
            'app::workspace_model_sync::tests::model_read_publishes_new_managed_chat_before_initialized_view_projection',
            'app::shared_views::tests::startup_admission::verified_retirement_survives_repeated_refresh_without_borrowing_metadata_sequence',
            'app::shared_views::tests::startup_admission::verified_retirement_never_borrows_rejected_ambiguous_or_changed_sources',
            'app::shared_views::tests::startup_admission::closed_retirement_history_still_requires_exact_proof_and_scope',
            'app::shared_views::tests::startup_admission::verified_retirement_keeps_unsaved_work_without_manufacturing_failed_proof',
            'app::shared_views::tests::startup_admission::retirement_witness_requires_the_full_issuer_before_canonical_admission',
            'app::shared_views::tests::startup_admission::verified_terminal_retirement_preserves_exact_ended_and_archived_sources',
            'persistence::tests::migration_session_inventory_budget_spans_windows_and_precedes_json_decode',
            'terminal::hosted::tests::systemd_scope_preserves_environment_arguments_and_isolates_cgroup',
        ]:
            name = test.rsplit('::', 1)[1]
            output = run(name, [binary, '--exact', test, '--nocapture', '--test-threads=1', '--color=never'], required=False)
            text = output.read_text()
            if name not in failures:
                assert test in text and '1 passed; 0 failed' in text
        if failures:
            raise RuntimeError('User startup qualification failed: ' + ', '.join(failures))
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
    selected_artifact = next(row for row in rows if row.get('reason') == 'compiler-artifact'
                             and row.get('executable') == helpers[0] and row.get('target', {}).get('name') == 'cagentsd')
    helper, helper_before = freeze_helper(helpers[0], selected_artifact, private_tmp / 'frozen-host-helper')
    os.environ['CAGENTS_TEST_RETAINED_ACTOR_HELPER'] = str(helper)
    assert observe_helper(helper) == helper_before, 'HOST helper changed before suite execution'
    try:
        run('host-all-targets-suite', ['cargo', 'test', *host, '--all-targets', '--', '--test-threads=1'], required=False)
    finally:
        preceding_error = repr(sys.exc_info()[1]) if sys.exc_info()[1] else None
        try:
            helper_after = observe_helper(helper)
        except BaseException as error:
            (evidence / 'host-helper-after.json').write_text(json.dumps({
                'preceding_error': preceding_error, 'observation_error': repr(error), 'matches': False,
            }, indent=2) + '\n')
            raise
        (evidence / 'host-helper-after.json').write_text(json.dumps({
            'preceding_error': preceding_error, 'observed': helper_after, 'matches': helper_after == helper_before,
        }, indent=2) + '\n')
        assert helper_after == helper_before, 'HOST helper changed during suite execution'
    run('ui-all-targets-suite', ['cargo', 'test', *lane, '--all-targets', '--', '--test-threads=1'], required=False)
    if failures:
        raise RuntimeError('Qualification failures: ' + ', '.join(failures))
finally:
    (evidence / 'commands.json').write_text(json.dumps(receipts, indent=2) + '\n')
    files = sorted(path for path in evidence.iterdir() if path.is_file() and path.name != 'sha256.txt')
    (evidence / 'sha256.txt').write_text(''.join(f'{digest(path)}  {path.name}\n' for path in files))
