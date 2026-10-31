"""Prepared eb7 startup packet A: exactly3 isolated binary checks; independent review and fresh grant required. No tests/native qualification."""
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

WT = Path('/home/marvin/Documents/repositories/cagents/.worktrees/fw16-startup-recovery-composition')
TARGET = Path('/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-target')
LOCK = TARGET.parent / 'lane-c.lock'
TOOL = Path('/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin')
PLAN = Path('/home/marvin/Documents/repositories/cagents/.worktrees/coord-b9-startup-recovery-checks/qualification-packets/startup-recovery-checks/plan.json')
PROCESS_ATTEMPTS = []  # Append-only immutable successful-spawn tuples; never inferred from evidence files.
PROCESS_STATES = []    # Separate original-wait and evidence observations; null stays unknown.
SUPERVISION_FAILURES = []  # Sticky history, including failures after an otherwise successful wait.
CANCELLATION_SIGNALS = []  # Cooperative cancellation, never cleared by cleanup.
PLAN_SHA = 'f53a1415715da6e109a905cf309be7fb2e387818b92f4120767ecb0c4fb9fc0e'
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


def binary_check_pass(stdout, stderr):
    """Check-only output contract; original wait0 is separately mandatory."""
    text = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', stdout + '\n' + stderr)
    lines = text.splitlines()
    finished = [line for line in lines if line.lstrip().startswith('Finished ')]
    assert len(finished) == 1 and re.fullmatch(r'\s*Finished `dev` profile(?: .*)?', finished[0]), 'Missing/extra/wrong Cargo check completion'
    assert not any(line.lstrip().startswith(('error:', 'error[', 'test result:', 'running ', 'test ', 'Running unittests')) for line in lines), 'Check emitted compiler error or unexpected test result'
    return {'finished_dev_marker': finished[0], 'check_output_contract': True,
            'scope': 'Binary-check output only; zero test/artifact/inventory qualification'}


def execute_contracts(plan, invoke, save, checks):
    assert plan['job_kind'] == 'A_CHECKS_ONLY' and plan['artifact_groups'] == []
    assert plan['planned_phase_count'] == 3 and len(plan['binary_checks']) == 3
    for check in plan['binary_checks']:
        argv = [str(TOOL / 'cargo'), *check['argv'][1:]]
        d = invoke(check['label'], argv, check['seconds'])
        proof = binary_check_pass((d / 'stdout.log').read_text(), (d / 'stderr.log').read_text())
        record = {'label': check['label'], 'argv': argv, 'scope': 'isolated binary check candidate only', **proof}
        save(check['label'] + '/check-output.json', record)
        checks.append(record)
    assert [check['label'] for check in checks] == plan['planned_phase_order']
    save('binary-checks.json', checks)


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


