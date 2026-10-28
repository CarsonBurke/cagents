#!/usr/bin/env python3
"""Hosted-only fixture qualification. GitHub owns the VM and 60-minute deadline.

No policy/credential removal, provider, public daemon, native UI, process census
or local executor is introduced. Existing private test fixtures are unchanged.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

FIXED = '83377f3faff86c979dc95cf2a7b0df65171f59bf'
CONTROL = '71e6aa844c2f580c631ec6d1146a065589ed289a'
TREES = {FIXED: 'a48fb887a9d2b5143f928fd4bb58a05516bf9141',
         CONTROL: '25ff5ac3e09edc4b25b737012493ea66f3c1a1a4'}
ENTRY = 'device_service::no_start_entry_tests::public_device_request_honors_inherited_no_start_without_cli_policy_setup'
REQUESTS = ['user_request_keeps_readiness_and_authenticates_before_one_device_operation',
            'explicit_no_start_user_request_uses_the_selected_socket_without_readiness',
            'remote_present_authority_is_refused_before_socket_resolution_or_startup',
            'scoped_request_keeps_captured_authority_through_selection_and_device_refusal',
            'refused_present_authority_never_dispatches_or_falls_back_to_user',
            'scoped_and_no_start_requests_preserve_selected_socket_errors_without_startup',
            'handshake_refusal_and_proof_mismatch_never_authenticate_dispatch_or_retry',
            'missing_selected_socket_never_starts_or_retries_as_user']
CLI = [ENTRY, *['device_service::request_tests::' + n for n in REQUESTS],
       'device::tests::delayed_bridge_worker_keeps_the_original_no_start_policy_after_cleanup']
READS = ['every_read_can_skip_daemon_start_through_flag_or_environment',
         'stopping_an_absent_daemon_twice_succeeds_and_has_a_stable_shape',
         'remote_stop_is_idempotent_and_no_start_reads_report_remote_absence',
         'a_genuine_ssh_failure_is_not_an_already_stopped_remote_daemon',
         'unresolved_runtime_is_an_error_rather_than_an_already_stopped_daemon',
         'a_closed_listener_with_a_held_inventory_lease_is_not_daemon_absence',
         'no_start_daemon_start_still_requires_a_protocol_handshake']
HOST = ['host::client::device_request_tests::authenticated_agent_cannot_prepare_merged_worktree_removal',
        'host::client::device_request_tests::user_invalid_merged_worktree_removal_keeps_its_inert_response']
EXTRA = {'.github/workflows/script-contracts.yml', 'CLAUDE.md', 'scripts/check-format',
         'scripts/check-format-cache.py', 'scripts/run-development',
         'src/app/chat_ui/prose.rs', 'tests/check_format.py', 'tests/run-development-zig.py'}
LINES = ('        || std::env::var("CAGENTS_NO_START")\n'
         '            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))\n')
SUMMARY = re.compile(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; '
                     r'(\d+) measured; (\d+) filtered out;', re.M)
PANIC = re.compile(r"^thread '([^']+)'(?: \(\d+\))? panicked at ([^\n]+):(\d+):(\d+):\n"
                   r'(.*?)(?=^thread |\Z)', re.M | re.S)


def require(value, message):
    if not value:
        raise RuntimeError(message)


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1024 * 1024), b''):
            h.update(b)
    return h.hexdigest()


def write(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def git(source, *args):
    return subprocess.check_output(['git', '--no-optional-locks', '-C', str(source), *args], text=True).strip()


def point(source, revision):
    require(git(source, 'rev-parse', 'HEAD') == revision
            and git(source, 'rev-parse', 'HEAD^{tree}') == TREES[revision], 'wrong literal source')
    require(git(source, 'rev-parse', '--abbrev-ref', 'HEAD') == 'HEAD'
            and not git(source, 'status', '--porcelain=v1', '--untracked-files=all'), 'source must be clean/detached')
    return {'head': revision, 'tree': TREES[revision]}


def entries(source):
    result = {}
    raw = subprocess.check_output(['git', '-C', str(source), 'ls-tree', '-rz', 'HEAD'])
    for row in raw.split(b'\0'):
        if row:
            metadata, path = row.split(b'\t', 1)
            result[path.decode()] = metadata.decode()
    return result


def invoke(command, source, env, output, stem):
    phase = output / stem
    phase.mkdir(mode=0o700)
    print(json.dumps({'phase': stem, 'command': command}), flush=True)
    write(phase / 'COMMAND.json', {'command': command, 'source_head': git(source, 'rev-parse', 'HEAD'),
          'containment': 'GitHub-hosted VM/native 60-minute job timeout; no process-census helper'})
    with (phase / 'stdout.log').open('wb') as out, (phase / 'stderr.log').open('wb') as err:
        code = subprocess.run(command, cwd=source, env=env, stdout=out, stderr=err, check=False).returncode
    write(phase / 'EXIT.json', {'actual_exit': code})
    return code


def compile_target(source, revision, features, target, name, env, cargo, output, stem):
    before = point(source, revision)
    common = ['--locked', '--no-default-features', *(['--features', 'host'] if features else [])]
    command = [str(cargo), 'test', *common, *target, '--no-run', '--message-format=json']
    require(invoke(command, source, env, output, stem) == 0, 'compilation failure is not a causal negative')
    events = [json.loads(line) for line in (output / stem / 'stdout.log').read_text().splitlines()]
    require([e['success'] for e in events if e.get('reason') == 'build-finished'] == [True], 'missing build completion')
    artifacts = [e for e in events if e.get('reason') == 'compiler-artifact']
    forbidden = ['libcosmic', 'servo', 'webrender', 'mozjs', 'spidermonkey'] + ([] if features else ['ghostty'])
    require(artifacts and not any(term in (e['package_id'] + ' ' + e['target']['name']).lower()
                                 for e in artifacts for term in forbidden), 'implementation graphs mixed')
    selected = [e for e in artifacts if e.get('executable') and e.get('profile', {}).get('test') is True
                and e.get('manifest_path') == str(source / 'Cargo.toml') and e['target']['name'] == name
                and e['target']['kind'] == (['test'] if '--test' in target else ['lib'])]
    require(len(selected) == 1 and selected[0]['features'] == features, 'wrong Cargo test artifact')
    binary = Path(selected[0]['executable']).resolve(strict=True)
    require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])), 'artifact outside isolated target')
    sealed = output / 'artifacts' / stem
    sealed.parent.mkdir(mode=0o700, exist_ok=True)
    shutil.copy2(binary, sealed)
    require(sha(sealed) == sha(binary), 'artifact seal mismatch')
    require(invoke([str(binary), '--list'], source, env, output, stem + '-list') == 0, 'test inventory failed')
    names = re.findall(r'^([^\s]+): test$', (output / (stem + '-list') / 'stdout.log').read_text(), re.M)
    require(names and len(names) == len(set(names)) and point(source, revision) == before, 'bad catalog/source changed')
    record = {'source': before, 'features': features, 'artifact': selected[0], 'binary_sha256': sha(binary),
              'sealed_binary': str(sealed), 'declared_tests': len(names), 'names': names,
              'graph': [{'package': e['package_id'], 'target': e['target']['name'], 'features': e['features']} for e in artifacts]}
    if name == 'cli_read_contracts':
        cli = [e for e in artifacts if e.get('executable') and e.get('manifest_path') == str(source / 'Cargo.toml')
               and e['target']['name'] == 'cagents' and e['target']['kind'] == ['bin']]
        require(len(cli) == 1 and cli[0]['features'] == [], 'missing matched CLI product artifact')
        path = Path(cli[0]['executable']).resolve(strict=True)
        require(path.is_relative_to(Path(env['CARGO_TARGET_DIR'])), 'CLI executable outside target')
        preserved = output / 'artifacts' / (stem + '-cagents')
        shutil.copy2(path, preserved)
        require(sha(preserved) == sha(path), 'CLI product sealing failed')
        record['matched_cli'] = {'path': str(path), 'sha256': sha(path),
                                 'sealed_path': str(preserved), 'cargo_artifact': cli[0]}
    write(output / (stem + '.json'), record)
    return binary, record


def test(source, revision, binary, artifact, selectors, env, output, stem, negative=False):
    point(source, revision)
    require(set(selectors).issubset(artifact['names']) and sha(binary) == artifact['binary_sha256'], 'missing selector/artifact changed')
    if 'matched_cli' in artifact:
        require(sha(Path(artifact['matched_cli']['path'])) == artifact['matched_cli']['sha256'], 'matched CLI changed')
    command = [str(binary), *(['--exact', selectors[0]] if len(selectors) == 1 else []),
               '--test-threads=1', '--color', 'never', '--nocapture']
    code = invoke(command, source, env, output, stem)
    stdout = (output / stem / 'stdout.log').read_text()
    stderr = (output / stem / 'stderr.log').read_text()
    names = re.findall(r'^test ([^\s]+) \.\.\.', stdout, re.M)
    sums = SUMMARY.findall(stdout)
    n = len(selectors)
    expected = ('FAILED', '0', '1', '0', '0', str(artifact['declared_tests'] - 1)) if negative else (
        'ok', str(n), '0', '0', '0', str(artifact['declared_tests'] - n))
    good = (len(names) == n and set(names) == set(selectors) and sums == [expected]
            and re.findall(r'^running (\d+) tests?\s*$', stdout, re.M) == [str(n)]
            and code == (101 if negative else 0))
    if negative:
        panics = PANIC.findall(stderr)
        good = (good and len(panics) == 2 and all(p[0] == ENTRY for p in panics)
                and all(p[1].endswith('src/device_service/no_start_entry_tests.rs') for p in panics)
                and [int(p[2]) for p in panics] == [285, 199]
                and panics[0][4].startswith('user/1:')
                and 'direct entry must honor inherited no-start without CLI setup' in panics[1][4]
                and re.search(r'^\s*left: 1\s*\n\s*right: 0\s*$', panics[1][4], re.M) is not None)
    point(source, revision)
    require(sha(binary) == artifact['binary_sha256'], 'test binary changed')
    if 'matched_cli' in artifact:
        require(sha(Path(artifact['matched_cli']['path'])) == artifact['matched_cli']['sha256'], 'matched CLI changed in fixture')
    result = {'source': artifact['source'], 'selectors': selectors, 'actual_exit': code,
              'summaries': sums, 'names': names, 'qualified': bool(good), 'expected_negative': negative,
              'binary_sha256': artifact['binary_sha256'], 'logs': stem}
    write(output / (stem + '.json'), result)
    require(good, 'unexpected owning test result; preserve logs and fail job')
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ['source', 'control', 'target', 'output']:
        p.add_argument('--' + name, type=Path, required=True)
    args = p.parse_args()
    require(os.environ.get('GITHUB_ACTIONS') == 'true' and not sys.flags.optimize, 'hosted entry only')
    for marker in ['CAGENTS_AGENT_AUTHORITY', 'CAGENTS_AGENT_TOKEN', 'CAGENTS_SESSION_TOKEN',
                   'CAGENTS_TEST_DEVICE_NO_START_ENTRY_ROLE', 'CAGENTS_TEST_DEVICE_NO_START_ENTRY_POLICY',
                   'CAGENTS_TEST_DEVICE_NO_START_ENTRY_ROOT', 'CAGENTS_TEST_USER_FIXTURE_SELECTOR']:
        require(marker not in os.environ, 'genuine User/CI prerequisite: present ' + marker)
    source, control = args.source.resolve(strict=True), args.control.resolve(strict=True)
    output, target = args.output.resolve(), args.target.resolve()
    private = Path(os.environ['RUNNER_TEMP']).resolve(strict=True)
    require(output.is_relative_to(private) and target.is_relative_to(private)
            and not output.exists() and not target.exists(), 'fresh private hosted paths required')
    output.mkdir(mode=0o700)
    results = []
    try:
        point(source, FIXED); point(control, CONTROL)
        a, b = entries(source), entries(control)
        changed = {name for name in set(a) | set(b) if a.get(name) != b.get(name)}
        require(changed == EXTRA | {'src/control.rs'}, 'control includes unexpected changes')
        fixed = (source / 'src/control.rs').read_text()
        require(fixed.count(LINES) == 1 and fixed.replace(LINES, '', 1) == (control / 'src/control.rs').read_text(),
                'control is not the exact two-line inverse')
        require('#[cfg(feature = "ui")]\npub mod app;' in (source / 'src/modules.rs').read_text(), 'UI exclusion changed')
        research = Path(__file__).resolve().parent.parent
        require(git(research, 'rev-parse', 'HEAD') == os.environ['DEVICE_RESEARCH_SHA'], 'research head mismatch')
        tool = Path(os.environ['DEVICE_TOOLCHAIN_BIN'])
        require(tool.resolve(strict=True) == tool, 'physical Rust toolchain required')
        env = os.environ.copy()
        env.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='2', RUSTUP_TOOLCHAIN='1.93.0',
                   RUSTC=str(tool / 'rustc'), RUSTDOC=str(tool / 'rustdoc'), PATH=str(tool) + os.pathsep + env['PATH'])
        versions = {n: subprocess.check_output([str(tool / n), '--version'], env=env, text=True).strip()
                    for n in ['cargo', 'rustc', 'rustdoc']}
        require(all('1.93.0' in v for v in versions.values()), 'wrong Rust version')
        write(output / 'PROVENANCE.json', {'research_head': os.environ['DEVICE_RESEARCH_SHA'],
              'script_sha256': sha(Path(__file__)), 'sources': TREES, 'control_extra_paths': sorted(EXTRA),
              'control_limit': 'Old whole tree; identical selected CLI/HOST code except accessor2lines. UI-only prose and dev files differ.',
              'tool_versions': versions, 'tool_hashes': {n: sha(tool / n) for n in versions},
              'target': str(target), 'natural_User_markers': 'Required absent by presence before fixture setup; never removed.',
              'authority_HOME_noStart': 'Inherited unchanged by runner. Existing private fixture isolation remains synthetic.'})
        cargo = tool / 'cargo'
        binary, artifact = compile_target(control, CONTROL, [], ['--lib'], 'cagents', env, cargo, output, 'control-cli')
        results.append(test(control, CONTROL, binary, artifact, [ENTRY], env, output, 'causal-control', True))
        binary, artifact = compile_target(source, FIXED, [], ['--lib'], 'cagents', env, cargo, output, 'fixed-cli')
        for i, selector in enumerate(CLI):
            results.append(test(source, FIXED, binary, artifact, [selector], env, output, 'cli-' + str(i)))
        require(invoke([str(cargo), 'check', '--locked', '--no-default-features', '--bin', 'cagents'],
                       source, env, output, 'check-cli') == 0, 'CLI boundary check failed')
        binary, artifact = compile_target(source, FIXED, [], ['--test', 'cli_read_contracts'],
                                          'cli_read_contracts', env, cargo, output, 'fixed-cli-reads')
        require(set(artifact['names']) == set(READS), 'CLI read target inventory changed')
        results.append(test(source, FIXED, binary, artifact, READS, env, output, 'cli-read-contracts'))
        require(invoke([str(cargo), 'check', '--locked', '--no-default-features', '--features', 'host', '--bin', 'cagentsd'],
                       source, env, output, 'check-host') == 0, 'HOST boundary check failed')
        binary, artifact = compile_target(source, FIXED, ['host'], ['--lib'], 'cagents', env, cargo, output, 'fixed-host')
        for i, selector in enumerate(HOST):
            results.append(test(source, FIXED, binary, artifact, [selector], env, output, 'host-' + str(i)))
        require(sum(len(r['selectors']) for r in results) == 20 and len(results) == 14, 'incomplete fixture selection')
        point(source, FIXED); point(control, CONTROL)
        write(output / 'FINAL.json', {'qualified': True, 'results': results,
              'fixed_positive_parents': 19, 'control_expected_negative_parents': 1,
              'limits': 'CLI routing/Host admission fixtures only; no real daemon startup, SSH, provider, native UI or merge approval.'})
        return 0
    except Exception as error:
        write(output / 'FINAL.json', {'qualified': False, 'results': results,
              'error': str(error), 'error_type': type(error).__name__})
        raise


if __name__ == '__main__':
    sys.exit(main())
