"""Prepared PR1239 exact-head isolated HOST check plus nine ordinary HOST parents. No execution without fresh cagents grant and independent controller review."""
import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone

WT = Path('/home/marvin/Documents/repositories/cagents/.worktrees/fw16-pr1239-inventory-varint-fixture')
TARGET = Path('/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-target')
LOCK = TARGET.parent / 'lane-c.lock'
TOOL = Path('/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin')
PLAN = Path('/home/marvin/Documents/repositories/cagents/.worktrees/coord-b9-1239-7348-host9-control/qualification-packets/pr1239-7348/plan.json')
PROCESS_ATTEMPTS = []  # Append-only immutable successful-spawn tuples; never inferred from evidence files.
PROCESS_STATES = []    # Separate original-wait and evidence observations; null stays unknown.
SUPERVISION_FAILURES = []  # Sticky history, including failures after an otherwise successful wait.
CANCELLATION_SIGNALS = []  # Cooperative cancellation, never cleared by cleanup.
PLAN_SHA = '88b0b79fb975089b08d85d2b30a4a6bad1df5d7d5f2f3d9c28db188618695c6d'
SCOPE_PARENT = None  # No selected PR1239 contract enables the dormant donor staged-scope path.


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def now():
    return datetime.now(timezone.utc).isoformat()


def observed_wait(state, child, timeout):
    code = child.wait(timeout=timeout)
    state['exit'] = code
    state['original_wait_complete'] = code is not None
    return code


def supervision_failure(state, error, stage):
    detail = type(error).__name__ + ': ' + str(error)
    SUPERVISION_FAILURES.append({'stage': stage, 'pid': state['pid'] if state else None,
                                 'label': state['label'] if state else None, 'error': detail})
    if state is not None:
        state['supervision_error'] = ((state['supervision_error'] + '; ') if state['supervision_error'] else '') + detail
    return detail


def bounded_capture(argv, absolute_deadline=None):
    require_workload_allowed()
    # Source checks use the same sticky spawn/wait supervision as owning phases.
    local_deadline = time.monotonic() + 10
    if absolute_deadline is not None:
        local_deadline = min(local_deadline, absolute_deadline)
    code, expired, process_error, child_fd, state = None, False, None, None, None
    launch_requested = False
    try:
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            remaining = local_deadline - time.monotonic()
            assert remaining > 0, 'Source-check budget exhausted before launch'
            require_workload_allowed()
            launch_requested = True
            child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, close_fds=True, start_new_session=True)
            PROCESS_ATTEMPTS.append(('source', None, tuple(argv), child.pid))
            state = {'stage': 'source', 'label': None, 'pid': child.pid, 'exit': None,
                     'original_wait_complete': False, 'direct_cleanup_attempted': False,
                     'supervision_error': None, 'evidence_complete': False, 'custody': None}
            PROCESS_STATES.append(state)
            try:
                child_fd = os.pidfd_open(child.pid)
                try:
                    remaining = local_deadline - time.monotonic()
                    if remaining <= 0:
                        raise subprocess.TimeoutExpired(argv, 0)
                    code = observed_wait(state, child, remaining)
                except subprocess.TimeoutExpired:
                    expired = True
                    state['direct_cleanup_attempted'] = True
                    signal_pidfd(child_fd, signal.SIGTERM)
                    try:
                        code = observed_wait(state, child, 3)
                    except subprocess.TimeoutExpired:
                        signal_pidfd(child_fd, signal.SIGKILL)
                        try:
                            code = observed_wait(state, child, 3)
                        except subprocess.TimeoutExpired:
                            pass
            finally:
                if child_fd is not None:
                    os.close(child_fd)
            assert code == 0 and not expired, ('Read-only source Git failed/uncertain', code, expired)
            stdout.seek(0)
            stderr.seek(0)
            output, errors = stdout.read(1024 * 1024 + 1), stderr.read(1024 * 1024 + 1)
            assert len(output) <= 1024 * 1024 and len(errors) <= 1024 * 1024, 'Source-check output exceeded bound'
            result = output.decode().strip()
        state['evidence_complete'] = True
        return result
    except Exception as exc:
        if launch_requested:
            supervision_failure(state, exc, 'source')
        raise


GIT_ROUTING = (
    'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_INDEX_FILE',
    'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES',
    'GIT_CONFIG', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_COUNT', 'GIT_ATTR_SOURCE',
)


def refuse_git_routing():
    # Match the maintained checkout helper. Never log, remove or replace values.
    assert not any(name in os.environ for name in GIT_ROUTING), 'Unsupported Git-routing variable names are present'


def git(*args, absolute_deadline=None):
    refuse_git_routing()
    return bounded_capture([
        'git', '--no-replace-objects', '--no-optional-locks',
        '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false',
        '-c', 'core.untrackedCache=false', '-c', 'submodule.recurse=false',
        '-C', str(WT), *args,
    ], absolute_deadline)


def git_nul_fields(value):
    assert not value or value.endswith('\0'), 'Unterminated Git metadata'
    return value[:-1].split('\0') if value else []


def guard_git_metadata(absolute_deadline=None):
    """Refuse executable metadata contexts before the first status invocation.

    These read-only guards mirror prepare-validation-checkout's configuration,
    index and effective-attribute policy. The original supervised Git capture
    remains responsible for every spawned process and its original wait.
    """
    refuse_git_routing()
    keys = git_nul_fields(git('config', '--null', '--name-only', '--list', absolute_deadline=absolute_deadline))
    assert all(key for key in keys), 'Empty Git configuration key'
    assert not any(re.match(r'^(includeif\.|remote\..*\.promisor$|extensions\.partialclone$)', key.lower())
                   for key in keys), 'Conditional Git configuration and partial/promisor clones are unsupported'
    for setting in ('core.sparsecheckout', 'index.sparse'):
        if setting in {key.lower() for key in keys}:
            assert git('config', '--bool', '--get', setting, absolute_deadline=absolute_deadline) == 'false', 'Sparse Git source is unsupported'
    indexed = git_nul_fields(git('ls-files', '-v', '-z', absolute_deadline=absolute_deadline))
    assert indexed and all(entry.startswith('H ') and entry[2:] for entry in indexed), 'Index flags or unresolved entries conceal source'
    paths = [entry[2:] for entry in indexed]
    assert len(paths) == len(set(paths)), 'Duplicate index paths'
    staged = {}
    for entry in git_nul_fields(git('ls-files', '--stage', '-z', absolute_deadline=absolute_deadline)):
        header, separator, path = entry.partition('\t')
        fields = header.split(' ')
        assert (separator and path and len(fields) == 3 and fields[0] in ('100644', '100755', '120000')
                and re.fullmatch(r'[0-9a-f]{40}', fields[1]) and fields[2] == '0' and path not in staged), 'Invalid or submodule index entry'
        staged[path] = (fields[0], fields[1])
    assert set(staged) == set(paths), 'Index path inventories disagree'
    committed = {}
    for entry in git_nul_fields(git('ls-tree', '-r', '-z', 'HEAD', absolute_deadline=absolute_deadline)):
        header, separator, path = entry.partition('\t')
        fields = header.split(' ')
        assert (separator and path and len(fields) == 3 and fields[0] in ('100644', '100755', '120000')
                and fields[1] == 'blob' and re.fullmatch(r'[0-9a-f]{40}', fields[2]) and path not in committed), 'Invalid or submodule committed entry'
        committed[path] = (fields[0], fields[2])
    assert staged == committed, 'Staged source differs from the committed tree'
    # Bounded argv batches avoid stdin pipes and preserve original wait/custody.
    batches, batch, size = [], [], 0
    for path in paths:
        cost = len(os.fsencode(path)) + 1
        assert cost <= 32768, 'Tracked path exceeds the metadata argument bound'
        if batch and size + cost > 32768:
            batches.append(batch)
            batch, size = [], 0
        batch.append(path)
        size += cost
    if batch:
        batches.append(batch)
    for batch in batches:
        for source in ((), ('--cached',)):
            values = git_nul_fields(git('check-attr', '-z', '--all', *source, '--', *batch,
                                        absolute_deadline=absolute_deadline))
            assert len(values) % 3 == 0, 'Incomplete effective Git attribute metadata'
            seen = set()
            for offset in range(0, len(values), 3):
                path, attribute, _value = values[offset:offset + 3]
                assert path in batch and attribute and (path, attribute) not in seen, 'Invalid or duplicate effective Git attribute'
                seen.add((path, attribute))
                assert attribute != 'filter', 'Effective Git filter attributes are unsupported'


def signal_pidfd(fd, number):
    try:
        signal.pidfd_send_signal(fd, number)
    except OSError as error:
        if error.errno != errno.ESRCH:
            raise


def children():
    result = []
    for pid in Path(f'/proc/self/task/{os.getpid()}/children').read_text().split():
        try:
            fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
            if int(fields[1]) == os.getpid():
                result.append({'pid': int(pid), 'start': int(fields[19]), 'state': fields[0]})
        except (FileNotFoundError, ProcessLookupError):
            pass
    return result


