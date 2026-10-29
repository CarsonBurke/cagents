"""Reuse completed same-head suites; qualify native behavior separately."""
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys
import urllib.request
import zipfile

product, control = map(Path, sys.argv[1:3])
if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or 'CAGENTS_AGENT_AUTHORITY' in os.environ:
    raise RuntimeError('A genuine GitHub-hosted User context is required; authority is unchanged')
pin = json.loads((control / '.github/qualification/completed-suites.json').read_text())
head = '017ccd7f878abbb2fcad19d53e6ec2b6c426abc6'
tree = 'ef14211ef4051ca8e665adb890379b3ab2a532d6'
controller = '8313f249080b160783af5574b1640ccdda8812fe'
run_id, job_id = 40921866095, 123616969646
workflow_hash = 'ae43e0d7a0f7a352489b1020170e807c975eff8c5e0b0c5f4db83830eea333eb'
suite_hash = '01d4a0d38f7bd36f30ca5e69537b32dc71482a6b40ee38b2659eae6387a41ed2'
setup_hash = '82a5284e0640b3c6648e3b9b3d6e0e0b323ada03eb087cbf95b9b63ca3d0c052'
repo = 'CarsonBurke/cagents'
api = 'https://api.github.com/repos/' + repo
raw = 'https://raw.githubusercontent.com/' + repo + '/'


def sha(data):
    return hashlib.sha256(data).hexdigest()


def fetch(url, limit):
    request = urllib.request.Request(url, headers={'User-Agent': 'held-scope-suite-provenance',
                                                  'Accept': 'application/vnd.github+json'})
    # Public reads only. No workflow token or additional permission is needed.
    with urllib.request.urlopen(request, timeout=30) as response:
        data = response.read(limit + 1)
        assert len(data) <= limit, 'Response exceeds its fixed size bound'
        return data


def get(path):
    return json.loads(fetch(api + path, 2 * 1024 * 1024))


def git(*args):
    return subprocess.check_output(['git', '-C', str(product), *args], text=True).strip()


assert os.environ['PRODUCT_SHA'] == head and os.environ['PRODUCT_TREE'] == tree
assert git('rev-parse', 'HEAD') == head and git('rev-parse', 'HEAD^{tree}') == tree
assert not git('status', '--porcelain=v1')
assert sha((control / '.github/qualification/held-scope.py').read_bytes()) == suite_hash
assert sha((control / '.github/actions/setup-linux/action.yml').read_bytes()) == setup_hash
assert sha(fetch(raw + controller + '/.github/workflows/qualify-held-scope.yml', 1024 * 1024)) == workflow_hash
assert sha(fetch(raw + controller + '/.github/qualification/held-scope.py', 1024 * 1024)) == suite_hash

run = get('/actions/runs/' + str(run_id))
assert run['id'] == run_id and run['run_attempt'] == 1
assert run['status'] == 'completed' and run['conclusion'] == 'failure'
assert run['head_sha'] == controller and run['head_branch'] == 'qual/held-saved-scope-fw16'
assert run['event'] == 'pull_request' and run['path'] == '.github/workflows/qualify-held-scope.yml'
assert run['repository']['id'] == 1347863312 and run['repository']['full_name'] == repo
assert [p['number'] for p in run['pull_requests']] == [1933]
jobs = get('/actions/runs/' + str(run_id) + '/jobs?per_page=100')
assert jobs['total_count'] == 1 and len(jobs['jobs']) == 1
job = jobs['jobs'][0]
assert job['id'] == job_id and job['run_id'] == run_id and job['run_attempt'] == 1
assert job['head_sha'] == controller and job['name'] == 'user-ui'
assert job['status'] == 'completed' and job['conclusion'] == 'failure'
assert all(s['status'] == 'completed' for s in job['steps'])
assert [s['name'] for s in job['steps'] if s['conclusion'] == 'failure'] == [
    'Capture actual native X11 before and after evidence']
for name in ['Require a genuine hosted User context', 'Verify the pinned setup action',
             'Check exact UI source and execute focused User regressions',
             'Upload native interaction evidence', 'Run every affected isolated suite after native cleanup',
             'Recompute all artifact checksums', 'Upload source and validation receipts']:
    assert len([s for s in job['steps'] if s['name'] == name and s['conclusion'] == 'success']) == 1, name
assert len([s for s in job['steps'] if s['name'] == 'Preserve the hosted lane after validation'
            and s['conclusion'] == 'skipped']) == 1

