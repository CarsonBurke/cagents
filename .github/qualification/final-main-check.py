"""Bounded local receipt checks, not an official-artifact/RAW/UX qualifier.

The original22/100 stay at b3/3e. This file never merges historical logs or
claims native Runtime internals that only the four owning Rust parents inspect.
"""
import hashlib
import json
from pathlib import Path
import re
import stat
import sys

A = '00000000-0000-4000-8000-000000002018'
B = '00000000-0000-4000-8000-000000002019'
BEFORE = {'sha': 'b3e036c71e83c0168b8ff7b3a1f6607caa24cced', 'tree': 'fe98f00eb1d0d90a800c1e5808a0bbc756258ab5'}
PRODUCT = {'sha': '91a8dff516e6fb76fadab432119e67c472fd9489', 'tree': 'aea2db52b5ed089ea94e396e58dec8a4d81edb38'}
LIBRARY_SELECTORS = [
    'app::shared_views::tests::published_card_edits::a_draft_shows_and_starts_on_options_chosen_in_another_window',
    'app::shared_views::tests::published_card_edits::an_adopted_draft_keeps_its_failure_reason_until_its_account_changes',
    'app::shared_views::tests::published_card_edits::a_draft_choosing_a_file_takes_up_options_chosen_meanwhile_in_another_window',
    'app::shared_views::tests::startup_admission::mixed_saved_scope_keeps_an_image_draft_through_peer_options_and_chooser_cancel',
]
REPOSITORY_SELECTORS = [
    'a_hook_that_backgrounds_a_pipe_holder_still_releases_the_mutation_lock',
    'owned_hook_pipe_holder_settles_when_no_hook_handoff_starts',
    'owned_hook_pipe_fixture_preserves_failures_and_settles_after_real_handoff',
]
HEX = re.compile('[0-9a-f]{64}')
OUTSIDE = ['official original artifact/download and whole-terminal provenance', 'sole independent RAW',
           'independent native UX', 'original22/100 applicability and custody', 'execution acceptance and merge decision']


def need(value, reason):
    if not value:
        raise ValueError(reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def member(root, ref):
    need(set(ref) == {'path', 'sha256', 'bytes'}, 'member-shape')
    need(type(ref['bytes']) is int and ref['bytes'] >= 0 and isinstance(ref['sha256'], str)
         and HEX.fullmatch(ref['sha256']), 'member-shape')
    relative = Path(ref['path'])
    need(not relative.is_absolute() and '..' not in relative.parts and relative.parts, 'member-path')
    path = root / relative
    need(root.resolve(strict=True) == root and path.resolve(strict=True) == path, 'member-alias')
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and not path.is_symlink(), 'member-type')
    raw = path.read_bytes()
    need(len(raw) == ref['bytes'] and digest(raw) == ref['sha256'], 'member-bytes')
    return raw


def check_retained(value):
    need(value['arms'] == 22 and value['selected_captures'] == 100, 'retained-counts')
    need(value['original_product'] == BEFORE['sha'] and value['original_controller'] == '3e2c925b4e05c9ad9687f3cc22c02b77807af7cc', 'retained-source')
    need((value['original_run'], value['original_attempt'], value['original_job']) == (41138272161, 1, 124346021374), 'retained-origin')
    need(value['applicability'] == dict(path='/tmp/held-saved-scope-fw16-evidence/final-integration-native-evidence-applicability-b3-to183-v1.json',
         sha256='91e7bf6ca0119a25a8a89aba492bd5c555bdfe182040a4205476f0549ca23632', bytes=33415), 'applicability-ref')
    need(value['qualified_capsule'] == dict(path='/tmp/held-saved-scope-fw16-evidence/b3e3e2c-qualified-frozen-source-evidence-capsule-v1.json',
         sha256='e8e7dd6f3d7ff56631910b0bf7c04ec8dacf58b69114f78cb06599d49cd41c4f', bytes=5670), 'capsule-ref')