def settle_children():
    forced = False
    observed = []
    deadline = time.monotonic() + 3
    while True:
        while time.monotonic() < deadline:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
                if pid == 0:
                    break
            except ChildProcessError:
                break
        remaining = children()
        if not remaining:
            return {'clean': True, 'forced': forced, 'observed': observed}
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    observed = remaining
    deadline = time.monotonic() + 10
    while remaining and time.monotonic() < deadline:
        forced = True
        for child in remaining:
            if time.monotonic() >= deadline:
                break
            try:
                fd = os.pidfd_open(child['pid'])
                try:
                    if any(c['pid'] == child['pid'] and c['start'] == child['start'] for c in children()):
                        signal_pidfd(fd, signal.SIGKILL)
                finally:
                    os.close(fd)
            except OSError as error:
                if error.errno != errno.ESRCH:
                    raise
        time.sleep(0.05)
        while time.monotonic() < deadline:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
                if pid == 0:
                    break
            except ChildProcessError:
                break
        remaining = children()
    return {'clean': not remaining, 'forced': forced, 'observed': observed, 'remaining': remaining}


def cargo_test_artifact(text, manifest_path, expected_features):
    artifacts, finished = [], []
    for line in text.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'build-finished':
            finished.append(item.get('success'))
        if (item.get('reason') == 'compiler-artifact' and item.get('profile', {}).get('test')
                and item.get('target', {}).get('kind') == ['lib']
                and item.get('target', {}).get('name') == 'cagents' and item.get('executable')
                and Path(item.get('manifest_path', '')).resolve() == Path(manifest_path).resolve()):
            artifacts.append(item)
    assert finished == [True], 'Cargo did not report exactly one successful build-finished'
    assert len(artifacts) == 1, 'Exact root cagents library test artifact missing/ambiguous'
    artifact = artifacts[0]
    assert artifact.get('features') == expected_features, 'Wrong or unified feature graph'
    assert set(expected_features) in (set(), {'host'}, {'ui'}), 'Only isolated CLI/HOST/UI graphs permitted'
    return artifact


def inventory_names(text, selected):
    actual, benchmarks, totals = [], [], []
    for line in text.splitlines():
        if not line:
            continue
        named = re.fullmatch(r'(\S+): (test|benchmark)', line)
        if named:
            (actual if named.group(2) == 'test' else benchmarks).append(named.group(1))
            continue
        total = re.fullmatch(r'(\d+) tests?, (\d+) benchmarks?', line)
        assert total, 'Unrecognized compiled inventory line'
        totals.append(tuple(map(int, total.groups())))
    assert totals == [(len(actual), len(benchmarks))], 'Missing/duplicate/inconsistent inventory total'
    assert selected and len(selected) == len(set(selected)), 'Distinct exact selectors required'
    assert all(actual.count(name) == 1 for name in selected), 'Missing/duplicate selected test'
    assert len(actual + benchmarks) == len(set(actual + benchmarks)), 'Duplicate full inventory name'
    return actual


