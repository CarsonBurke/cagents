"""Immutable source case selection and hosted-context preflight only; never Cargo or native execution."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re

CONTROL = Path(__file__).resolve().parents[3]

def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()

def control_path(value):
    relative = Path(value)
    assert not relative.is_absolute() and '..' not in relative.parts, 'Control reference escapes source checkout'
    path = CONTROL / relative
    assert path.resolve(strict=True) == path and not path.is_symlink() and path.is_file(), 'Control source path is not canonical regular source'
    return str(path)


def hosted_context():
    present = sorted(name for name in os.environ if name.startswith('CAGENTS_'))
    assert not present, 'Inherited CAGENTS scope is unsupported; names only, environment unchanged'
    assert os.environ.get('GITHUB_ACTIONS') == 'true' and os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
    assert os.environ.get('RUNNER_OS') == 'Linux' and os.environ.get('RUNNER_ARCH') == 'X64'
    assert os.environ.get('GITHUB_EVENT_NAME') == 'pull_request' and os.environ.get('GITHUB_SERVER_URL') == 'https://github.com'
    assert os.environ.get('GITHUB_REPOSITORY') == 'CarsonBurke/cagents'
    event_path = Path(os.environ['GITHUB_EVENT_PATH'])
    event_bytes = event_path.read_bytes()
    assert len(event_bytes) <= 16 * 1024 * 1024, 'Unsupported event metadata size'
    event = json.loads(event_bytes)
    pr = event['pull_request']
    head = pr['head']
    assert event['number'] == 1921 and head['repo']['full_name'] == 'CarsonBurke/cagents'
    assert head['ref'] == os.environ.get('GITHUB_HEAD_REF') == 'research/pr1853-bd88-qualification-20261028'
    assert re.fullmatch(r'[0-9a-f]{40}', head['sha'])
    assert CONTROL == Path(os.environ['GITHUB_WORKSPACE']) / 'research' and CONTROL.resolve(strict=True) == CONTROL
    head_file = CONTROL / '.git/HEAD'
    assert head_file.is_file() and not head_file.is_symlink() and head_file.read_text().strip() == head['sha'], 'Exact detached control checkout required'
    run, attempt = os.environ.get('GITHUB_RUN_ID', ''), os.environ.get('GITHUB_RUN_ATTEMPT', '')
    assert run.isdecimal() and int(run) > 0 and attempt.isdecimal() and int(attempt) > 0
    workflow_ref, workflow_sha = os.environ.get('GITHUB_WORKFLOW_REF', ''), os.environ.get('GITHUB_WORKFLOW_SHA', '')
    assert workflow_ref.startswith('CarsonBurke/cagents/.github/workflows/research-pr1853-bd88.yml@')
    assert re.fullmatch(r'[0-9a-f]{40}', workflow_sha) and re.fullmatch(r'[0-9a-f]{40}', os.environ.get('GITHUB_SHA', ''))
    case_path = Path(control_path('.github/qualification/pr2015/case.json'))
    case_sha = os.environ.get('PR2015_CASE_SHA256', '')
    assert re.fullmatch(r'[0-9a-f]{64}', case_sha) and digest(case_path) == case_sha, 'Immutable selected research case differs'
    case = json.loads(case_path.read_bytes())
    assert case['case'] in ('installer2015', 'legacy-final-save-1726', 'cli-cache-producer-2015') and case['never_merge'] is True
    if case['case'] == 'cli-cache-producer-2015':
        assert attempt == '1' and event.get('action') == 'synchronize', 'Producer requires original synchronize attempt1'
        assert case['product_head'] == '375c37ff01aab0b226c58f547012af74e93a8643'
        assert case['product_tree'] == 'acb2cc6e049562469643e49a1ee5f7abc179c28b'
    if case['case'] == 'installer2015':
        assert attempt == '1', 'Installer2015 requires one original attempt1; reruns are unsupported and no global C grant is implied'
        assert case['product_head'] == '6dc09b9a1b5d205650e244401da0938b653d16da' and case['product_tree'] == 'aaab67a21378958066103707baf3abe8850577d6'
    return {'repository': 'CarsonBurke/cagents', 'pr': 1921, 'event': 'pull_request',
            'head_ref': head['ref'], 'control_head': head['sha'], 'run_id': run, 'run_attempt': attempt,
            'run_binding': run + ':' + attempt + ':' + head['sha'],
            'workflow_ref': workflow_ref, 'workflow_sha': workflow_sha, 'github_sha': os.environ['GITHUB_SHA'],
            'runner_environment': 'github-hosted', 'runner_os': 'Linux', 'runner_arch': 'X64',
            'inherited_cagents_names': [], 'case_sha256': case_sha,
            'scope': 'Checked original engine/event metadata; actual authenticated hosted provenance still requires original GitHub run/job/log/artifact RAW readback.'}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--require-installer-review', action='store_true')
    parser.add_argument('--require-producer-review', action='store_true')
    args = parser.parse_args()
    record = {'status': 'failed', 'case': None, 'heavy_execution': 'UNRUN', 'owning_passes': 0,
              'never_merge': True, 'global_C': 'External Root coordination required; source selection allocates nothing.'}
    output = Path(os.environ['RUNNER_TEMP']) / 'issue2015-hosted-evidence'
    try:
        context = hosted_context()
        case = json.loads(Path(control_path('.github/qualification/pr2015/case.json')).read_bytes())
        record.update(context=context, case=case['case'], case_sha256=context['case_sha256'])
        producer = case['case'] == 'cli-cache-producer-2015'
        if not producer:
            output.mkdir(mode=0o700, exist_ok=True)
        for name, expected in case['source_sha256'].items():
            assert digest(control_path(name)) == expected, 'Selected source pin changed: ' + name
        assert case['legacy_published_source']['head'] == '7dcf367268c51236ccd0b5e12cdb0bc5c64d273c'
        assert digest(control_path(case['legacy_published_source']['archive_path'])) == case['legacy_published_source']['workflow_sha256']
        if producer:
            assert not args.require_installer_review
            plan = json.loads(Path(control_path('.github/qualification/pr2015/cli-cache-producer-plan.json')).read_bytes())
            receipt_path = Path(control_path(case['independent_review_path']))
            receipt = json.loads(receipt_path.read_bytes())
            assert receipt['verdict'] == 'CONTROLLER_SOURCE_CLEAR'
            assert receipt['head'] == case['product_head'] == plan['product_head']
            assert receipt['tree'] == case['product_tree'] == plan['product_tree']
            assert receipt['runner_sha256'] == case['runner_sha256']
            assert receipt['plan_sha256'] == case['plan_sha256']
            assert plan['counts']['qualifying_outer_parents'] == 0 and plan['planned_phase_count'] == 3
            if args.require_producer_review:
                product = Path(os.environ['GITHUB_WORKSPACE']) / 'product'
                head_file = product / '.git/HEAD'
                assert head_file.is_file() and not head_file.is_symlink() and head_file.read_text().strip() == case['product_head']
                assert all(digest(product / path) == expected for path, expected in plan['source_paths_sha256'].items()), 'Exact current product source changed'
            # No new-producer output/tool/setup/target effect precedes its source/review checks.
            output.mkdir(mode=0o700, exist_ok=True)
            record['independent_review_sha256'] = digest(receipt_path)
            if args.require_producer_review:
                with Path(os.environ['GITHUB_ENV']).open('a') as stream:
                    stream.write('CARGO_TARGET_DIR=' + str(Path(os.environ['RUNNER_TEMP']) / 'held-scope-target') + '\n')
                    stream.write('QUALIFICATION_EVIDENCE=' + str(output) + '\n')
                    stream.write('PR2015_CONTROLLER_REVIEW_SHA256=' + record['independent_review_sha256'] + '\n')
                    stream.write('PR2015_RUN_BINDING=' + context['run_binding'] + '\n')
        else:
            assert not args.require_producer_review
        if args.require_installer_review:
            assert case['case'] == 'installer2015'
            product = Path(os.environ['GITHUB_WORKSPACE']) / 'product'
            head_file = product / '.git/HEAD'
            assert head_file.is_file() and not head_file.is_symlink() and head_file.read_text().strip() == case['product_head']
            manifest = json.loads(Path(control_path('.github/qualification/pr2015/manifest.json')).read_bytes())
            assert all(digest(product / item['path']) == item['sha256'] for item in manifest['source_pins']), 'Immutable product bytes changed'
            receipt_path = Path(control_path(case['independent_review_path']))
            receipt = json.loads(receipt_path.read_bytes())
            assert receipt['verdict'] == 'CONTROLLER_SOURCE_CLEAR'
            assert receipt['head'] == case['product_head'] and receipt['tree'] == case['product_tree']
            assert receipt['runner_sha256'] == case['runner_sha256'] and receipt['plan_sha256'] == case['plan_sha256']
            record['independent_review_sha256'] = digest(receipt_path)
            with Path(os.environ['GITHUB_ENV']).open('a') as stream:
                stream.write('CARGO_TARGET_DIR=' + str(Path(os.environ['RUNNER_TEMP']) / 'held-scope-target') + '\n')
                stream.write('QUALIFICATION_EVIDENCE=' + str(output) + '\n')
                stream.write('PR2015_CONTROLLER_REVIEW_SHA256=' + record['independent_review_sha256'] + '\n')
                stream.write('PR2015_RUN_BINDING=' + context['run_binding'] + '\n')
        record['legacy_final_save_1726'] = 'UNRUN_DESELECTED' if case['case'] == 'installer2015' else 'SELECTED_NOT_EXECUTED_BY_PREFLIGHT'
        record['installer2015'] = 'SELECTED_NOT_EXECUTED_BY_PREFLIGHT' if case['case'] == 'installer2015' else 'UNRUN_DESELECTED'
        record['cli_cache_producer_2015'] = 'SELECTED_NOT_EXECUTED_BY_PREFLIGHT' if producer else 'UNRUN_DESELECTED'
        if producer:
            record['legacy_final_save_1726'] = 'UNRUN_DESELECTED'
        record['status'] = 'source_selected'
        with Path(os.environ['GITHUB_OUTPUT']).open('a') as stream:
            stream.write('case=' + case['case'] + '\n')
    except (AssertionError, KeyError, OSError, ValueError, TypeError) as error:
        record['error'] = {'type': type(error).__name__, 'message': str(error)}
    if not output.exists():
        print(json.dumps({'status': record['status'], 'case': record['case'], 'heavy_execution': 'UNRUN', 'error': record.get('error'), 'receipt_path': None}))
        return 1
    receipt = output / ('producer-source-preflight.json' if args.require_producer_review else 'installer-source-preflight.json' if args.require_installer_review else 'case-selection.json')
    with receipt.open('x') as stream:
        json.dump(record, stream, indent=2)
        stream.write('\n')
    print(json.dumps({'status': record['status'], 'case': record['case'], 'heavy_execution': 'UNRUN',
                      'receipt_path': str(receipt), 'error': record.get('error')}))
    return 0 if record['status'] == 'source_selected' else 1


if __name__ == '__main__':
    raise SystemExit(main())