def check_accounting(attempts, states, phases, checks, out):
    labels = ('cli-binary-check', 'host-binary-check', 'ui-binary-check')
    phase_attempts = [attempt for attempt in attempts if attempt[0] == 'phase']
    assert [attempt[1] for attempt in phase_attempts] == list(labels[:len(phase_attempts)])
    assert 0 <= len(checks) <= len(phase_attempts) <= 3
    raw, errors = [], []
    for stage, label, argv, pid in phase_attempts:
        state = next((state for state in states if state['stage'] == stage and state['label'] == label and state['pid'] == pid), None)
        phase = next((phase for phase in phases if phase['label'] == label and phase['pid'] == pid), None)
        row = {'label': label, 'pid': pid, 'exit': state['exit'] if state else None,
               'original_wait_complete': bool(state and state['original_wait_complete']),
               'phase_recorded': phase is not None, 'evidence_complete': bool(state and state['evidence_complete']),
               'supervision_error': state['supervision_error'] if state else 'Spawn has no supervision state',
               'output_read_error': None, 'output_parse_error': None, 'qualified_check_candidate': False}
        try:
            stdout = (out / label / 'stdout.log').read_text()
            stderr = (out / label / 'stderr.log').read_text()
            try:
                row['output_proof'] = binary_check_pass(stdout, stderr)
                row['qualified_check_candidate'] = bool(state and state['original_wait_complete'] and state['exit'] == 0
                    and state['evidence_complete'] and not state['supervision_error'] and not state['direct_cleanup_attempted']
                    and custody_clean(state['custody']) and phase and not phase['timed_out'] and phase['process_error'] is None
                    and any(check['label'] == label and check['argv'] == list(argv) for check in checks))
            except AssertionError as exc:
                row['output_parse_error'] = type(exc).__name__ + ': ' + str(exc)
        except Exception as exc:
            row['output_read_error'] = type(exc).__name__ + ': ' + str(exc)
            errors.append(row['output_read_error'])
        raw.append(row)
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
             'unknown_phase_count': len(phase_attempts) - settled_phases, 'unrun_phase_count': 3 - len(phase_attempts),
             'attempted_source_check_count': len(source_attempts),
             'original_wait_settled_source_check_count': settled_sources,
             'unknown_source_check_count': len(source_attempts) - settled_sources,
             'all_original_waits_complete': len(attempts) == len(states) and all(state['original_wait_complete'] for state in states),
             'requested_binary_check_count': 3, 'qualified_binary_check_candidate_count': sum(row['qualified_check_candidate'] for row in raw),
             'attempted_binary_check_count': len(phase_attempts), 'unrun_binary_check_count': 3 - len(phase_attempts),
             'nonqualifying_attempted_binary_check_count': len(phase_attempts) - sum(row['qualified_check_candidate'] for row in raw),
             'declared_test_invocations': 0, 'declared_test_parents': 0, 'declared_ELF_inventories': 0}, raw, errors)


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
    assert review['kind'] == 'AUTHOR_EB7_PACKET_A_SOURCE_BINDING_NOT_INDEPENDENT_APPROVAL'
    assert review['verdict'] == 'BOUND_EXACT_STARTUP_SOURCE_CHAIN_ONLY_NO_WHOLE_HEAD_OR_RUNTIME_APPROVAL'
    assert review['job_kind'] == plan['job_kind'] == 'A_CHECKS_ONLY'
    def read_bound(key, as_json=True):
        ref = review[key]
        payload = Path(ref['path']).read_bytes()
        assert hashlib.sha256(payload).hexdigest() == ref['sha256'], 'Changed source/report input: ' + key
        return json.loads(payload) if as_json else payload.decode('utf-8')
    data = read_bound('command_data')
    preflight = read_bound('source_preflight')
    transport = read_bound('transport_review')
    composition = read_bound('composition_freeze')
    publication = read_bound('eb7_publication')
    b914_publication = read_bound('b914_publication')
    design = read_bound('design', False)
    read_bound('prior_design', False)
    donor_acceptance = read_bound('donor_acceptance')
    donor_review = read_bound('donor_controller_review')
    read_bound('donor_runner', False)
    read_bound('donor_plan')
    read_bound('donor_handoff')
    read_bound('donor_source_receipt')
    read_bound('transport_freeze')
    for ref in review['composition_reviews']:
        payload = Path(ref['path']).read_bytes()
        assert hashlib.sha256(payload).hexdigest() == ref['sha256']
    assert data['schema'] == 'eb7-startup-recovery-packet-A-three-isolated-binary-checks-v1'
    assert data['kind'] == 'FIXED_SOURCE_DATA_ONLY_NO_EXECUTION_OR_GRANT'
    assert data['head'] == review['head'] == plan['product_head'] == 'eb7ff16beb8864349573231521c5d4eab5270cc6'
    assert data['tree'] == review['tree'] == plan['product_tree'] == '8d60fb0154109a82861f9b595f2b7296e4f55c26'
    assert data['parent'] == review['parent'] == plan['product_parent'] == 'b914f9d9b55527aa1bb22d591968c58958d717ee'
    assert data['product_cwd'] == str(WT)
    assert data['job_kind'] == 'A_CHECKS_ONLY' and data['artifact_groups'] == plan['artifact_groups'] == []
    assert data['binary_checks'] == plan['binary_checks'] == [
        {'label': 'cli-binary-check', 'argv': ['cargo', 'check', '--locked', '--no-default-features', '--bin', 'cagents'], 'seconds': 600},
        {'label': 'host-binary-check', 'argv': ['cargo', 'check', '--locked', '--no-default-features', '--features', 'host', '--bin', 'cagentsd'], 'seconds': 600},
        {'label': 'ui-binary-check', 'argv': ['cargo', 'check', '--locked', '--no-default-features', '--features', 'ui', '--bin', 'cagents-ui'], 'seconds': 900}]
    assert data['counts'] == review['counts'] == plan['counts'] == {'checks': 3, 'lib_compilations': 0, 'ELF_inventories': 0, 'test_invocations': 0, 'qualifying_outer_parents': 0, 'test_groups': 0, 'total_phases': 3}
    assert data['caps_override']['phase_caps_seconds'] == [600, 600, 900]
    assert data['caps_override']['sum_caps_seconds'] == 2100 and data['caps_override']['remaining_overhead_seconds'] == 300
    assert data['caps_override']['authority_reference'] == 'EC53a57eea as relayed by Root task'
    assert data['packet_deadline_seconds'] == plan['packet_deadline_seconds'] == 2400
    assert plan['planned_phase_count'] == 3 and plan['planned_phase_order'] == ['cli-binary-check', 'host-binary-check', 'ui-binary-check']
    assert preflight['product_head'] == publication['head'] == plan['product_head']
    assert preflight['product_tree'] == publication['tree'] == plan['product_tree']
    assert preflight['source_paths_sha256'] == review['source_paths_sha256'] == plan['source_paths_sha256']
    assert plan['source_paths_sha256']['src/control.rs'] == '5ffc695ec8029d10808e05647d5bd106ff51e2e0b728b9a3b6ed3f2404e410c1'
    assert transport['result'] == 'SOURCE_CLEAR_BOUNDED_SHARED_TRANSPORT_TEST' and transport['blocking_findings'] == []
    assert publication['source_review_sha256'] == review['transport_review']['sha256']
    assert publication['parent'] == b914_publication['head'] == data['parent']
    assert composition['schema'] == 'fw16-startup-recovery-composition-source-v1'
    assert all(name == 'src/control.rs' or plan['source_paths_sha256'][name] == spec['sha256'] for name, spec in composition['files'].items())
    assert 'Job A: three isolated binary checks' in design
    assert donor_acceptance['decision'] == 'CLOSED_ACCEPTED_BOUNDED_EXACT_73487_HOST9_PASS'
    assert donor_acceptance['source_controller']['runner_sha256'] == donor_review['runner_sha256'] == review['donor_runner']['sha256'] == '71b1e67c73d075891c9b5c48a8fe6e53a4168bb2b86365bf23287b4590cf2829'
    assert donor_acceptance['source_controller']['plan_sha256'] == donor_review['plan_sha256'] == review['donor_plan']['sha256']
    assert SCOPE_PARENT is None and data['actual_grant'] is None
    assert plan['lane']['target'] == str(TARGET) and plan['lane']['lock'] == str(LOCK)
    assert plan['lane']['jobs'] == 2 and plan['lane']['new_cache'] is False
    assert plan['physical_tool_bin'] == str(TOOL)
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
    out = Path('/tmp/coord-b9-startup-recovery-checks-validation-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '-' + str(os.getpid()))
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
    phases, checks = [], []
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
        execute_contracts(plan, run, save, checks)
    except Exception as exc:
        error = type(exc).__name__ + ': ' + str(exc)
    finally:
        try:
            final_errors, custody = final_observations(source, context, before, before_context, save)
            if final_errors:
                error = (error or '') + '; ' + '; '.join(final_errors)
            accounting, raw, raw_errors = check_accounting(PROCESS_ATTEMPTS, PROCESS_STATES, phases, checks, out)
            for raw_error in raw_errors:
                supervision_failure(None, RuntimeError(raw_error), 'raw-check-evidence')
                error = (error or '') + '; raw check evidence: ' + raw_error
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
                'phases': phases, 'raw_check_results': raw,
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
             'error': error, 'phases': phases, 'binary_checks': checks, **accounting, 'raw_check_results': raw,
             'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES, 'supervision_failures': SUPERVISION_FAILURES,
             'cancellation_signals': CANCELLATION_SIGNALS,
             'custody': custody, 'historical_handoff_clean': historical_handoff_clean,
             'lock_closed': lock_closed, 'lock_unlocked': lock_unlocked, 'lock_close_error': lock_close_error,
             'actual_release': actual_release,
             'release_policy': 'Original Cfd closes only after complete original waits/evidence and clean unforced phase plus both final custody observations. No LOCK_UN is issued. Unknown assessment/handoff or close retains no safe-return claim; ambiguous close is never retried.',
             'owning_commands_qualified': error is None and not CANCELLATION_SIGNALS and len(checks) == 3 and accounting['qualified_binary_check_candidate_count'] == 3 and [phase['label'] for phase in phases] == plan['planned_phase_order'] and actual_release,
             'qualification_scope': plan['qualification_scope'],
             'native_provider_UI_global_qualification': 'NOT_ESTABLISHED_BY_THIS_PACKET',
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