def check_rust_records(plan, rows, focused=False):
    need(plan['selectors'] == LIBRARY_SELECTORS and plan['repository_selectors'] == REPOSITORY_SELECTORS, 'selector-partition')
    names = ['ui-check', 'ui-all-targets-compile',
             *[selector.rsplit('::', 1)[-1] for selector in LIBRARY_SELECTORS + REPOSITORY_SELECTORS], 'ui-all-targets-suite']
    need(plan['fresh_rust_records'] == names, 'rust-records')
    need([row['name'] for row in rows] == (names[:9] if focused else names),
         'rust-focused-prefix' if focused else 'rust-records')


def check_rust_source(source, plan_hash, workflow_hash):
    need(source['product'] == PRODUCT and source['rust_records'] == 10
         and source['plan_sha256'] == plan_hash and source['workflow_sha256'] == workflow_hash, 'rust-source-binding')


def check_rust(plan, rows, logs, compiled, selected, repository_selected, product_directory, cargo_target_directory):
    check_rust_records(plan, rows)
    check_rust_records(plan, rows[:-1], focused=True)
    lane = ['--locked', '--no-default-features', '--features', 'ui']
    need(rows[0]['argv'] == ['cargo', 'check', *lane, '--bin', 'cagents-ui'], 'ui-check-argv')
    need(rows[1]['argv'] == ['cargo', 'test', *lane, '--all-targets', '--no-run', '--message-format=json'], 'compile-argv')
    need(rows[-1]['argv'] == ['cargo', 'test', *lane, '--all-targets', '--', '--test-threads=1', '--color=never'], 'broad-argv')
    need(all(row['exit_code'] == 0 and row['product'] == PRODUCT for row in rows), 'rust-exit-source')
    # These ephemeral CI paths are observed/source-bound, not reopened here.
    # Actual archive roots and member bytes are checked separately by inspect.
    def canonical(value):
        need(isinstance(value, str) and '\x00' not in value, 'selected-path')
        path = Path(value)
        need(path.is_absolute() and '..' not in path.parts and str(path) == value, 'selected-path')
        return path
    product, target = canonical(product_directory), canonical(cargo_target_directory)

    def select(name, kind, relative_source, receipt, prefix):
        artifacts = [row for row in compiled if row.get('reason') == 'compiler-artifact'
                     and row.get('target', {}).get('name') == name
                     and row.get('target', {}).get('kind') == [kind]
                     and row.get('target', {}).get('src_path') == str(product / relative_source)
                     and row.get('profile', {}).get('test') is True and row.get('executable')]
        need(len(artifacts) == 1 and receipt['artifact'] == artifacts[0], prefix + '-artifact')
        artifact = artifacts[0]
        need(isinstance(artifact.get('package_id'), str) and artifact['package_id']
             and artifact.get('manifest_path', str(product / 'Cargo.toml')) == str(product / 'Cargo.toml'), prefix + '-package')
        need('ui' in artifact['features'] and not {'host', 'full', 'integrated-browser'} & set(artifact['features']), prefix + '-features')
        binary = canonical(artifact['executable'])
        need(binary.is_relative_to(target) and binary != target
             and receipt['executable'] == str(binary)
             and isinstance(receipt['observed_binary_sha256'], str)
             and HEX.fullmatch(receipt['observed_binary_sha256']), prefix + '-identity')
        # Identical Cargo filenames across origins are legitimate. Binary bytes
        # are CI-observed, not retained/rehashed here; official custody is external.
        need(receipt['compile_stdout_sha256'] == rows[1]['stdout']['sha256'], prefix + '-compile-ref')
        return artifact

    library = select('cagents', 'lib', 'src/lib.rs', selected, 'selected')
    repository = select('repository_git_contract', 'test', 'tests/repository_git_contract.rs', repository_selected, 'selected-repository')
    need(repository['package_id'] == library['package_id'], 'selected-repository-package')
    for focused_rows, selectors, receipt in [(rows[2:6], plan['selectors'], selected),
                                             (rows[6:9], plan['repository_selectors'], repository_selected)]:
        for row, selector in zip(focused_rows, selectors, strict=True):
            need(row['argv'] == [receipt['executable'], selector, '--exact', '--test-threads=1', '--nocapture', '--color=never'], 'focused-argv')
            text = logs[row['name']]
            need(text.splitlines().count('test ' + selector + ' ... ok') == 1, 'focused-owning-line')
            summaries = re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', text, re.M)
            need(len(summaries) == 1 and summaries[0][:5] == ('ok', '1', '0', '0', '0'), 'focused-summary')
            # Filtered counts are allowed for exact selectors. Caught inner
            # panics may appear in stderr; only the real parent counts here.
    broad = logs['ui-all-targets-suite']
    summaries = re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', broad, re.M)
    need(summaries and all(row[0] == 'ok' and row[2] == '0' and row[5] == '0' for row in summaries)
         and any(int(row[1]) > 0 for row in summaries), 'broad-summary')
    for selector in plan['selectors'] + plan['repository_selectors']:
        need(broad.splitlines().count('test ' + selector + ' ... ok') == 1, 'broad-owning-line')