assert isinstance(pin['artifact_id'], int) and pin['artifact_id'] > 0
assert re.fullmatch(r'[0-9a-f]{40}', pin['evidence_commit'])
assert re.fullmatch(r'[0-9a-f]{64}', pin['archive_sha256'])
assert 0 < pin['archive_size'] <= 32 * 1024 * 1024
artifact = get('/actions/artifacts/' + str(pin['artifact_id']))
assert artifact['id'] == pin['artifact_id'] and not artifact['expired']
assert artifact['name'] == 'held-scope-' + head + '-1'
assert artifact['digest'] == 'sha256:' + pin['archive_sha256']
assert artifact['size_in_bytes'] == pin['archive_size']
assert artifact['workflow_run']['id'] == run_id and artifact['workflow_run']['head_sha'] == controller
assert artifact['workflow_run']['repository_id'] == 1347863312
assert artifact['workflow_run']['head_repository_id'] == 1347863312
archive = fetch(raw + pin['evidence_commit'] + '/suite-receipts/40921866095-full.zip', 32 * 1024 * 1024)
assert len(archive) == pin['archive_size'] and sha(archive) == pin['archive_sha256']

# Do not extract the archive or mix old source/commands/native failure into the
# successor evidence. Validate every entry and checksum under strict bounds.
with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
    members = zipped.infolist()
    assert 0 < len(members) <= 2000
    assert sum(m.file_size for m in members) <= 128 * 1024 * 1024
    files = {}
    for member in members:
        path = PurePosixPath(member.filename)
        assert member.filename and not path.is_absolute() and '..' not in path.parts
        assert '\\' not in member.filename and not stat.S_ISLNK(member.external_attr >> 16)
        assert not member.is_dir() and str(path) == member.filename
        assert member.filename not in files and member.file_size <= 32 * 1024 * 1024
        files[member.filename] = zipped.read(member)
assert 'sha256.txt' in files
manifest = {}
for line in files['sha256.txt'].decode().splitlines():
    checksum, name = line.split(maxsplit=1)
    if name.startswith('./'):
        name = name[2:]
    assert re.fullmatch(r'[0-9a-f]{64}', checksum) and name not in manifest
    manifest[name] = checksum
assert set(files) == set(manifest) | {'sha256.txt'}
assert all(sha(files[name]) == checksum for name, checksum in manifest.items())
source = json.loads(files['source.json'])
assert source['product_sha'] == head and source['product_tree'] == tree and source['control_sha'] == controller
assert source['workflow_sha256'] == workflow_hash and source['setup_sha256'] == setup_hash
assert source['run_id'] == str(run_id) and source['run_attempt'] == '1'
commands = json.loads(files['commands.json'])
expected = [
    'atspi-disabled-state-tests', 'ui-check', 'ui-all-targets-compile',
    'active_mixed_managed_saved_scope_recovers_healthy_panes_only',
    'hidden_mixed_managed_saved_scope_recovers_healthy_panes_only',
    'quarantined_chat_remove_routes_never_contact_or_close_the_inventory_owner',
    'ordinary_closed_retained_chat_history_and_same_process_reopen_dispatch_only_the_explicit_resume',
    'keyboard_saved_draft_copy_preserves_the_first_attachment_locations_click',
    'saved_pane_check_results_survive_transport_recovery_and_follow_the_current_source',
    'quarantined_pane_removal_restores_the_surviving_native_control',
    'notification_snapshot_before_model_reprojects_canonical_unread_without_another_snapshot',
    'model_read_publishes_new_managed_chat_before_initialized_view_projection',
    'verified_retirement_survives_repeated_refresh_without_borrowing_metadata_sequence',
    'verified_retirement_never_borrows_rejected_ambiguous_or_changed_sources',
    'closed_retirement_history_still_requires_exact_proof_and_scope',
    'verified_retirement_keeps_unsaved_work_without_manufacturing_failed_proof',
    'retirement_witness_binds_canonical_metadata_and_issuer_separately',
    'verified_terminal_retirement_preserves_exact_ended_and_archived_sources',
    'migration_session_inventory_budget_spans_windows_and_precedes_json_decode',
    'systemd_scope_preserves_environment_arguments_and_isolates_cgroup',
    'cli-check', 'host-check', 'cli-all-targets-suite', 'host-helper-build',
    'host-all-targets-suite', 'ui-all-targets-suite']
assert [c['name'] for c in commands] == expected
assert all(c['exit_code'] == 0 for c in commands)
by_name = {c['name']: c for c in commands}
ui_lane = ['--locked', '--no-default-features', '--features', 'ui']
host_lane = ['--locked', '--no-default-features', '--features', 'host']
fixed_commands = {
    'atspi-disabled-state-tests': ['cargo', 'test', '--locked', '-p', 'accesskit_atspi_common',
        '--lib', 'node::disabled_state_tests::', '--', '--test-threads=1', '--color=never'],
    'ui-check': ['cargo', 'check', *ui_lane, '--bin', 'cagents-ui'],
    'ui-all-targets-compile': ['cargo', 'test', *ui_lane, '--all-targets', '--no-run', '--message-format=json'],
    'cli-check': ['cargo', 'check', '--locked', '--no-default-features', '--bin', 'cagents'],
    'host-check': ['cargo', 'check', *host_lane, '--bin', 'cagentsd'],
    'host-helper-build': ['cargo', 'build', *host_lane, '--bin', 'cagentsd', '--message-format=json'],
}
for name, argv in fixed_commands.items():
    assert by_name[name]['argv'] == argv, name