def parent_pass(text, contract):
    """Parse only an original stdout stream, never concatenate staged stderr."""
    name = contract['name']
    assert contract['expected_stdout_child_results'] == 0
    events, summaries = [], []
    for line in text.splitlines():
        if line.startswith('test result:'):
            found = re.fullmatch(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; 0 measured; (\d+) filtered out(?:; finished in \d+(?:\.\d+)?s)?', line)
            assert found and found.groups()[:4] == ('ok', '1', '0', '0'), 'Original stream must be exactly1P0F0I'
            if 'expected_filtered_out' in contract:
                assert int(found.group(5)) == contract['expected_filtered_out'], 'Filtered count differs from fresh actual ELF inventory N-1'
            summaries.append(found.groups())
            events.append('summary')
        elif line.lstrip().startswith('test result:'):
            raise AssertionError('Malformed summary-prefixed line')
        elif line.startswith('running '):
            assert line == 'running 1 test', 'Unexpected libtest run count/shape'
            events.append('run1')
        elif line.startswith('test '):
            assert line == 'test ' + name + ' ... ok', 'Unexpected selected name, failure or result shape'
            events.append('named-ok')
        elif line == 'ok' or line == 'FAILED' or line.startswith('ignored'):
            raise AssertionError('Unexpected standalone completion/failure/ignored result')
    assert events == ['run1', 'named-ok', 'summary'] and len(summaries) == 1, 'Missing/duplicate/interleaved original completion'
    return {'outer_passed': 1, 'outer_failed': 0, 'outer_ignored': 0,
            'nonadditive_child_results': [], 'source_derived_event_shape': events}


def scope_observation(raw_stderr, contract, outer_qualified):
    """Describe actual staged evidence; no absent marker can establish isolation."""
    assert contract['name'] == SCOPE_PARENT
    reasons = [value.decode('utf-8', errors='replace') for value in re.findall(
        rb'scope isolation unqualified:[^\r\n]*', raw_stderr, re.IGNORECASE)]
    result = {'scope_isolation': 'NOT_QUALIFIED' if reasons else 'UNKNOWN_UNQUALIFIED',
              'unqualified_reasons': reasons, 'adoption_close': 'UNKNOWN_UNQUALIFIED',
              'frames_complete': False, 'child_stdout_result': None, 'frame_or_child_error': None,
              'streams': {}, 'nonadditive_child_results': [],
              'limits': 'Actual conserved owning fixture assertions only; no native/provider/UI/global qualification.'}
    try:
        cursor, streams = 0, {}
        for stream in ('stdout', 'stderr'):
            start = ('native adoption/close staged ' + stream + ' begin\n').encode()
            end = ('\nnative adoption/close staged ' + stream + ' end\n').encode()
            assert raw_stderr.count(start) == raw_stderr.count(end) == 1, 'Missing/duplicate/incomplete ' + stream + ' frame'
            first = raw_stderr.index(start)
            last = raw_stderr.index(end)
            assert first >= cursor and last >= first + len(start), 'Reordered/overlapping staged frames'
            assert first == 0 or raw_stderr[first - 1:first] == b'\n', 'Staged header is not a complete labelled line'
            if streams:
                assert first == cursor, 'Unexpected bytes between original staged streams'
            payload = raw_stderr[first + len(start):last]
            streams[stream] = payload
            result['streams'][stream] = {'bytes': len(payload), 'sha256': hashlib.sha256(payload).hexdigest()}
            cursor = last + len(end)
        result['frames_complete'] = True
        # Strict decoding prevents replacement bytes from manufacturing libtest completion.
        child = parent_pass(streams['stdout'].decode('utf-8'), contract)
        result['child_stdout_result'] = child
        result['nonadditive_child_results'] = [{'name': SCOPE_PARENT, 'passed': 1, 'failed': 0, 'ignored': 0,
                                               'source': 'Complete original staged stdout frame; never added to outer parent count.'}]
        if outer_qualified:
            result['adoption_close'] = 'OWNING_ASSERTIONS_COMPLETED'
        marker = b'scope isolation qualified: manager namespace proved; strict holder/root separation asserted'
        positives = [line for line in streams['stderr'].splitlines() if line == marker]
        all_positives = [line for line in raw_stderr.splitlines() if line == marker]
        if reasons:
            result['scope_isolation'] = 'NOT_QUALIFIED'
        elif outer_qualified and len(positives) == len(all_positives) == 1:
            result['scope_isolation'] = 'QUALIFIED_SCOPED_FIXTURE'
    except (AssertionError, UnicodeError, ValueError) as exc:
        result['frame_or_child_error'] = type(exc).__name__ + ': ' + str(exc)
    return result


def validate_controller_review(data, head, tree, runner_sha, plan_sha):
    review = json.loads(data)
    assert review['verdict'] == 'CONTROLLER_SOURCE_CLEAR'
    assert review['head'] == head and review['tree'] == tree
    assert review['runner_sha256'] == runner_sha and review['plan_sha256'] == plan_sha
    return review


def evidence_copy(out, name, payload, expected_sha):
    assert hashlib.sha256(payload).hexdigest() == expected_sha
    path = out / name
    with path.open('xb') as stream:
        stream.write(payload)
    assert digest(path) == expected_sha


def execute_contracts(plan, invoke, save, parents):
    artifacts = {}
    for check in plan['binary_checks']:
        invoke(check['label'], [str(TOOL / 'cargo'), *check['argv'][1:]], check['seconds'])
    for group in plan['artifact_groups']:
        gid = group['id']
        compiled = invoke('compile-' + gid, [str(TOOL / 'cargo'), *group['compile_argv'][1:]], group['compile_seconds'])
        artifact = cargo_test_artifact((compiled / 'stdout.log').read_text(), WT / group['manifest'], group['features'])
        exe = Path(artifact['executable'])
        assert exe.is_absolute() and exe == exe.resolve(strict=True)
        assert exe.is_relative_to(TARGET.resolve(strict=True)) and exe.is_file() and not exe.is_symlink()
        with exe.open('rb') as stream:
            assert stream.read(4) == b'\x7fELF'
        elf = {'path': str(exe), 'sha256': digest(exe), 'features': artifact['features'], 'artifact': artifact,
               'limits': 'Original Cargo-reported ELF is hashed before and after use; full ELF bytes are not archived.'}
        artifacts[gid] = elf
        save('test-elf-' + gid + '.json', elf)
        assert digest(exe) == elf['sha256']
        inventory = invoke('inventory-' + gid, [str(exe), '--list', '--color=never'], group['inventory_seconds'])
        assert digest(exe) == elf['sha256'], 'ELF changed during inventory'
        selected = [contract['name'] for contract in group['selectors']]
        compiled_selected = group['source_predicted_compiled_names']
        absent = group['source_predicted_absent_names']
        actual = inventory_names((inventory / 'stdout.log').read_text() + '\n' + (inventory / 'stderr.log').read_text(), compiled_selected)
        assert set(selected).issubset(compiled_selected), 'Assigned execution is absent from source-pinned compilation contract'
        assert not set(absent).intersection(actual), 'Source-pinned excluded feature selectors unexpectedly compiled'
        save('inventory-' + gid + '/inventory.json', {'assigned_execution_names': selected,
             'source_predicted_compiled_selected_names': compiled_selected, 'source_predicted_absent_names': absent,
             'full_names': actual, 'actual_test_N': len(actual), 'assigned_execution_count': len(selected),
             'compiled_selected_count': len(compiled_selected), 'not_passes': True})
        for index, contract in enumerate(group['selectors'], 1):
            contract = {**contract, 'expected_filtered_out': len(actual) - 1}
            assert digest(exe) == elf['sha256']
            d = invoke('parent-' + gid + '-' + str(index),
                       [str(exe), contract['name'], '--exact', '--nocapture', '--test-threads=1', '--color=never'],
                       contract['seconds'], contract)
            assert digest(exe) == elf['sha256'], 'ELF changed during owning parent'
            proof = parent_pass((d / 'stdout.log').read_text(), contract)
            if contract.get('staged_child_observation'):
                observation = scope_observation((d / 'stderr.log').read_bytes(), contract, True)
                save(str(d.relative_to(d.parent)) + '/staged-observation.json', observation)
                assert observation['adoption_close'] == 'OWNING_ASSERTIONS_COMPLETED', 'Incomplete actual staged adoption/close assertions'
                proof['nonadditive_child_results'] = observation['nonadditive_child_results']
                proof['scope_observation'] = observation
            parents.append({'group': gid, 'name': contract['name'], 'passed': 1, 'failed': 0, 'ignored': 0,
                            'elf_sha256': elf['sha256'], **proof})
    save('test-elfs.json', artifacts)
    return artifacts


def custody_clean(custody):
    return (isinstance(custody, dict) and custody.get('clean') is True
            and custody.get('forced') is False and not custody.get('inspection_error')
            and not custody.get('supervision_error') and not custody.get('remaining'))


def safe_lane_handoff(attempts, states, phases, final_custody, failures):
    if (failures or not custody_clean(final_custody)
            or len(final_custody.get('passes', [])) != 2
            or not all(custody_clean(value) for value in final_custody['passes'])):
        return False
    if [(attempt[0], attempt[1], attempt[3]) for attempt in attempts] != [
            (state['stage'], state['label'], state['pid']) for state in states]:
        return False
    if not all(state['original_wait_complete'] is True and state['exit'] is not None
               and state['direct_cleanup_attempted'] is False
               and not state['supervision_error'] and state['evidence_complete'] is True
               for state in states):
        return False
    phase_states = [state for state in states if state['stage'] == 'phase']
    return ([(state['label'], state['pid']) for state in phase_states]
            == [(phase['label'], phase['pid']) for phase in phases]
            and all(custody_clean(state['custody']) for state in phase_states))


def parent_accounting(attempts, states, phases, parents, out):
    attempted = [attempt for attempt in attempts if attempt[0] == 'phase' and attempt[1].startswith('parent-')]
    raw, errors = [], []
    for stage, label, argv, pid in attempted:
        state = next((state for state in states if state['stage'] == stage and state['label'] == label and state['pid'] == pid), None)
        record = next((phase for phase in phases if phase['label'] == label and phase['pid'] == pid), None)
        row = {'phase': label, 'pid': pid, 'exit': state['exit'] if state else None,
               'original_wait_complete': bool(state and state['original_wait_complete']),
               'phase_recorded': record is not None,
               'evidence_complete': bool(state and state['evidence_complete']),
               'supervision_error': state['supervision_error'] if state else 'Spawn has no supervision state',
               'summaries': [], 'output_read_error': None}
        try:
            output = (out / label / 'stdout.log').read_text()
            row['summaries'] = [line for line in output.splitlines() if line.lstrip().startswith('test result:')]
            if len(argv) > 1 and argv[1] == SCOPE_PARENT:
                contract = {'name': SCOPE_PARENT, 'expected_stdout_child_results': 0}
                inventory = json.loads((out / 'inventory-host-lib/inventory.json').read_bytes())
                contract['expected_filtered_out'] = inventory['actual_test_N'] - 1
                outer_qualified = False
                try:
                    parent_pass(output, contract)
                    outer_qualified = bool(state and state['original_wait_complete'] and state['exit'] == 0
                                           and state['evidence_complete'] and not state['supervision_error'])
                except (AssertionError, UnicodeError, ValueError):
                    pass
                row['scope_observation'] = scope_observation((out / label / 'stderr.log').read_bytes(), contract, outer_qualified)
        except Exception as exc:
            row['output_read_error'] = type(exc).__name__ + ': ' + str(exc)
            errors.append(row['output_read_error'])
        raw.append(row)
    count = len(attempted)
    assert 0 <= len(parents) <= count <= 9
    settled = sum(row['original_wait_complete'] for row in raw)
    phase_attempts = [attempt for attempt in attempts if attempt[0] == 'phase']
    phase_states = [state for state in states if state['stage'] == 'phase']
    source_attempts = [attempt for attempt in attempts if attempt[0] == 'source']
    source_states = [state for state in states if state['stage'] == 'source']
    settled_phases = sum(state['original_wait_complete'] for state in phase_states)
    settled_sources = sum(state['original_wait_complete'] for state in source_states)
    return ({'attempted_process_count': len(attempts),
             'original_wait_settled_process_count': settled_phases + settled_sources,
             'unknown_process_count': len(attempts) - settled_phases - settled_sources,
             'attempted_phase_count': len(phase_attempts), 'recorded_phase_count': len(phases),
             'original_wait_settled_phase_count': settled_phases,
             'unknown_phase_count': len(phase_attempts) - settled_phases,
             'unrun_phase_count': 12 - len(phase_attempts),
             'attempted_source_check_count': len(source_attempts),
             'original_wait_settled_source_check_count': settled_sources,
             'unknown_source_check_count': len(source_attempts) - settled_sources,
             'all_original_waits_complete': len(attempts) == len(states) and all(state['original_wait_complete'] for state in states),
             'requested_parent_count': 9, 'passed_parent_count': len(parents),
             'attempted_parent_count': count, 'executed_parent_count': count,
             'original_wait_settled_parent_count': settled, 'unknown_parent_count': count - settled,
             'recorded_parent_count': sum(row['phase_recorded'] for row in raw),
             'incomplete_parent_record_count': sum(not row['evidence_complete'] for row in raw),
             'nonqualifying_parent_count': count - len(parents), 'unrun_parent_count': 9 - count}, raw, errors)


def final_observations(source, context, before, before_context, save):
    errors, custody_passes = [], []
    def collect():
        try:
            value = settle_children()
        except Exception as exc:
            value = {'clean': False, 'forced': False, 'inspection_error': type(exc).__name__ + ': ' + str(exc)}
        custody_passes.append(value)
        if not value['clean'] or value['forced']:
            errors.append('Final custody unclean/forced/uncertain: ' + json.dumps(value))
    collect()
    try:
        final_source, final_context = source(), context()
        assert final_source == before and final_context == before_context
        save('source-after.json', final_source)
        save('context-after.json', final_context)
    except Exception as exc:
        errors.append('Final source/context: ' + type(exc).__name__ + ': ' + str(exc))
    # Source checks can spawn Git. Re-establish custody after the last such process,
    # before any clean-family lock release claim. Earlier failures/forced state survive.
    collect()
    custody = {'clean': all(custody_clean(value) for value in custody_passes),
               'forced': any(value['forced'] for value in custody_passes),
               'passes': custody_passes}
    return errors, custody


def note_lane_cancellation(number, _frame):
    # Cooperative cancellation cannot implicitly drop a live/unknown C lease.
    if len(CANCELLATION_SIGNALS) < 64:
        CANCELLATION_SIGNALS.append(number)


def require_workload_allowed():
    if CANCELLATION_SIGNALS:
        raise RuntimeError('Cancellation refuses new validation workload')


def retain_uncertain_lane(save, evidence):
    """Keep the original process/reservation; perform no further control effects.

    Handlers are installed before C acquisition. No close, retry, signal or new
    cleanup occurs here. An ambiguous prior close is not called a held lock.
    SIGKILL and external process loss remain limits; the allocator cannot infer
    return from lock availability.
    """
    while True:
        try:
            save('C-return-unconfirmed.json', {
                'safe_handback': False, 'original_evidence': evidence,
                'cancellation_signals': CANCELLATION_SIGNALS,
                'policy': 'Original process retained; no workload, close retry, signalling or new cleanup.'})
        except BaseException:
            pass
        try:
            time.sleep(1)
        except BaseException:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--grant', required=True, help='Fresh actual EC lane grant message ID; preparation is not authority')
    parser.add_argument('--runner-sha256', required=True, help='Externally frozen and independently reviewed executing-runner SHA256')
    parser.add_argument('--controller-review', required=True, help='Actual independent frozen controller review receipt')
    parser.add_argument('--controller-review-sha256', required=True, help='Externally frozen independent receipt SHA256')
    args = parser.parse_args()
    assert re.fullmatch(r'[0-9a-f]{8}(?:-[0-9a-f-]{27})?', args.grant), 'Actual grant ID required'
    assert re.fullmatch(r'[0-9a-f]{64}', args.runner_sha256), 'Frozen runner SHA required'
    runner = Path(__file__)
    assert runner.is_absolute() and not runner.is_symlink() and runner.resolve(strict=True) == runner
    runner_bytes = runner.read_bytes()
    assert hashlib.sha256(runner_bytes).hexdigest() == args.runner_sha256, 'Executing runner differs from reviewed pin'
    plan_bytes = PLAN.read_bytes()
    assert hashlib.sha256(plan_bytes).hexdigest() == PLAN_SHA
    plan = json.loads(plan_bytes)
    assert str(runner) == plan['runner_path'] and str(WT) == plan['worktree']
    assert digest(plan['source_review']) == plan['source_review_sha256']
    review_bytes = Path(plan['source_review']).read_bytes()
    review = json.loads(review_bytes)
    assert review['head'] == plan['product_head'] and review['tree'] == plan['product_tree']
    assert review['kind'] == 'AUTHOR_ASSEMBLED_PR1239_HOST9_SOURCE_BINDING_NOT_A_NEW_REVIEW'
    assert review['verdict'] == 'BOUND_EXACT_SCOPED_SOURCE_CHAIN_ONLY_NO_WHOLE_HEAD_APPROVAL'
    assert review['owning_parent_count'] == 9
    def read_bound(key, as_json=True):
        ref = review[key]
        payload = Path(ref['path']).read_bytes()
        assert hashlib.sha256(payload).hexdigest() == ref['sha256'], 'Changed source/report input: ' + key
        return json.loads(payload) if as_json else payload.decode('utf-8')
    original_data = read_bound('command_data_v1')
    data = read_bound('command_data_v2')
    assert review['command_data_v1']['sha256'] == '589e6e7179d4f4cef2cc912f5a5f971b0a23a0e2af505ad611a3808c0d30aa9a'
    assert original_data['schema'] == 'pr1239-195052bf-narrow-host-qualification-command-test-data-v1'
    assert original_data['kind'] == 'PLAIN_JSON_COMMAND_AND_TEST_DATA_ONLY_NO_EXECUTOR'
    source_data = original_data['source']
    assert source_data['head'] == review['eof_ancestor_head'] == '195052bf1d7ce2d5b6580e3335c6ef0628cae981'
    assert source_data['tree'] == review['eof_ancestor_tree'] == 'c92260aa4c2ea0a5a69935ca6aa347baa84497cb'
    assert source_data['parent'] == review['eof_base'] == '4976799fce3752372e421a2b5be247278b9a71e7'
    assert source_data['worktree'] == review['eof_ancestor_worktree']
    assert source_data['only_committed_changed_path'] == 'src/chat/runtime.rs'
    source_hashes = {pin['path']: pin['sha256'] for pin in source_data['pins']}
    assert len(source_hashes) == len(source_data['pins']) == 9
    assert source_hashes == review['retained_eof_source_hashes']
    runtime_sha = source_hashes['src/chat/runtime.rs']
    assert runtime_sha == 'e27090299528724f6a6e24d1f48fe5d8f8eeb5fedd52f2118feb51f97e2bc1ca'
    names = [parent['selector'] for parent in original_data['selected_parents']]
    assert len(names) == len(set(names)) == 6 and all(isinstance(name, str) for name in names)
    assert all(parent['source'] == 'src/chat/runtime.rs' and parent['source_sha256'] == runtime_sha
               and parent['ignored'] is False and parent['static_parent_count'] == 1
               for parent in original_data['selected_parents'])
    assert original_data['planned_counts']['proposed_phases'] == 9
    assert original_data['planned_counts']['original_required_libtest_parents'] == 6
    assert not set(names).intersection(parent['selector'] for parent in original_data['explicitly_excluded_parents'])
    # This versioned DATA normalization is explicit; execute_contracts stays byte-exact.
    expected_data = json.loads(json.dumps(original_data))
    expected_data['schema'] = 'pr1239-195052bf-narrow-host-qualification-command-test-data-v2-host9-argv-normalization'
    expected_data['Cargo_JSON_ELF_admission_recipe_DATA']['inventory'] = 'Invoke that one artifact with --list --color=never and original exit0. Preserve full actual inventory. Require all six exact selectors once as tests; selected-name static declarations are not binary discovery.'
    expected_data['proposed_serial_commands'][2]['argv'] = ['--list', '--color=never']
    for index in range(3, 9):
        expected_data['proposed_serial_commands'][index]['argv'].append('--color=never')
    for parent in expected_data['selected_parents']:
        parent['future_argv'].append('--color=never')
    expected_data['normalization'] = review['normalization']
    assert data == expected_data
    assert review['normalization']['source_sha256'] == review['command_data_v1']['sha256']
    assert len(review['normalization']['changes']) == 15
    production = read_bound('production_source_review', False)  # Actual raw Markdown, never relabelled JSON.
    oracle = read_bound('oracle_source_review')
    fixture = read_bound('fixture_source_review')
    freeze = read_bound('source_freeze')
    publication = read_bound('publication')
    conservation = read_bound('source_conservation')
    script_review = read_bound('script_ci_review')  # Prior Script63 review only; no original log read or rerun.
    refs = original_data['reused_reviews']
    for key, ref in zip(('production_source_review', 'oracle_source_review', 'fixture_source_review'), refs):
        assert review[key]['original_path'] == ref['path'] and review[key]['sha256'] == ref['sha256']
    assert 'Verdict: SOURCE-CLEAR for the two production changes.' in production
    assert 'Base: ' + review['eof_base'] in production and 'Reviewed runtime.rs SHA256: ' + runtime_sha in production
    assert 'source clearance only' in production and 'not approved by this receipt' in production
    assert oracle['base'] == oracle['head_readback'] == review['eof_base']
    assert oracle['verdict']['result'].startswith('Bounded source clear for the regression oracle;')
    assert oracle['owning_inventory']['selector'] == names[0]
    assert (oracle['owning_inventory']['new_libtest_parents'], oracle['owning_inventory']['nested_cases'], oracle['owning_inventory']['nonadditive']) == (1, 4, True)
    assert oracle['conservation']['full_reversion_equals_base'] is True
    assert {pin['path']: pin['sha256'] for pin in oracle['source_pins']}['src/chat/runtime.rs'] == runtime_sha
    assert fixture['verdict'] == 'BOUNDED_SOURCE_CLEAR_NO_CONCRETE_NEW_TEST_OWNERSHIP_OR_TYPE_BLOCKER_IDENTIFIED'
    assert fixture['findings'] == [] and fixture['compilation'] == fixture['Rust_tests'] == 'UNRUN'
    assert fixture['source']['observed_HEAD'] == review['eof_base']
    assert fixture['source']['runtime_source_matches_frozen_v2'] is True
    assert {pin['path']: pin['sha256'] for pin in fixture['source']['files']}['src/chat/runtime.rs'] == runtime_sha
    assert freeze['base'] == review['eof_base'] and freeze['source']['sha256'] == runtime_sha
    assert freeze['source']['bytes'] == 426433 and freeze['counts'] == {'parents': 1, 'nested_cases': 4, 'nonadditive': True}
    assert oracle['freeze_and_design'][0]['sha256'] == fixture['source']['freeze']['sha256'] == review['source_freeze']['sha256']
    # The archived stale196/2 sidecar is not repaired or promoted into a source result.
    assert freeze['diff_numstat'] == '196\t2\tsrc/chat/runtime.rs'
    assert source_data['numstat'] == '203 additions /2 deletions'
    assert conservation['kind'] == 'AUTHOR_EXACT_COMMIT_AND_SOURCE_CONSERVATION_NOT_INDEPENDENT_APPROVAL'
    assert conservation['head'] == publication['head'] == review['eof_ancestor_head']
    assert conservation['tree'] == publication['tree'] == review['eof_ancestor_tree']
    assert conservation['parent'] == publication['parent'] == review['eof_base']
    assert conservation['only_changed_path'] == ['src/chat/runtime.rs'] and conservation['all_other_tree_entries_equal'] is True
    assert conservation['source_paths_sha256'] == source_hashes
    assert conservation['current_runtime_sha256'] == publication['source_sha256'] == runtime_sha
    assert conservation['parent_runtime_sha256'] in production
    assert conservation['parent_tree'] == oracle['base_tree'] == fixture['source']['observed_HEAD_tree']
    assert publication['pr'] == 1239 and publication['branch'] == 'campaign/provider_stream'
    assert publication['reviews'] == refs and publication['push']['exit_code'] == 0
    assert publication['remote_readback'].splitlines() == [
        review['eof_ancestor_head'] + '\trefs/heads/campaign/provider_stream',
        review['eof_ancestor_head'] + '\trefs/heads/fix/fw16-pr1239-eof-backlog']
    assert script_review['publication_receipt']['sha256'] == review['publication']['sha256']
    assert script_review['head'] == script_review['current_pr']['head_sha'] == review['eof_ancestor_head']
    assert script_review['tree'] == script_review['run']['head_commit']['tree'] == review['eof_ancestor_tree']
    assert script_review['parent'] == review['eof_base'] and script_review['result'] == 'CLEAR_SCRIPT_CI_ONLY'
    assert script_review['observed_tests']['distinct_methods'] == script_review['observed_tests']['passed'] == 63
    assert script_review['qualification']['owning_HOST_tests'] == 'UNRUN'
    assert script_review['run']['id'] == original_data['linked_script63_result_DATA']['run_id'] == 41161495569
    assert review['script_ci_review']['sha256'] == original_data['linked_script63_result_DATA']['sha256']
    # The retained chain above covers identical EOF bytes only. The following
    # two-file working-delta reviews are tied to the newly published child.
    current = read_bound('current_command_data')
    current_delta = read_bound('current_data_delta')
    api_review = read_bound('test_api_review', False)  # Genuine raw Markdown.
    test_conservation = read_bound('test_conservation_review')
    test_freeze = read_bound('test_source_freeze')
    patch_archive = read_bound('test_source_delta')
    current_publication = read_bound('current_publication')
    old_return = read_bound('original_negative_return')  # Existing metadata only, not RAW re-audit.
    committed = review['current_commit_binding']  # Author binding, not independent whole-head approval.
    assert current['schema'] == 'pr1239-8bcd52d1-narrow-host-command-data-v3-host8'
    assert current['kind'] == 'TASK_BOUND_SOURCE_DATA_ONLY_NOT_AN_EXECUTION_GRANT'
    assert current['source']['head'] == review['host8_head'] == '8bcd52d1d99f9193cc3d4f5a8b121e0211d19a6e'
    assert current['source']['tree'] == review['host8_tree'] == '456474c461129f22dcfb51bd4dd6fe09e112fe2b'
    assert current['source']['parent'] == review['host8_parent'] == review['eof_ancestor_head']
    assert review['eof_ancestor_head'] == '195052bf1d7ce2d5b6580e3335c6ef0628cae981'
    assert current['source']['worktree'] == review['host8_worktree']
    current_hashes = {pin['path']: pin['sha256'] for pin in current['source']['pins']}
    assert len(current_hashes) == len(current['source']['pins']) == 11
    assert current_hashes == review['host8_source_paths_sha256']
    assert current['source']['pins'][:9] == original_data['source']['pins']
    assert {path: value for path, value in current_hashes.items() if path not in source_hashes} == {
        'src/chat/client.rs': 'a625d2b0d3773f234a8d8b7d66bb1ed0f001c365c833ed5debb6e98c41a6e156',
        'src/chat/native_holder/transport.rs': '27788e187001a61d794404208d3cd4653d8e7ceccf4ee3a4ed6504867c293283'}
    assert all(current_hashes[path] == value for path, value in source_hashes.items())
    assert current['selected_parents'][:6] == data['selected_parents']
    current_names = [parent['selector'] for parent in current['selected_parents']]
    assert current_names == names + [
        'chat::client::tests::handled_command_refusals_do_not_become_shared_snapshot_errors',
        'chat::native_holder::transport::tests::legacy_named_json_inspection_messages_remain_decodable']
    assert len(current_names) == len(set(current_names)) == 8
    assert all(parent['ignored'] is False and parent['static_parent_count'] == 1
               and parent['source_sha256'] == current_hashes[parent['source']]
               for parent in current['selected_parents'])
    assert current['counts'] == {'HOST_checks': 1, 'HOST_lib_compiles': 1,
        'actual_unique_ELF_inventories': 1, 'outer_parents': 8, 'phases': 11,
        'retained_first_parents': 6, 'appended_parents': 2,
        'additional_counted_children': 0, 'new_EOF_table_cases_nonadditive': 4}
    assert len(current['proposed_serial_commands']) == 11
    assert all(command['cwd'] == review['host8_worktree'] for command in current['proposed_serial_commands'])
    assert current['proposed_serial_commands'][2]['argv'] == ['--list', '--color=never']
    assert [command['argv'] for command in current['proposed_serial_commands'][:9]] == [command['argv'] for command in data['proposed_serial_commands']]
    for index, name in enumerate(current_names):
        assert current['proposed_serial_commands'][index + 3]['argv'] == [name, '--exact', '--nocapture', '--test-threads=1', '--color=never']
        assert current['selected_parents'][index]['future_argv'] == current['proposed_serial_commands'][index + 3]['argv']
    assert current_delta['before']['sha256'] == review['command_data_v2']['sha256']
    assert current_delta['after_sha256'] == review['current_command_data']['sha256']
    assert 'Verdict: SOURCE CLEAR for the exact three added cfg(test) lines only.' in api_review
    assert review['host8_parent'] in api_review and all(current_hashes[path] in api_review for path in ('src/chat/client.rs', 'src/chat/native_holder/transport.rs'))
    assert test_conservation['verdict'] == 'SOURCE_CONSERVATION_CLEAR_FOR_EXACT_FROZEN_THREE_LINE_TEST_DELTA'
    assert test_conservation['blocking_findings'] == []
    assert test_conservation['pins']['parent_commit'] == review['host8_parent']
    assert test_conservation['pins']['parent_tree'] == review['eof_ancestor_tree']
    assert test_conservation['exact_delta']['inserted_lines'] == 3 and test_conservation['exact_delta']['deleted_lines'] == 0
    assert test_conservation['exact_delta']['production_changed'] is False
    assert test_conservation['exact_delta']['original_assertions_weakened_or_removed'] is False
    assert test_conservation['all_other_entries_and_modes']['conserved_other_leaf_entries'] == 2221
    assert {pin['path']: pin['candidate_sha256'] for pin in test_conservation['pins']['source_files']} == {
        path: current_hashes[path] for path in ('src/chat/client.rs', 'src/chat/native_holder/transport.rs')}
    assert all(pin['inverse_byte_equal_entire_original_file'] and pin['production_prefix_byte_equal']
               and pin['original_test_assertions_and_all_other_bytes_retained'] for pin in test_conservation['pins']['source_files'])
    assert {pin['path']: pin['candidate_sha256'] for pin in test_conservation['original_eof_and_selected_parent_source_preservation']['all9_original_pinned_files_equal']} == source_hashes
    assert test_freeze['parent'] == review['host8_parent']
    assert test_freeze['source_files'] == {path: current_hashes[path] for path in ('src/chat/client.rs', 'src/chat/native_holder/transport.rs')}
    assert test_conservation['pins']['freeze_sha256'] == review['test_source_freeze']['sha256']
    assert patch_archive['raw_sha256'] == test_freeze['delta_sha256'] == test_conservation['pins']['frozen_delta_sha256']
    raw_patch = patch_archive['raw_utf8'].encode('utf-8')
    assert hashlib.sha256(raw_patch).hexdigest() == patch_archive['raw_sha256']
    assert len(raw_patch) == patch_archive['raw_bytes'] == test_conservation['pins']['delta_bytes'] == 1089
    assert committed['kind'] == 'AUTHOR_COMMITTED_THREE_CFGTEST_LINES_BINDING_NOT_INDEPENDENT_WHOLE_HEAD_APPROVAL'
    assert committed['head'] == current_publication['head'] == review['host8_head']
    assert committed['tree'] == current_publication['tree'] == review['host8_tree']
    assert committed['parent'] == current_publication['parent'] == review['host8_parent']
    assert committed['delta_sha256'] == patch_archive['raw_sha256']
    assert committed['changed_paths'] == current['source']['only_changed_paths_from_parent'] == ['src/chat/client.rs', 'src/chat/native_holder/transport.rs']
    assert committed['inserted_lines'] == 3 and committed['deleted_lines'] == 0
    assert committed['all_other_entries_and_modes_equal'] is True and committed['exact_inverse_changed_files_equals_parent'] is True
    assert committed['conserved_other_entries'] == 2221 and committed['source_pins'] == current['source']['pins']
    assert current_publication['source_review_sha256'] == review['test_api_review']['sha256']
    assert current_publication['conservation_review_sha256'] == review['test_conservation_review']['sha256']
    assert current_publication['pr'] == 1239 and current_publication['state'] == 'OPEN_DRAFT'
    assert current_publication['Script_contracts'] == current['current_Script_metadata']
    assert current_publication['Script_contracts']['head'] == review['host8_head']
    assert current_publication['Script_contracts']['status'] == 'completed' and current_publication['Script_contracts']['conclusion'] == 'success'
    assert old_return['product'] == review['host8_parent'] and old_return['controller'] == '54f49ab7dd2edd7367349f0698bae83efbd953b6'
    assert old_return['actual_results']['host_binary_check_exit'] == 0 and old_return['actual_results']['host_library_compile_exit'] == 101
    assert old_return['actual_results']['parents_attempted'] == old_return['actual_results']['parents_passed'] == 0
    assert old_return['actual_results']['parents_unrun'] == 6 and old_return['new_execution_authorized'] is False
    assert old_return['allocation'] == 'aa35221a-9192-4d38-b8f3-5e426578bfba'
    # The return retains its original pending timestamp. Later supplied publication
    # metadata records acceptance; neither record authorizes this new attempt.
    assert current_publication['old_C'] == 'aa352CLOSED_ACCEPTED2492b6bc; no consumedgranttransfer'
    # Existing HOST8 source evidence above remains scoped to its original
    # tuple. Only this new fixture block is bound to the current HOST9 source.
    host9 = read_bound('host9_command_data')
    host9_delta = read_bound('host9_data_delta')
    varint_semantic = read_bound('varint_semantic_review', False)  # Genuine raw Markdown.
    varint_conservation = read_bound('varint_conservation_review')
    varint_freeze = read_bound('varint_source_freeze')
    varint_patch = read_bound('varint_source_delta')
    varint_publication = read_bound('varint_publication')
    varint_committed = review['varint_commit_binding']  # Author binding only, not whole-head approval.
    leaf = 'src/host/upgrade_inspect/current.rs'
    varint_name = 'host::upgrade_inspect::current::tests::frozen_inventory_subset_accepts_99_through_104_only'
    assert host9['schema'] == 'pr1239-73487c8d-narrow-host-command-data-v4-host9'
    assert host9['source']['head'] == review['head'] == plan['product_head'] == '73487c8d69e826de3865b8e5692cd9e39485ddd5'
    assert host9['source']['tree'] == review['tree'] == plan['product_tree'] == '0ac51f1aa553ddd30fd0e993d25397b08e58172f'
    assert host9['source']['parent'] == review['parent'] == review['host8_head']
    assert review['host8_head'] == '8bcd52d1d99f9193cc3d4f5a8b121e0211d19a6e'
    assert host9['source']['worktree'] == str(WT)
    host9_hashes = {pin['path']: pin['sha256'] for pin in host9['source']['pins']}
    assert len(host9_hashes) == len(host9['source']['pins']) == 12
    assert host9_hashes == review['source_paths_sha256'] == plan['source_paths_sha256']
    assert host9['source']['pins'][:11] == current['source']['pins']
    assert {path: value for path, value in host9_hashes.items() if path not in current_hashes} == {
        leaf: 'c118617f5666ab3de65f5f755d670a4595c1432bf9374e0586189fcc8d2a4765'}
    assert all(host9_hashes[path] == value for path, value in current_hashes.items())
    assert host9['selected_parents'][:8] == current['selected_parents']
    host9_names = [parent['selector'] for parent in host9['selected_parents']]
    assert host9_names == current_names + [varint_name] and len(set(host9_names)) == 9
    assert all(parent['ignored'] is False and parent['static_parent_count'] == 1
               and parent['source_sha256'] == host9_hashes[parent['source']] for parent in host9['selected_parents'])
    assert host9['selected_parents'][8]['source'] == leaf
    assert host9['selected_parents'][8]['nested_cases'] == 7 and host9['selected_parents'][8]['nested_cases_are_nonadditive'] is True
    assert len(host9['proposed_serial_commands']) == 12 and host9['counts']['outer_parents'] == 9 and host9['counts']['phases'] == 12
    assert host9['counts']['additional_counted_children'] == 0
    assert all(command['cwd'] == str(WT) for command in host9['proposed_serial_commands'])
    assert [command['argv'] for command in host9['proposed_serial_commands'][:11]] == [command['argv'] for command in current['proposed_serial_commands']]
    assert host9['proposed_serial_commands'][2]['argv'] == ['--list', '--color=never']
    for index, name in enumerate(host9_names):
        assert host9['proposed_serial_commands'][index + 3]['argv'] == [name, '--exact', '--nocapture', '--test-threads=1', '--color=never']
        assert host9['selected_parents'][index]['future_argv'] == host9['proposed_serial_commands'][index + 3]['argv']
    assert host9_delta['before']['sha256'] == review['current_command_data']['sha256']
    assert host9_delta['after_sha256'] == review['host9_command_data']['sha256']
    assert 'Verdict: bounded SOURCE CLEAR for this cfg(test) fixture correction.' in varint_semantic
    assert review['parent'] in varint_semantic and host9_hashes[leaf] in varint_semantic
    assert varint_conservation['verdict'] == 'SOURCE_CONSERVATION_CLEAR_FOR_EXACT_FROZEN_ONE_TEST_BLOCK_DELTA'
    assert varint_conservation['blocking_findings'] == []
    assert varint_conservation['pins']['parent_commit'] == varint_freeze['parent'] == review['parent']
    assert varint_conservation['pins']['parent_tree'] == varint_freeze['parent_tree'] == review['host8_tree']
    assert varint_conservation['pins']['changed_source'] == leaf
    assert varint_conservation['pins']['candidate_file_sha256'] == varint_freeze['file_sha256'] == host9_hashes[leaf]
    assert varint_conservation['one_intended_test_block']['complete_file_after_inverse_byte_equal_parent'] is True
    assert varint_conservation['production_allowlist_assertion_conservation']['production_prefix_byte_equal'] is True
    assert varint_conservation['production_allowlist_assertion_conservation']['supports_protocol_function_byte_equal'] is True
    assert varint_conservation['production_allowlist_assertion_conservation']['source_protocols_helper_byte_equal'] is True
    assert varint_conservation['production_allowlist_assertion_conservation']['protocol_allowlist_change'] is False
    assert varint_conservation['production_allowlist_assertion_conservation']['original_assertion_start_line_sequence_byte_equal'] is True
    assert varint_conservation['all_other_tracked_entries']['conserved_other_entries'] == 2222
    assert varint_conservation['pins']['freeze_sha256'] == review['varint_source_freeze']['sha256']
    raw_varint_patch = varint_patch['raw_utf8'].encode('utf-8')
    assert hashlib.sha256(raw_varint_patch).hexdigest() == varint_patch['raw_sha256'] == varint_freeze['delta_sha256'] == varint_conservation['pins']['delta_sha256']
    assert len(raw_varint_patch) == varint_patch['raw_bytes'] == varint_conservation['pins']['delta_bytes'] == 1099
    assert varint_committed['kind'] == 'AUTHOR_EXACT_COMMITTED_VARINT_FIXTURE_BINDING_NOT_WHOLE_HEAD_APPROVAL'
    assert varint_committed['head'] == varint_publication['head'] == plan['product_head']
    assert varint_committed['tree'] == varint_publication['tree'] == plan['product_tree']
    assert varint_committed['parent'] == varint_publication['parent'] == review['parent']
    assert varint_committed['delta_sha256'] == varint_patch['raw_sha256']
    assert varint_committed['changed_paths'] == host9['source']['only_changed_paths_from_parent'] == [leaf]
    assert varint_committed['current_file_sha256'] == varint_publication['source_sha256'] == host9_hashes[leaf]
    assert varint_committed['exact_one_block_inverse_equals_parent'] is True and varint_committed['all_other_entries_and_modes_equal'] is True
    assert varint_committed['conserved_other_entries'] == 2222 and varint_committed['source_pins'] == host9['source']['pins']
    assert varint_publication['semantic_review_sha256'] == review['varint_semantic_review']['sha256']
    assert varint_publication['conservation_review_sha256'] == review['varint_conservation_review']['sha256']
    assert varint_publication['pr'] == 1239 and varint_publication['public_state'] == 'OPEN Draft'
    assert varint_publication['normal_push_original_exit'] == 0 and varint_publication['Rust_compile_tests_native'] == 'UNRUN'
    assert varint_publication['wire127_migration'] == 'HOLD unchanged; no allowlist widening/current installed inference'
    assert [group['id'] for group in plan['artifact_groups']] == ['host-lib']
    group = plan['artifact_groups'][0]
    assert group['features'] == ['host']
    assert [contract['name'] for contract in group['selectors']] == host9_names
    assert group['source_predicted_compiled_names'] == host9_names and group['source_predicted_absent_names'] == []
    assert all(contract['expected_stdout_child_results'] == 0 and contract['seconds'] == 90
               and not contract.get('staged_child_observation') for contract in group['selectors'])
    assert SCOPE_PARENT is None
    assert len(plan['binary_checks']) == 1
    assert plan['binary_checks'][0]['argv'] == ['cargo', *host9['proposed_serial_commands'][0]['argv']]
    assert group['compile_argv'] == ['cargo', *host9['proposed_serial_commands'][1]['argv']]
    assert plan['binary_checks'][0]['seconds'] == 600 and group['compile_seconds'] == 900 and group['inventory_seconds'] == 30
    assert plan['planned_phase_order'] == ['host-binary-check', 'compile-host-lib', 'inventory-host-lib'] + ['parent-host-lib-' + str(index) for index in range(1, 10)]
    assert plan['lane']['target'] == str(TARGET) and plan['lane']['lock'] == str(LOCK)
    assert plan['lane']['jobs'] == 2 and plan['lane']['new_cache'] is False
    assert plan['packet_deadline_seconds'] == 2400 and plan['planned_phase_count'] == 12
    assert plan['counts']['qualifying_outer_parents'] == 9
    controller_path = Path(args.controller_review)
    assert controller_path.is_absolute() and not controller_path.is_symlink() and controller_path.resolve(strict=True) == controller_path
    controller_bytes = controller_path.read_bytes()
    assert re.fullmatch(r'[0-9a-f]{64}', args.controller_review_sha256)
    assert hashlib.sha256(controller_bytes).hexdigest() == args.controller_review_sha256
    controller_review = validate_controller_review(controller_bytes, plan['product_head'], plan['product_tree'], args.runner_sha256, PLAN_SHA)
    archive_bytes = []
    for binding in plan['archive_bindings']:
        payload = Path(binding['path']).read_bytes()
        assert hashlib.sha256(payload).hexdigest() == binding['sha256']
        archive_bytes.append((binding, payload))
    assert TARGET.is_dir() and not TARGET.is_symlink()
    for name, expected_sha in plan['tool_sha256'].items():
        path = TOOL / name
        assert path.is_file() and not path.is_symlink() and digest(path) == expected_sha
    out = Path('/tmp/coord-b9-pr1239-73487c8d-host9-validation-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '-' + str(os.getpid()))
    out.mkdir(mode=0o700)
    save = lambda name, value: (out / name).write_text(json.dumps(value, indent=2) + '\n')
    # Custody and original bytes are archived before touching the lane lock.
    evidence_copy(out, 'executed-runner.py', runner_bytes, args.runner_sha256)
    evidence_copy(out, 'plan.json', plan_bytes, PLAN_SHA)
    for binding, payload in archive_bytes:
        evidence_copy(out, binding['name'], payload, binding['sha256'])
    evidence_copy(out, 'controller-review.json', controller_bytes, args.controller_review_sha256)
    save('controller-before-lease.json', {'grant': args.grant, 'runner_path': str(runner),
         'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA,
         'review_sha256': plan['source_review_sha256'], 'controller_review_path': str(controller_path),
         'controller_review_sha256': args.controller_review_sha256, 'head': plan['product_head'], 'tree': plan['product_tree'],
         'argv': sys.argv, 'execution': 'Original runner/plan/review bytes retained before lock open/flock; no historical provenance transfer.', 'at': now()})
    env = os.environ.copy()
    overrides = {'CARGO_TARGET_DIR': str(TARGET), 'CARGO_BUILD_JOBS': '2', 'RUSTC': str(TOOL / 'rustc'),
                 'RUSTDOC': str(TOOL / 'rustdoc'), 'RUSTUP_TOOLCHAIN': '1.93.0', 'RUSTUP_AUTO_INSTALL': '0',
                 'PATH': str(TOOL) + os.pathsep + env['PATH']}
    env.update(overrides)
    # Install cooperative retention before any source child or C acquisition.
    signal.signal(signal.SIGINT, note_lane_cancellation)
    signal.signal(signal.SIGTERM, note_lane_cancellation)
    phases, tests = [], []
    error = None
    acquired = False
    lock_fd = None
    lock_closed = False
    lock_unlocked = False
    lock_close_error = None
    deadline = None

    def source(absolute_deadline=None):
        if deadline is not None:
            absolute_deadline = deadline if absolute_deadline is None else min(deadline, absolute_deadline)
        guard_git_metadata(absolute_deadline)
        value = {'head': git('rev-parse', 'HEAD', absolute_deadline=absolute_deadline),
                 'tree': git('rev-parse', 'HEAD^{tree}', absolute_deadline=absolute_deadline),
                 'status': git('status', '--porcelain=v1', '--untracked-files=all', absolute_deadline=absolute_deadline),
                 'hashes': {path: digest(WT / path) for path in plan['source_paths_sha256']}}
        assert value == {'head': plan['product_head'], 'tree': plan['product_tree'], 'status': '',
                         'hashes': plan['source_paths_sha256']}, 'Source/head/tree/clean-status changed'
        assert digest(runner) == args.runner_sha256 and digest(PLAN) == PLAN_SHA
        assert digest(plan['source_review']) == plan['source_review_sha256']
        assert digest(controller_path) == args.controller_review_sha256
        assert all(digest(binding['path']) == binding['sha256'] for binding in plan['archive_bindings'])
        assert {name: digest(TOOL / name) for name in plan['tool_sha256']} == plan['tool_sha256'], 'Physical tool bytes changed'
        return value

    def context():
        candidates = {path / '.cargo' / name for path in [WT, *WT.parents] for name in ('config', 'config.toml')}
        cargo_home = Path(env.get('CARGO_HOME', str(Path(env['HOME']) / '.cargo')))
        candidates.update(cargo_home / name for name in ('config', 'config.toml'))
        configs = {}
        for path in sorted(candidates):
            try:
                info = path.lstat()
            except FileNotFoundError:
                configs[str(path)] = {'present': False}
                continue
            assert path.is_file()
            configs[str(path)] = {'present': True, 'sha256': digest(path), 'mode': info.st_mode,
                                  'symlink': os.readlink(path) if path.is_symlink() else None}
        keys = {'PATH', 'HOME', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'RUSTC', 'RUSTDOC',
                'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
                'RUSTDOCFLAGS', 'CARGO_ENCODED_RUSTDOCFLAGS', 'CC', 'CXX', 'AR', 'LD', 'LD_LIBRARY_PATH',
                'PKG_CONFIG_PATH', 'PKG_CONFIG_LIBDIR', 'PKG_CONFIG_SYSROOT_DIR'}
        hashed = {key: hashlib.sha256(value.encode()).hexdigest() for key, value in env.items()
                  if key in keys or key.startswith(('CARGO_BUILD_', 'CARGO_TARGET_', 'CARGO_PROFILE_', 'CC_', 'CXX_', 'AR_'))}
        return {'configs': configs, 'build_input_hashes': hashed,
                'inherited_cagents_names': sorted(key for key in env if key.startswith('CAGENTS_'))}

    assert ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0, 'Dedicated subreaper required before any Git/check/test process'
    try:
        before, before_context = source(), context()
    except Exception as prelease_error:
        try:
            prelease_custody = settle_children()
        except Exception as custody_error:
            prelease_custody = {'clean': False, 'forced': False, 'inspection_error': type(custody_error).__name__ + ': ' + str(custody_error)}
        save('prelease-source-failure.json', {'error': type(prelease_error).__name__ + ': ' + str(prelease_error),
             'custody': prelease_custody, 'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES,
             'supervision_failures': SUPERVISION_FAILURES, 'lock_acquired': False, 'qualified': False, 'at': now()})
        raise
    save('source-before.json', before)
    save('context-before.json', before_context)

    def run(label, argv, seconds, expected_parent_contract=None):
        require_workload_allowed()
        started = time.monotonic()
        phase_deadline = min(deadline, started + seconds)
        assert source(absolute_deadline=phase_deadline) == before and context() == before_context
        d = out / label
        d.mkdir()
        save(label + '/command.json', {'argv': argv, 'cwd': str(WT), 'started': now(),
             'policy': 'Inherited opaque authority untouched; compiler overrides only',
             'compiler_overrides': {key: value for key, value in overrides.items() if key != 'PATH'},
             'timeout_seconds': seconds, 'packet_deadline_seconds': plan['packet_deadline_seconds'],
             'expected_parent_contract': expected_parent_contract})
        print(json.dumps({'phase': label, 'state': 'STARTED', 'at': now()}), flush=True)
        expired, code, process_error, state, failure = False, None, None, None, None
        child_fd = None
        launch_requested = False
        try:
            with (d / 'stdout.log').open('wb') as stdout, (d / 'stderr.log').open('wb') as stderr:
                remaining = phase_deadline - time.monotonic()
                assert remaining > 0, 'Absolute packet/phase deadline exhausted before launch; no child starts'
                require_workload_allowed()
                launch_requested = True
                child = subprocess.Popen(argv, cwd=WT, env=env, stdout=stdout, stderr=stderr,
                                         close_fds=True, start_new_session=True)
                PROCESS_ATTEMPTS.append(('phase', label, tuple(argv), child.pid))
                state = {'stage': 'phase', 'label': label, 'pid': child.pid, 'exit': None,
                         'original_wait_complete': False, 'direct_cleanup_attempted': False,
                         'supervision_error': None, 'evidence_complete': False, 'custody': None}
                PROCESS_STATES.append(state)
                try:
                    child_fd = os.pidfd_open(child.pid)
                    try:
                        remaining = phase_deadline - time.monotonic()
                        if remaining <= 0:
                            raise subprocess.TimeoutExpired(argv, 0)
                        code = observed_wait(state, child, remaining)
                    except subprocess.TimeoutExpired:
                        expired = True
                        state['direct_cleanup_attempted'] = True
                        signal_pidfd(child_fd, signal.SIGTERM)
                        try:
                            code = observed_wait(state, child, 3)
                        except subprocess.TimeoutExpired:
                            signal_pidfd(child_fd, signal.SIGKILL)
                            try:
                                code = observed_wait(state, child, 3)
                            except subprocess.TimeoutExpired:
                                pass  # Unknown original exit stays null; no poll/reaper inference.
                finally:
                    if child_fd is not None:
                        os.close(child_fd)
        except Exception as exc:
            failure = exc
            if launch_requested:
                process_error = supervision_failure(state, exc, 'phase-supervision')
        if state is None:
            if failure is not None:
                raise failure
            raise AssertionError('No successful phase spawn')
        try:
            custody = settle_children()
        except Exception as exc:
            custody = {'clean': False, 'forced': False, 'inspection_error': type(exc).__name__ + ': ' + str(exc)}
        state['custody'] = custody
        record = {'label': label, 'argv': argv, 'pid': state['pid'], 'exit': state['exit'],
                  'direct_wait_complete': state['original_wait_complete'], 'timed_out': expired,
                  'custody': custody, 'process_error': process_error,
                  'seconds': None, 'finished': None, 'stdout_sha256': None, 'stderr_sha256': None}
        phases.append(record)
        try:
            record.update(seconds=time.monotonic() - started, finished=now(),
                          stdout_sha256=digest(d / 'stdout.log'), stderr_sha256=digest(d / 'stderr.log'))
            (d / 'exit.txt').write_text(str(state['exit']) + '\n')
            save(label + '/result.json', record)
            state['evidence_complete'] = True
        except Exception as exc:
            record['evidence_error'] = supervision_failure(state, exc, 'phase-evidence')
            raise
        print(json.dumps({'phase': label, 'exit': state['exit'], 'seconds': record['seconds'], 'custody': custody}), flush=True)
        if failure is not None:
            raise failure
        assert source(absolute_deadline=phase_deadline) == before and context() == before_context
        assert process_error is None and state['original_wait_complete'] and not expired and custody_clean(custody), 'Uncertain phase custody/timeout/process control'
        if state['exit'] != 0:
            raise RuntimeError('Actual nonzero phase exit: ' + label + ': ' + str(state['exit']))
        return d

    try:
        require_workload_allowed()
        lock_fd = os.open(LOCK, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
        info = os.fstat(lock_fd)
        assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
        require_workload_allowed()
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        acquired = True
        deadline = time.monotonic() + plan['packet_deadline_seconds']
        save('lease.json', {'grant': args.grant, 'lane': 'C', 'target': str(TARGET), 'lock': str(LOCK),
             'nonblocking': True, 'device': info.st_dev, 'inode': info.st_ino, 'pid': os.getpid(), 'at': now()})
        save('tools.json', {'physical_paths': {name: str(TOOL / name) for name in plan['tool_sha256']},
                            'sha256': plan['tool_sha256'], 'version_processes': 'Not launched; physical1.93 paths and exact bytes pinned.'})
        print(json.dumps({'state': 'LEASE_ACQUIRED', 'evidence': str(out), 'at': now()}), flush=True)
        artifacts = execute_contracts(plan, run, save, tests)
    except Exception as exc:
        error = type(exc).__name__ + ': ' + str(exc)
    finally:
        try:
            final_errors, custody = final_observations(source, context, before, before_context, save)
            if final_errors:
                error = (error or '') + '; ' + '; '.join(final_errors)
            accounting, raw, raw_errors = parent_accounting(PROCESS_ATTEMPTS, PROCESS_STATES, phases, tests, out)
            for raw_error in raw_errors:
                supervision_failure(None, RuntimeError(raw_error), 'raw-parent-evidence')
                error = (error or '') + '; raw parent evidence: ' + raw_error
            historical_handoff_clean = safe_lane_handoff(
                PROCESS_ATTEMPTS, PROCESS_STATES, phases, custody, SUPERVISION_FAILURES)
        except BaseException as finalization_error:
            if acquired:
                retain_uncertain_lane(save, {
                    'stage': 'handoff-assessment', 'error_type': type(finalization_error).__name__,
                    'lock_fd': lock_fd, 'close_attempted': False,
                    'attempts': PROCESS_ATTEMPTS, 'states': PROCESS_STATES,
                    'phases': phases, 'supervision_failures': SUPERVISION_FAILURES})
            raise
        if acquired and not historical_handoff_clean:
            retain_uncertain_lane(save, {
                'stage': 'positive-handoff-unconfirmed', 'original_error': error,
                'lock_fd': lock_fd, 'close_attempted': False,
                'attempts': PROCESS_ATTEMPTS, 'states': PROCESS_STATES,
                'phases': phases, 'raw_parent_results': raw,
                'custody': custody, 'supervision_failures': SUPERVISION_FAILURES})
        if lock_fd is not None:
            try:
                # No prior LOCK_UN: only this confirmed close releases the lease.
                # All original wait/evidence/family predicates were checked first.
                os.close(lock_fd)
                lock_closed = True
            except BaseException as close_error:
                lock_close_error = type(close_error).__name__ + ': ' + str(close_error)
                if acquired:
                    retain_uncertain_lane(save, {
                        'stage': 'original-Cfd-close-unconfirmed', 'error': lock_close_error,
                        'lock_fd': lock_fd, 'close_attempted': True,
                        'physical_lock_disposition': 'UNKNOWN_AFTER_CLOSE_ATTEMPT',
                        'custody': custody, 'attempts': PROCESS_ATTEMPTS,
                        'states': PROCESS_STATES, 'supervision_failures': SUPERVISION_FAILURES})
                raise
        actual_release = acquired and lock_closed and lock_close_error is None and historical_handoff_clean
        summary = {'head': plan['product_head'], 'tree': plan['product_tree'], 'grant': args.grant,
             'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA, 'source_review_sha256': plan['source_review_sha256'],
             'controller_review_sha256': args.controller_review_sha256,
             'error': error, 'phases': phases, 'parents': tests, **accounting, 'raw_parent_results': raw,
             'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES, 'supervision_failures': SUPERVISION_FAILURES,
             'cancellation_signals': CANCELLATION_SIGNALS,
             'custody': custody, 'historical_handoff_clean': historical_handoff_clean,
             'lock_closed': lock_closed, 'lock_unlocked': lock_unlocked, 'lock_close_error': lock_close_error,
             'actual_release': actual_release,
             'release_policy': 'Original Cfd closes only after complete original waits/evidence and clean unforced phase plus both final custody observations. No LOCK_UN is issued. Unknown assessment/handoff or close retains no safe-return claim; ambiguous close is never retried.',
             'owning_commands_qualified': error is None and not CANCELLATION_SIGNALS and len(tests) == 9 and [phase['label'] for phase in phases] == plan['planned_phase_order'] and actual_release,
             'scope_observations': [row['scope_observation'] for row in raw if 'scope_observation' in row],
             'scope_isolation': next((row['scope_observation']['scope_isolation'] for row in raw if 'scope_observation' in row), 'UNKNOWN_UNQUALIFIED'),
             'adoption_close': next((row['scope_observation']['adoption_close'] for row in raw if 'scope_observation' in row), 'UNKNOWN_UNQUALIFIED'),
             'native_provider_UI_global_qualification': 'NOT_ESTABLISHED_BY_THIS_PACKET',
             'nonadditive_child_result_count': sum(len(parent['nonadditive_child_results']) for parent in tests),
             'finished': now(), 'limits': plan['limits']}
        save('summary.json', summary)
        files = sorted(path for path in out.rglob('*') if path.is_file())
        (out / 'SHA256SUMS').write_text(''.join(digest(path) + '  ' + str(path.relative_to(out)) + '\n' for path in files))
        # Root owns coordination; this runner never sends a message or starts another agent.
        print(json.dumps(summary), flush=True)
        print('EVIDENCE ' + str(out), flush=True)
        print('RELEASE_STATUS ' + ('ACTUAL_CLEAN_C_RELEASE' if summary['actual_release'] else 'NO_SAFE_RELEASE_CLAIM'), flush=True)
    return 0 if summary['owning_commands_qualified'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