def check_sources(source, events):
    initial, chosen, rejected = [source[key] for key in ('initial', 'chosen', 'rejected')]
    need(not any(row['event'].startswith('UNEXPECTED') for row in events), 'actor-or-unknown-request')
    need(initial['id'] == chosen['id'] != rejected['id'] and initial['proof'] is None and chosen['proof'] is None, 'draft-identity')
    need(initial['native_continuity'] == chosen['native_continuity'] == 'ReconnectOnly'
         and rejected['launch']['session_id'] == rejected['id'], 'draft-continuity')
    for value in (initial, chosen):
        need(value['launch']['session_id'] is None and value['terminal'] is None and value['owned_codex_listener'] is None, 'unstarted-source')
    need({key: value for key, value in initial.items() if key != 'launch'} == {key: value for key, value in chosen.items() if key != 'launch'}, 'source-drift')
    need(initial['launch']['provider'] == chosen['launch']['provider'] == rejected['launch']['provider'], 'provider-drift')
    old, new = initial['launch']['recipe'], chosen['launch']['recipe']
    root = Path(old['working_directory']).parent
    need(old['program'] == new['program'] == 'synthetic-offline-claude-never-execute'
         and old['arguments'] == new['arguments'] == ['--permission-mode', 'default'], 'constant-mode-model-effort')
    need(old['environment'] == {} and old['unset_environment'] == [], 'initial-recipe')
    need(new['environment'] == {'CLAUDE_CONFIG_DIR': str(root / 'peer-home'), 'SYNTHETIC_RECIPE_DIMENSION': 'peer-B'}
         and new['unset_environment'] == ['SYNTHETIC_UNUSED_VALUE'] and new['working_directory'] == str(root / 'project/peer-directory'), 'chosen-full-recipe')
    model = source['full_model']
    need(len(model['agents']) == 1 and len(model['views'][0]['panes']) == 2, 'raw-full-scope')
    panes = {pane['id']: pane for pane in model['views'][0]['panes']}
    need(set(panes) == {A, B} and panes[A]['kind'] == dict(kind='chat', **initial)
         and panes[B]['kind'] == dict(kind='chat', **rejected), 'raw-pane-source')
    agent = model['agents'][0]
    need(agent['id'] == B and not agent['closed'] and agent['placement'] == model['views'][0]['scope'], 'raw-issuer-placement')
    issued = agent['session']['Chat'] if 'Chat' in agent['session'] else agent['session']['chat']
    need({key: value for key, value in issued.items() if key != 'proof'} == {key: value for key, value in rejected.items() if key != 'proof'}, 'issuer-source')
    proof = rejected['proof']
    need(proof == source['connection_proof'] and all(proof.get(key) for key in ('host_session_id', 'boot_id', 'host_incarnation'))
         and issued['proof']['host_session_id'] == proof['host_session_id'] and issued['proof']['boot_id'] == proof['boot_id']
         and issued['proof']['host_incarnation'] == 'synthetic-wrong-issuer' != proof['host_incarnation'], 'wrong-issuer-proof')
    seeds = [row for row in events if row['event'] == 'seed-request' and row['scope'] == model['views'][0]['scope']]
    need(len(seeds) == 1 and seeds[0]['panes'] == [panes[A]] and seeds[0]['closed'] == [], 'seed-A-only')
    acks = [row for row in events if row['event'] == 'seed-ack' and row['scope'] == seeds[0]['scope']]
    need(len(acks) == 1 and acks[0]['request_id'] == seeds[0]['request_id'] and acks[0]['unix_ns'] >= seeds[0]['unix_ns'], 'real-seed-ack')
    full_models = ([row['full_model'] for row in acks]
                   + [row['full_model'] for row in events if row['event'] == 'peer-source-witness']
                   + [row['model'] for row in events if row['event'] == 'full-model'])
    for full in full_models:
        need(full['identity'] == model['identity'] and full['agents'] == model['agents'], 'full-issuer-conservation')
        need(full.get('projects') == model.get('projects') and full.get('channels') == model.get('channels')
             and [view['scope'] for view in full['views']] == [view['scope'] for view in model['views']]
             and [view['retired'] for view in full['views']] == [view['retired'] for view in model['views']], 'full-scope-conservation')
        need([pane['id'] for pane in full['views'][0]['panes']] == [A, B]
             and full['views'][0]['panes'][0]['kind'] in (dict(kind='chat', **initial), dict(kind='chat', **chosen)), 'full-A-conservation')
        need(next(pane for pane in full['views'][0]['panes'] if pane['id'] == B) == panes[B], 'full-B-conservation')
    peer_witnesses = [row for row in events if row['event'] == 'peer-source-witness']
    need(len(peer_witnesses) == 1 and peer_witnesses[0]['full_model']['sequence'] > acks[0]['full_model']['sequence']
         and next(pane for pane in peer_witnesses[0]['full_model']['views'][0]['panes'] if pane['id'] == A)['kind'] == dict(kind='chat', **chosen), 'peer-full-A-source')
    return root