selected = json.loads(files['selected-test.json'])
assert 'ui' in selected['features'] and not {'host', 'integrated-browser'} & set(selected['features'])
assert re.fullmatch(r'[0-9a-f]{64}', selected['sha256'])
for index, name in enumerate(expected[3:20], start=3):
    prefix = 'app::shared_views::tests::startup_admission::'
    if index == 10:
        prefix = 'app::notification_ui::tests::'
    elif index == 11:
        prefix = 'app::workspace_model_sync::tests::'
    elif index == 18:
        prefix = 'persistence::tests::'
    elif index == 19:
        prefix = 'terminal::hosted::tests::'
    test = prefix + name
    assert by_name[name]['argv'] == [selected['executable'], '--exact', test, '--nocapture',
                                    '--test-threads=1', '--color=never'], name
    text = files[name + '.log'].decode()
    assert test in text and '1 passed; 0 failed' in text, name
state_text = files['atspi-disabled-state-tests.log'].decode()
for name in ['disabled_action_controls_do_not_advertise_enabled_or_sensitive',
             'read_only_text_state_behavior_is_preserved']:
    assert state_text.splitlines().count('test node::disabled_state_tests::' + name + ' ... ok') == 1
assert 'test result: ok. 2 passed; 0 failed; 0 ignored;' in state_text
for lane, prefix in [('cli', []), ('host', ['--features', 'host']), ('ui', ['--features', 'ui'])]:
    command = next(c for c in commands if c['name'] == lane + '-all-targets-suite')
    assert command['argv'] == ['cargo', 'test', '--locked', '--no-default-features', *prefix,
                               '--all-targets', '--', '--test-threads=1']
    text = files[lane + '-all-targets-suite.log'].decode()
    assert 'test result: ok.' in text and not re.search(r'^test .* \.\.\. FAILED', text, re.MULTILINE)
parents = {
    'cli': ['terminal_close_keeps_confirmed_completion_when_detach_connection_ends',
            'terminal_close_preserves_unconfirmed_outcomes_and_other_detach_errors'],
    'host': ['agent_close_reply_drains_before_ordinary_exit_seals_delivery',
             'recovery_reservation_counts_existing_resident_once_above_lowered_limit',
             'pending_startup_keeps_exact_cleanup_owner_until_retry_proves_teardown',
             'installed_startup_with_uncommitted_attention_adopts_before_cleanup_retry',
             'upgrade_commit_failure_preserves_live_control_authority',
             'upgrade_rejects_stale_inventory_and_pending_reservations_without_shutdown',
             'committed_upgrade_fences_control_and_retains_checkpoint_until_close',
             'a_handoff_no_window_can_still_save_stops_refusing_the_upgrade',
             'restored_shell_links_exact_old_source_and_does_not_replay_consumed_checkpoint']}
for lane, names in parents.items():
    text = files[lane + '-all-targets-suite.log'].decode()
    for name in names:
        assert len(re.findall(r'^test [^ ]*::' + re.escape(name) + r' \.\.\. ok$', text, re.MULTILINE)) == 1, name
helper = json.loads(files['host-helper.json'])
after = json.loads(files['host-helper-after.json'])
assert helper['head'] == head and helper['tree'] == tree
assert after['matches'] is True and after['preceding_error'] is None and after['observed'] == helper['frozen']
assert helper['source']['sha256'] == helper['frozen']['sha256']
assert helper['frozen']['identity']['mode'] == 0o500 and helper['frozen']['identity']['links'] == 1
assert helper['artifact']['features'] == ['host'] and helper['artifact']['target']['name'] == 'cagentsd'
assert json.loads(files['native/cleanup.json'])['errors'] == [] and 'native/cleanup-complete' in files
assert not git('status', '--porcelain=v1') and git('rev-parse', 'HEAD') == head

destination = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'reused-suites'
destination.mkdir(parents=True, exist_ok=False)
(destination / '40921866095-full.zip').write_bytes(archive)
(destination / 'verification.json').write_text(json.dumps({
    'product_sha': head, 'product_tree': tree, 'suite_controller': controller,
    'run_id': run_id, 'job_id': job_id, 'run_attempt': 1, 'archive': pin,
    'source': source, 'commands': commands, 'helper_matches': True,
    'manifest_file_count': len(manifest), 'owning_parents': parents,
    'qualification_scope': 'Same-head suites only; predecessor native failed before peer trigger. '
                           'Successor native16 and independent actual-head review remain required.',
    'run': run, 'job': job, 'artifact': artifact,
}, indent=2) + '\n')
print('Verified all 26 same-head commands and completed isolated suites from run', run_id)