def check_storage(value, source, expected_a, image, text):
    cards, metadata = value['cards'], value['metadata']
    need(set(cards) == {A, B} and cards[A]['source'] == expected_a and cards[B]['source'] == source['rejected'], 'stored-source')
    need(cards[A]['existence'] == cards[B]['existence'] == 'open', 'stored-open')
    need(metadata['quarantined_panes'] == {B: dict(kind='chat', **source['rejected'])}, 'stored-quarantine')
    need(metadata['chat_drafts'].get(B) == 'Rejected sibling keeps its saved café draft.\nNever submit or start it.', 'stored-B-draft')
    need(metadata['chat_drafts'].get(A) == text and metadata['chat_draft_attachments'].get(A) == [image], 'stored-A-payload')
    need(metadata['local_only_cards'] == [], 'no-local-bypass')
    owners = metadata['threads']
    need(len(owners) == 1 and owners[0]['cards'] == [A, B] and owners[0]['focused'] == A
         and metadata['active_thread'] == owners[0]['id'], 'stored-scope-focus')


def check_flow(label, receipt, source, fixture_events, native_events, payload, final, png):
    need(receipt['revision'] == (BEFORE if label == 'before' else PRODUCT), 'arm-source')
    root = check_sources(source, fixture_events)
    need(type(receipt['ui_pid']) is int and receipt['ui_pid'] > 0 and type(receipt['window']) is int and receipt['window'] > 0, 'ui-target')
    need(HEX.fullmatch(receipt['ui_sha256']) and set(receipt['matched_triplet']) == {'cagents', 'cagentsd', 'cagents-ui'}, 'matched-triplet')
    need(receipt['matched_triplet']['cagents-ui']['sha256'] == receipt['ui_sha256'], 'ui-build-identity')
    for row in receipt['matched_triplet'].values():
        need(HEX.fullmatch(row['sha256']) and Path(row['path']).is_absolute(), 'matched-build')
    opened, cancelled = receipt['portal_open'], receipt['portal_cancel']
    call, reply = opened['call'], opened['returned']
    need(opened['ui_pid'] == receipt['ui_pid'] and opened['frontend_pid'] == receipt['portal_frontend']['pid'], 'portal-caller')
    need(call['body'][0] == '' and call['body'][1] == 'Attach files to chat' and call['body'][2]['modal'] is True
         and call['body'][2]['multiple'] is True, 'actual-open-file')
    token = call['body'][2]['handle_token']
    expected_path = '/org/freedesktop/portal/desktop/request/' + call['sender'][1:].replace('.', '_') + '/' + token
    need(opened['request_path'] == expected_path and reply['body'] == [expected_path]
         and reply['reply_serial'] == call['serial'] and reply['destination'] == call['sender']
         and reply['sender'] == opened['frontend_owner'] == receipt['portal_frontend']['owner'], 'portal-request-binding')
    need(cancelled['path'] == expected_path and cancelled['sender'] == opened['frontend_owner'] and cancelled['body'][0] == 1, 'actual-cancel-response')
    descriptor = receipt['descriptor']
    need(descriptor['sha256'] == descriptor['original_sha256'] and HEX.fullmatch(descriptor['sha256'])
         and descriptor['original'] == '/usr/share/xdg-desktop-portal/portals/gtk.portal', 'official-descriptor')
    pending = [row for row in native_events if row['event'] == 'chooser-pending']
    cancel_actions = [row for row in native_events if row['event'] == 'native-click' and row['target'] == 'Cancel']
    peers = [row for row in fixture_events if row['event'] == 'peer-delivered']
    continuations = [row for row in native_events if row['event'] == 'postcancel-continuation']
    need(len(pending) == len(cancel_actions) == len(peers) == len(continuations) == 1, 'one-real-cancel')
    need(pending[0]['portal'] == opened and pending[0]['gtk_pid'] == cancel_actions[0]['pid'] == receipt['portal_backend']['pid']
         and pending[0]['window'] > 0 and pending[0]['chooser_directory'] == str(root / 'chooser-data'), 'gtk-dialog-owner')
    need(call['unix_ns'] < pending[0]['unix_ns'] < peers[0]['unix_ns'] < cancel_actions[0]['unix_ns'] < cancelled['unix_ns'] < continuations[0]['unix_ns'], 'callback-order')
    need(next(row['unix_ns'] for row in fixture_events if row['event'] == 'seed-ack'
              and row['scope'] == source['full_model']['views'][0]['scope']) < call['unix_ns'], 'seed-before-chooser')
    between = [row for row in native_events if row['event'] == 'command' and cancel_actions[0]['unix_ns'] < row['started_unix_ns'] < continuations[0]['unix_ns']]
    # Only the already declared Cancel pointer gesture, observations and exactly
    # one native append are allowed. No post-Cancel focus/activation repair.
    append = [row for row in between if row['argv'][:2] == ['xdotool', 'type']]
    need(len(append) == 1 and append[0]['argv'] == ['xdotool','type','--clearmodifiers','--delay','1',' Keyboard focus stayed here.'], 'continuation-input')
    need(cancelled['unix_ns'] < append[0]['started_unix_ns'] < append[0]['unix_ns'] < continuations[0]['unix_ns'], 'continuation-after-response')
    x, y, width, height = cancel_actions[0]['bounds']
    need(width > 0 and height > 0, 'cancel-native-bounds')
    gesture = ['xdotool','mousemove','--sync',str(x + width // 2),str(y + height // 2),'click','1']
    clicks = [row for row in between if row['argv'][:2] == ['xdotool','mousemove']]
    need(len(clicks) == 1 and clicks[0]['argv'] == gesture and clicks[0]['started_unix_ns'] < cancelled['unix_ns']
         and clicks[0]['started_unix_ns'] < clicks[0]['unix_ns'] < continuations[0]['unix_ns'], 'actual-cancel-gesture')
    for row in between:
        argv = row['argv']
        need(row['exit_code'] == 0, 'native-command-exit')
        need(argv[0] == 'xdotool' and argv[1] in ('getwindowfocus','getwindowpid','type','mousemove')
             and 'windowactivate' not in argv and 'windowfocus' not in argv, 'no-focus-repair')
        if argv[1] == 'getwindowfocus':
            need(argv == ['xdotool','getwindowfocus'], 'exact-focus-observation')
        if argv[1] == 'getwindowpid':
            need(argv == ['xdotool','getwindowpid',str(receipt['window'])], 'exact-focus-observation')
    first, continued = receipt['restored_focus'], receipt['continued_input']
    need(first['pid'] == continued['pid'] == receipt['ui_pid'] and first['window'] == continued['window'] == receipt['window'], 'same-focused-owner')
    need(continued['text'] == first['text'] + ' Keyboard focus stayed here.' == receipt['final_text'], 'continued-text')
    need(first['start_stamp'] == continued['start_stamp'] == receipt['ui_start_stamp'], 'ui-process-generation')
    need(first['caret'] == len(first['text']) and continued['caret'] == len(continued['text'])
         and first['selections'] == continued['selections'] == 0, 'editor-cursor-custody')
    need(continuations[0]['before'] == first and continuations[0]['after'] == continued, 'focus-observation')
    image = receipt['actual_image']
    need(image['reference']['number'] == 1 and image['reference']['name'] == 'Pasted image'
         and image['reference']['extension'] == 'png' and image['reference']['sha256'] == digest(png), 'image-reference')
    need(Path(image['path']).is_relative_to(root / 'data') and receipt['asset']['path'] == image['path']
         and receipt['asset']['sha256'] == digest(png) and receipt['asset']['bytes'] == len(png)
         and receipt['asset']['links'] == 1 and png.startswith(b'\x89PNG\r\n\x1a\n'), 'asset-custody')
    check_storage(payload, source, source['initial'], image, first['text'])
    check_storage(final, source, source['chosen'], image, continued['text'])
    need(payload['metadata']['chat_draft_attachments'][B] == final['metadata']['chat_draft_attachments'][B], 'B-image-custody')
    need([entry['name'] for entry in receipt['catalog_observed']] == ['Fixture model A', 'Fixture model B'], 'known-catalog')
    need('Claude · Default model' in receipt['postcancel_footer'] and 'Ask before edits' in receipt['postcancel_footer'], 'constant-footer')
    if label == 'after':
        need('Synthetic peer account' in receipt['postcancel_footer'] and 'Synthetic initial account' not in receipt['postcancel_footer'], 'after-footer')
        need(any(', ' + str(root / 'project/peer-directory') + ', focused' in header for header in receipt['postcancel_headers']), 'runtime-cwd-footer')
    # Before's actual footer may be initial or peer. Never require a failure.
    need(receipt['inspector_before']['present'] is True and receipt['inspector_after']['present'] is False, 'ordinary-inspector-setup')


def inspect(root, control):
    need(root.resolve(strict=True) == root and control.resolve(strict=True) == control, 'canonical-roots')
    members = {}
    for path in sorted(root.rglob('*')):
        need(not path.is_symlink() and path.resolve(strict=True) == path, 'artifact-alias')
        info = path.lstat()
        if stat.S_ISDIR(info.st_mode):
            continue
        need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'artifact-member-type')
        raw = path.read_bytes()
        members[str(path.relative_to(root))] = dict(sha256=digest(raw), bytes=len(raw))
    plan = json.loads((control / '.github/qualification/final-main-plan.json').read_text())
    need(plan['product'] == PRODUCT and plan['before'] == BEFORE, 'plan-source')
    check_retained(plan['retained_native_scope'])
    need(all(value is None for value in plan['future_actual'].values()), 'no-future-facts-in-plan')
    rows = json.loads((root / 'commands.json').read_text())
    logs = {}
    for row in rows:
        logs[row['name']] = member(root, row['stdout']).decode()
        member(root, row['stderr'])
    compiled = [json.loads(line) for line in logs['ui-all-targets-compile'].splitlines() if line.startswith('{')]
    selected = json.loads((root / 'selected-test.json').read_text())
    repository_selected = json.loads((root / 'selected-repository-test.json').read_text())
    source = json.loads((root / 'source.json').read_text())
    check_rust_source(source, digest((control / '.github/qualification/final-main-plan.json').read_bytes()),
                      digest((control / '.github/workflows/qualify-final-main-integration.yml').read_bytes()))
    check_rust(plan, rows, logs, compiled, selected, repository_selected,
               source['product_directory'], source['cargo_target_directory'])
    native_source = json.loads((root / 'native/source.json').read_text())
    need(native_source['product'] == PRODUCT and native_source['before'] == BEFORE
         and all(native_source[key] == source[key] for key in ('control_sha','control_tree','workflow_sha256','run','attempt')), 'native-source-binding')
    need(native_source['driver_sha256'] == digest((control / '.github/qualification/final-main-native.py').read_bytes())
         and native_source['fixture_sha256'] == digest((control / '.github/qualification/final-main-fixture.rs').read_bytes()), 'native-helper-source')
    native_events = [json.loads(line) for line in (root / 'native/events.jsonl').read_text().splitlines()]
    for label in ('before', 'after'):
        receipt = json.loads((root / ('native/' + label + '-source.json')).read_text())
        fixtures = [json.loads(line) for line in (root / ('native/' + label + '-fixture.jsonl')).read_text().splitlines()]
        sources = [row for row in fixtures if row['event'] == 'source']
        need(len(sources) == 1, 'one-source-witness')
        check_flow(label, receipt, sources[0], fixtures, [row for row in native_events if row['arm'] == label],
                   json.loads((root / ('native/' + label + '-payload-storage.json')).read_text()),
                   json.loads((root / ('native/' + label + '-storage.json')).read_text()),
                   (root / ('native/' + label + '-managed-image.png')).read_bytes())
        typed = json.loads((root / ('native/' + label + '-typed-inspection.json')).read_text())
        need(typed['event'] == 'typed-inspection' and typed['actual_image'] == receipt['actual_image']
             and typed['unavailable_sibling_asset_valid'] is False, 'typed-reopen')
        for capture in ('draft-image-wide', 'chooser-peer', 'postcancel-wide', 'postcancel-narrow'):
            need((root / ('native/' + label + '-' + capture + '.webp')).is_file(), 'capture-missing')
    cleanup = json.loads((root / 'native/cleanup.json').read_text())
    need(cleanup == {'errors': [], 'owned_descendants_remaining': []} and (root / 'native/cleanup-complete').is_file(), 'cleanup')
    for label in ('before', 'after'):
        cleanup = json.loads((root / ('native/' + label + '-cleanup.json')).read_text())
        need(cleanup['errors'] == [] and cleanup['documents_graceful']['exit_code'] == 0
             and cleanup['documents_graceful']['mounted_after'] is False
             and cleanup['owned_descendants_remaining'] == [] and all(owner is None for owner in cleanup['private_bus_names'].values()), 'arm-graceful-unmount')
    result = dict(kind='final-main-integration-local-contracts-v1', local_contracts_ok=True,
        fresh_rust_records=10, new_native_revisions=2, new_native_flows=1,
        original22_100_provenance=plan['retained_native_scope'], qualified=False,
        retained_evidence_verified_here=False, outside_local_checker=OUTSIDE, observed_members=members,
        limits=['Exact Runtime Target/editor/cursor/allocation and opaque full launch dimensions are asserted by the four owning Rust parents; native observes exact source, account/cwd footer, image/text, real portal and continuation input.',
                'CI-observed binary digests are source-bound receipts, not recomputed from unavailable retained executable bytes.'])
    return result


if __name__ == '__main__':
    need(len(sys.argv) == 3, 'Pass actual evidence root and frozen control source')
    root, control = [Path(value).absolute() for value in sys.argv[1:]]
    output = root / 'integration-local-check.json'
    need(not output.exists(), 'exclusive-output')
    output.write_text(json.dumps(inspect(root, control), indent=2) + '\n')
