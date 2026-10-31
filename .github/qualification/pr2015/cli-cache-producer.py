"""Prepared separate PR1921 hosted User CLI cache producer; no installer or test execution. No local authority stripping or execution during source preparation."""
import argparse
import ctypes
import contextlib
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

CONTROL = Path(__file__).resolve().parents[3]
WT = TARGET = LOCK = TOOL = None  # Actual hosted paths come only from admitted runtime metadata.
PLAN = CONTROL / '.github/qualification/pr2015/cli-cache-producer-plan.json'
PAYLOAD_DEADLINE = None
EVIDENCE_OUT = None
OWNERSHIP_INITIALIZED = False
PROCESS_PHASES = []
TOOL_CAPTURES = []
PROCESS_ATTEMPTS = []  # Append-only immutable successful-spawn tuples; never inferred from evidence files.
PROCESS_STATES = []    # Separate original-wait and evidence observations; null stays unknown.
SUPERVISION_FAILURES = []  # Sticky history, including failures after an otherwise successful wait.
CANCELLATION_SIGNALS = []  # Cooperative cancellation, never cleared by cleanup.
PLAN_SHA = 'c39055932062af3961999942c15df40239a761d2373472cd173a743d6c190e85'


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


def bounded_capture(argv, absolute_deadline=None, *, raw=False):
    require_workload_allowed()
    local_deadline = time.monotonic() + 10
    if absolute_deadline is not None:
        local_deadline = min(local_deadline, absolute_deadline)
    code, expired, child_fd, state = None, False, None, None
    launch_requested = False
    capture = None
    try:
        with contextlib.ExitStack() as stack:
            if raw:
                assert EVIDENCE_OUT is not None
                number = len(TOOL_CAPTURES) + 1
                prefix = 'tool-capture-' + str(number)
                capture = {'id': number, 'argv': list(argv), 'pid': None, 'exit': None,
                           'original_wait_complete': False, 'channels_complete': False,
                           'stdout_path': str(EVIDENCE_OUT / (prefix + '.stdout')),
                           'stderr_path': str(EVIDENCE_OUT / (prefix + '.stderr'))}
                TOOL_CAPTURES.append(capture)
                (EVIDENCE_OUT / (prefix + '-request.json')).write_text(json.dumps(capture, indent=2) + '\n')
                stdout = stack.enter_context(Path(capture['stdout_path']).open('x+b'))
                stderr = stack.enter_context(Path(capture['stderr_path']).open('x+b'))
            else:
                stdout = stack.enter_context(tempfile.TemporaryFile())
                stderr = stack.enter_context(tempfile.TemporaryFile())
            remaining = local_deadline - time.monotonic()
            assert remaining > 0, 'Source/tool budget exhausted before launch'
            require_workload_allowed()
            launch_requested = True
            child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, close_fds=True, start_new_session=True)
            PROCESS_ATTEMPTS.append(('source', None, tuple(argv), child.pid))
            state = {'stage': 'source', 'label': None, 'pid': child.pid, 'exit': None,
                     'original_wait_complete': False, 'direct_cleanup_attempted': False,
                     'supervision_error': None, 'evidence_complete': False, 'custody': None}
            PROCESS_STATES.append(state)
            if capture is not None:
                capture['pid'] = child.pid
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
            # Original on-disk tool channels survive even a nonzero/partial result.
            stdout.seek(0)
            stderr.seek(0)
            output, errors = stdout.read(1024 * 1024 + 1), stderr.read(1024 * 1024 + 1)
            if capture is not None:
                capture.update(exit=state['exit'], original_wait_complete=state['original_wait_complete'],
                               timed_out=expired, channels_complete=False,
                               read_after_original_wait=state['original_wait_complete'] is True,
                               independent_writer_closure_proven=False,
                               stdout_observed_bytes=len(output), stderr_observed_bytes=len(errors))
                (EVIDENCE_OUT / (prefix + '-status.json')).write_text(json.dumps(capture, indent=2) + '\n')
            assert len(output) <= 1024 * 1024 and len(errors) <= 1024 * 1024, 'Source/tool output exceeded bound'
            assert code == 0 and not expired, ('Read-only source/tool failed/uncertain', code, expired)
            result = {'stdout': output.decode(), 'stderr': errors.decode(), 'exit': code,
                      'capture_id': capture['id'], 'stdout_path': capture['stdout_path'],
                      'stderr_path': capture['stderr_path']} if raw else output.decode().strip()
        state['evidence_complete'] = True
        return result
    except BaseException as exc:
        if launch_requested:
            supervision_failure(state, exc, 'source-tool' if raw else 'source')
        if capture is not None:
            capture.update(exit=state['exit'] if state else None,
                           original_wait_complete=bool(state and state['original_wait_complete']),
                           channels_complete=False, error=type(exc).__name__ + ': ' + str(exc))
            try:
                (EVIDENCE_OUT / (prefix + '-failure.json')).write_text(json.dumps(capture, indent=2) + '\n')
            except BaseException:
                pass
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
    # Current parentage and a freshly opened pidfd do not establish original
    # custody or permission to stop an adopted provider descendant. Leave
    # unproven survivors unsignaled; the caller must retain its C/HOLD state.
    return {'clean': False, 'forced': forced, 'observed': remaining, 'remaining': remaining}


def cargo_test_artifacts(text, manifest_path, expected_target):
    harnesses, clis, finished, root_executables = [], [], [], []
    for line in text.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'build-finished':
            finished.append(item.get('success'))
        if (item.get('reason') != 'compiler-artifact' or not item.get('executable')
                or Path(item.get('manifest_path', '')).resolve() != Path(manifest_path).resolve()):
            continue
        root_executables.append(item)
        target, profile = item.get('target', {}), item.get('profile', {})
        if target.get('kind') == ['test'] and target.get('name') == expected_target and profile.get('test') is True:
            harnesses.append(item)
        if target.get('kind') == ['bin'] and target.get('name') == 'cagents' and profile.get('test') is False:
            clis.append(item)
    assert finished == [True], 'Cargo did not report exactly one successful build-finished'
    assert len(harnesses) == 1 and len(clis) == 1 and len(root_executables) == 2, 'Exact integration harness/CLI artifacts missing or ambiguous'
    assert all(item.get('features') == [] for item in root_executables), 'Only isolated no-default-features CLI permitted'
    return harnesses[0], clis[0]


def elf_record(artifact):
    exe = Path(artifact['executable'])
    assert exe.is_absolute() and exe == exe.resolve(strict=True)
    assert exe.is_relative_to(TARGET.resolve(strict=True)) and not exe.is_symlink()
    before = exe.lstat()
    assert stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid()
    with exe.open('rb') as stream:
        assert stream.read(4) == b'\x7fELF'
    value = digest(exe)
    after = exe.lstat()
    fields = ('st_dev', 'st_ino', 'st_mode', 'st_uid', 'st_size', 'st_mtime_ns')
    identity = {name: getattr(before, name) for name in fields}
    assert identity == {name: getattr(after, name) for name in fields}, 'ELF identity changed during hash'
    return {'path': str(exe), 'sha256': value, 'file_identity': identity, 'features': artifact['features'],
            'artifact': artifact, 'limits': 'Original Cargo-reported ELF identity and digest guarded during successful use; full ELF bytes not archived.'}


def verify_elf(record):
    exe = Path(record['path'])
    assert exe.is_absolute() and exe == exe.resolve(strict=True) and not exe.is_symlink()
    before = exe.lstat()
    assert record['file_identity'] == {name: getattr(before, name) for name in record['file_identity']}, 'ELF identity changed'
    assert digest(exe) == record['sha256'], 'ELF bytes changed'
    after = exe.lstat()
    assert record['file_identity'] == {name: getattr(after, name) for name in record['file_identity']}, 'ELF identity changed during verification'


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
    assert not benchmarks, 'Integration inventory unexpectedly contains benchmarks'
    return actual


def parent_pass(text, contract):
    name = contract['name']
    assert contract['expected_child_reexecutions'] == 0, 'Nested fixture helpers are nonadditive, not outer libtest summaries'
    count, filtered = contract['actual_inventory_N'], contract['expected_filtered_out']
    assert count > 0 and filtered == count - 1, 'Fresh same-harness inventory required'
    expected_diagnostics = contract['expected_stdout_diagnostics']
    special = name == 'external_installer_saves_closes_and_reopens_windows_even_on_failure'
    assert expected_diagnostics == (['inspector-refused', 'inspector-missing'] if special else []), 'Diagnostic exception belongs only to exact source-pinned parent'
    events, summaries, diagnostics = [], [], []

    def diagnostic(line):
        found = re.fullmatch(r'source inspection (inspector-refused|inspector-missing): (.*)', line)
        assert special and found, 'Unexpected fixture diagnostic shape'
        diagnostics.append(found.group(1))

    for line in text.splitlines():
        if line.startswith('test result:'):
            found = re.fullmatch(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; 0 measured; (\d+) filtered out(?:; finished in \d+(?:\.\d+)?s)?', line)
            assert found and found.groups() == ('ok', '1', '0', '0', str(filtered)), 'Owning result must be exactly1P0F0I with actualN-1'
            summaries.append(found.groups())
            events.append('summary')
        elif line.lstrip().startswith('test result:'):
            raise AssertionError('Malformed summary-prefixed line')
        elif line.startswith('running '):
            assert line == 'running 1 test', 'Unexpected libtest run count/shape'
            events.append('run1')
        elif line.startswith('test '):
            found = re.fullmatch(r'test (\S+) \.\.\. (.*)', line)
            assert found and found.group(1) == name, 'Unexpected owning name or result shape'
            tail = found.group(2)
            if tail == 'ok':
                events.append('named-ok')
            elif tail == '' and special:
                events.append('named-prefix')
            elif special and tail.startswith('source inspection inspector-refused: '):
                events.append('named-prefix')
                diagnostic(tail)
            else:
                raise AssertionError('Failure/ignored/unexpected owning result')
        elif line.startswith('source inspection '):
            diagnostic(line)
        elif line == 'ok':
            assert special
            events.append('bare-ok')
        elif line == 'FAILED' or line.startswith('ignored'):
            raise AssertionError('Unexpected failed/ignored standalone result')
    expected = ['run1', 'named-prefix', 'bare-ok', 'summary'] if special else ['run1', 'named-ok', 'summary']
    assert events == expected and len(summaries) == 1, 'Missing/duplicate/interleaved owning result'
    assert diagnostics == expected_diagnostics, 'Missing/duplicate/out-of-order source refusal diagnostics'
    assert all(marker in text for marker in contract['required_diagnostic_markers']), 'Missing source-pinned recovery/cause diagnostic'
    return {'outer_passed': 1, 'outer_failed': 0, 'outer_ignored': 0, 'measured': 0,
            'actual_inventory_N': count, 'filtered_out': filtered, 'expected_child_reexecutions': 0,
            'nonadditive_child_results': [], 'source_derived_event_shape': events,
            'source_refusal_diagnostics': diagnostics}


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
    assert not parents
    check = plan['binary_checks'][0]
    invoke(check['label'], [str(TOOL / 'cargo'), *check['argv'][1:]], check['seconds'])
    artifacts = {}
    for group in plan['artifact_groups']:
        gid = group['id']
        compiled = invoke('compile-' + gid, [str(TOOL / 'cargo'), *group['compile_argv'][1:]], group['compile_seconds'])
        test_artifact, cli_artifact = cargo_test_artifacts((compiled / 'stdout.log').read_text(), WT / group['manifest'], group['target_name'])
        harness, cli = elf_record(test_artifact), elf_record(cli_artifact)
        artifacts[gid] = {'harness': harness, 'cli': cli, 'executed': False}
        save('compiled-elf-' + gid + '.json', artifacts[gid])
        verify_elf(harness)
        verify_elf(cli)
    save('compiled-elfs.json', artifacts)
    assert not parents
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
    assert not parents, 'Cache preparation cannot invoke or qualify a test parent'
    phase_attempts = [attempt for attempt in attempts if attempt[0] == 'phase']
    phase_states = [state for state in states if state['stage'] == 'phase']
    source_attempts = [attempt for attempt in attempts if attempt[0] == 'source']
    source_states = [state for state in states if state['stage'] == 'source']
    assert len(phase_attempts) <= 3
    assert not any(attempt[1].startswith(('parent-', 'inventory-')) for attempt in phase_attempts)
    settled_phases = sum(state['original_wait_complete'] for state in phase_states)
    settled_sources = sum(state['original_wait_complete'] for state in source_states)
    return ({'attempted_process_count': len(attempts),
             'original_wait_settled_process_count': settled_phases + settled_sources,
             'unknown_process_count': len(attempts) - settled_phases - settled_sources,
             'attempted_phase_count': len(phase_attempts), 'recorded_phase_count': len(phases),
             'original_wait_settled_phase_count': settled_phases,
             'unknown_phase_count': len(phase_attempts) - settled_phases,
             'unrun_phase_count': 3 - len(phase_attempts),
             'attempted_source_or_tool_capture_count': len(source_attempts),
             'original_wait_settled_source_or_tool_capture_count': settled_sources,
             'unknown_source_or_tool_capture_count': len(source_attempts) - settled_sources,
             'requested_tool_capture_count': len(TOOL_CAPTURES),
             'recorded_tool_spawn_count': sum(item['pid'] is not None for item in TOOL_CAPTURES),
             'recorded_tool_original_wait_count': sum(item['original_wait_complete'] is True for item in TOOL_CAPTURES),
             'all_original_waits_complete': len(attempts) == len(states) and all(state['original_wait_complete'] for state in states),
             'requested_parent_count': 0, 'passed_parent_count': 0,
             'attempted_parent_count': 0, 'executed_parent_count': 0,
             'original_wait_settled_parent_count': 0, 'unknown_parent_count': 0,
             'recorded_parent_count': 0, 'incomplete_parent_record_count': 0,
             'nonqualifying_parent_count': 0, 'unrun_parent_count': 0}, [], [])


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
    assert case['case'] == 'cli-cache-producer-2015' and case['never_merge'] is True
    assert attempt == '1' and event.get('action') == 'synchronize', 'Producer requires original same-PR synchronize attempt1; no rerun or dispatch'
    assert case['product_head'] == '375c37ff01aab0b226c58f547012af74e93a8643' and case['product_tree'] == 'acb2cc6e049562469643e49a1ee5f7abc179c28b'
    return {'repository': 'CarsonBurke/cagents', 'pr': 1921, 'event': 'pull_request',
            'head_ref': head['ref'], 'control_head': head['sha'], 'run_id': run, 'run_attempt': attempt,
            'run_binding': run + ':' + attempt + ':' + head['sha'],
            'workflow_ref': workflow_ref, 'workflow_sha': workflow_sha, 'github_sha': os.environ['GITHUB_SHA'],
            'runner_environment': 'github-hosted', 'runner_os': 'Linux', 'runner_arch': 'X64',
            'inherited_cagents_names': [], 'case_sha256': case_sha,
            'scope': 'Checked original engine/event metadata; actual authenticated hosted provenance still requires original GitHub run/job/log/artifact RAW readback.'}


def materialize_plan(template, hosted):
    global WT, TARGET, LOCK, TOOL
    WT = Path(os.environ['GITHUB_WORKSPACE']) / 'product'
    assert WT.is_dir() and not WT.is_symlink() and WT.resolve(strict=True) == WT
    temporary = Path(os.environ['RUNNER_TEMP'])
    assert temporary.is_absolute() and temporary.resolve(strict=True) == temporary
    evidence = temporary / 'issue2015-hosted-evidence'
    assert evidence.resolve(strict=True) == evidence and evidence.is_dir() and not evidence.is_symlink()
    LOCK = evidence / 'cache-producer-owned-custody.lock'
    TARGET = temporary / 'held-scope-target'
    assert os.environ.get('CARGO_TARGET_DIR') == str(TARGET)
    assert not os.path.lexists(TARGET), 'Producer requires a fresh exact job-local target; no removal or fallback'
    case = json.loads(Path(control_path('.github/qualification/pr2015/case.json')).read_bytes())
    assert case['runner_sha256'] == digest(Path(__file__)) and case['plan_sha256'] == PLAN_SHA
    assert digest(control_path(case['selector_path'])) == case['selector_sha256']
    tools, versions, parsed = producer_tools(time.monotonic() + 120)
    TOOL = Path(tools['cargo']['path']).parent
    plan = json.loads(json.dumps(template))
    plan['worktree'] = str(WT)
    plan['runner_path'] = control_path(plan['runner_path'])
    plan['source_review'] = control_path(plan['source_review'])
    plan['source_review_sha256'] = digest(plan['source_review'])
    plan['lane']['target'], plan['lane']['lock'] = str(TARGET), str(LOCK)
    plan['physical_tool_bin'] = str(TOOL)
    plan['tool_sha256'] = {name: item['sha256'] for name, item in tools.items()}
    plan['actual_tool_observations'] = {'physical_tools': tools, 'version_commands': versions, 'toolchains': parsed}
    for binding in plan['archive_bindings']:
        binding['path'] = control_path(binding['path'])
    for name, path in [('executed-research-workflow.yml', CONTROL / '.github/workflows/research-pr1853-bd88.yml'),
                       ('selected-case.json', CONTROL / '.github/qualification/pr2015/case.json'),
                       ('case-select.py', Path(control_path(case['selector_path'])))]:
        plan['archive_bindings'].append({'name': name, 'path': str(path), 'sha256': digest(path)})
    TARGET.mkdir(mode=0o700)
    info = TARGET.lstat()
    assert stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o700
    assert TARGET.resolve(strict=True) == TARGET and not TARGET.is_symlink()
    plan['created_target'] = {'path': str(TARGET), 'identity': identity(info),
                              'scope': 'Declared new producer output only; no restored-cache admission is fabricated.'}
    return plan


class GateFailure(RuntimeError):
    pass



def require(condition, message):
    if not condition:
        raise GateFailure(message)



def identity(info):
    return dict(device=info.st_dev, inode=info.st_ino, bytes=info.st_size,
                mode=stat.S_IMODE(info.st_mode), uid=info.st_uid,
                mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns)



def directory(path):
    payload_deadline()
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and not path.is_symlink(), 'Expected existing nonsymlink directory: ' + str(path))
    require(info.st_uid == os.getuid(), 'Cache directory owner changed: ' + str(path))
    return identity(info)



def observe(path, target, limit=None):
    payload_deadline()
    require(path.is_relative_to(target) and path != target, 'Cache marker escaped target')
    parent = path.parent
    while parent != target:
        payload_deadline()
        directory(parent)
        parent = parent.parent
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and not path.is_symlink() and info.st_uid == os.getuid(),
            'Expected owned regular cache artifact: ' + str(path))
    require(info.st_size > 0 and (limit is None or info.st_size <= limit), 'Empty or oversized cache marker: ' + str(path))
    value = hashlib.sha256()
    data = bytearray() if limit is not None else None
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        require(identity(os.fstat(stream.fileno())) == identity(info), 'Cache artifact changed before read')
        header = stream.read(16)
        value.update(header)
        if data is not None:
            data.extend(header)
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            payload_deadline()
            value.update(block)
            if data is not None:
                data.extend(block)
        payload_deadline()
        require(identity(os.fstat(stream.fileno())) == identity(info), 'Cache artifact changed during read')
    require(identity(path.lstat()) == identity(info), 'Cache artifact path changed during read')
    return {'path': str(path), 'identity': identity(info), 'sha256': value.hexdigest(),
            'header_hex': header.hex()}, bytes(data) if data is not None else header



def toolchains(rustc, cargo):
    result = {}
    for name, text in [('rustc', rustc), ('cargo', cargo)]:
        require(text.startswith(name + ' 1.93.0 ('), 'Active ' + name + ' is not Rust 1.93.0')
        fields = dict(line.split(': ', 1) for line in text.splitlines()[1:] if ': ' in line)
        require(fields.get('release') == '1.93.0' and fields.get('host') == 'x86_64-unknown-linux-gnu',
                'Active ' + name + ' release/host does not match the hosted lane')
        require(re.fullmatch(r'[0-9a-f]{40}', fields.get('commit-hash', '')) is not None,
                'Active ' + name + ' commit identity is missing')
        result[name] = {'stdout': text, 'stdout_sha256': hashlib.sha256(text.encode()).hexdigest(), 'fields': fields}
    return result



def warm_target(target, runner_temp, rustc):
    payload_deadline()
    require(target.is_absolute() and runner_temp.is_absolute(), 'Absolute target/runtime paths required')
    require(runner_temp.resolve(strict=True) == runner_temp, 'Runner temp path is an alias')
    require(target == runner_temp / 'held-scope-target' and target.resolve(strict=True) == target,
            'Expected exact producer-created target; no alias or replacement target')
    directories = {str(path): directory(path) for path in (target, target / 'debug', target / 'debug/deps', target / 'debug/.fingerprint')}
    compiler_marker, data = observe(target / '.rustc_info.json', target, 1024 * 1024)
    marker = json.loads(data)
    require(type(marker.get('rustc_fingerprint')) is int and isinstance(marker.get('outputs'), dict), 'Unsupported normal Cargo rustc marker')
    versions = [(key, value) for key, value in marker['outputs'].items()
                if isinstance(value, dict) and isinstance(value.get('stdout'), str) and value['stdout'].startswith('rustc ')]
    require(versions and all(value.get('success') is True and value.get('code') == 0 and value['stdout'] == rustc
                             for _, value in versions), 'Produced Cargo rustc output differs from active Rust1.93')
    paths = []
    with os.scandir(target / 'debug/deps') as entries:
        for entry in entries:
            payload_deadline()
            if not (entry.name.startswith('libcagents-') and entry.name.endswith('.rmeta')):
                continue
            paths.append(target / 'debug/deps' / entry.name)
            require(len(paths) <= 128, 'Too many produced metadata candidates')
    paths.sort()
    require(0 < len(paths) <= 128, 'No bounded existing cagents root-library metadata candidates')
    accepted, rejected = [], []
    for artifact in paths:
        payload_deadline()
        try:
            suffix = artifact.name.removeprefix('libcagents-').removesuffix('.rmeta')
            require(re.fullmatch(r'[0-9a-f]{16}', suffix) is not None, 'Unexpected Cargo artifact suffix')
            metadata, header = observe(artifact, target)
            require(header.startswith(b'rust\x00\x00\x00\x0a') and metadata['identity']['bytes'] >= 128, 'Invalid Rust1.93 metadata')
            library, archive_header = observe(artifact.with_suffix('.rlib'), target)
            require(archive_header.startswith(b'!<arch>\n') and library['identity']['bytes'] >= 128, 'No complete warm CLI library archive')
            fingerprint_dir = target / 'debug/.fingerprint' / ('cagents-' + suffix)
            fingerprint_receipt, data = observe(fingerprint_dir / 'lib-cagents.json', target, 1024 * 1024)
            fingerprint = json.loads(data)
            require(type(fingerprint.get('rustc')) is int and type(fingerprint.get('profile')) is int
                    and fingerprint.get('compile_kind') == 0, 'Missing native compiler/profile fingerprint')
            features = json.loads(fingerprint['features'])
            require(features == [], 'UI/HOST/full/default library cannot establish CLI features[] warmth')
            hash_receipt, hash_data = observe(fingerprint_dir / 'lib-cagents', target, 128)
            require(re.fullmatch(rb'[0-9a-f]{16}', hash_data) is not None, 'Invalid normal Cargo fingerprint hash')
            dep_receipt, _ = observe(fingerprint_dir / 'dep-lib-cagents', target, 1024 * 1024)
            accepted.append({'metadata': metadata, 'library': library, 'fingerprint': fingerprint_receipt,
                             'fingerprint_hash': hash_receipt, 'dep_info': dep_receipt,
                             'compiler_fingerprint': fingerprint['rustc'], 'profile': fingerprint['profile'],
                             'features': features, 'compile_kind': fingerprint['compile_kind']})
        except (GateFailure, OSError, ValueError, KeyError, TypeError) as error:
            rejected.append({'path': str(artifact), 'reason': str(error)})
    require(accepted, 'Produced payload has no complete CLI features[] library; UI warmth is insufficient: ' + json.dumps(rejected))
    return {'path': str(target), 'directories': directories, 'rustc_info': compiler_marker,
            'matched_rustc_output_keys': [key for key, _ in versions], 'cli_libraries': accepted,
            'scope': 'Producer-created native features[] cagents debug library presence only; no service publication or future restore proved. Cargo decides freshness. No integration-harness/native/host/UI/installed warmth claim.'}



def producer_tools(absolute_deadline):
    records = {}
    for name in ('rustc', 'cargo', 'rustdoc'):
        require_workload_allowed()
        which_argv = ['rustup', 'which', '--toolchain', '1.93.0', name]
        which = bounded_capture(which_argv, absolute_deadline, raw=True)
        path = Path(which['stdout'].strip())
        assert path.is_absolute() and path.resolve(strict=True) == path and not path.is_symlink()
        before = path.lstat()
        assert stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid() and before.st_mode & 0o111
        value = digest(path)
        assert identity(path.lstat()) == identity(before), 'Physical pinned tool changed during read'
        records[name] = {'path': str(path), 'sha256': value, 'identity': identity(before),
                         'which_command': {'argv': which_argv, **which}}
    assert len({str(Path(row['path']).parent) for row in records.values()}) == 1
    versions = {}
    for name in ('rustc', 'cargo'):
        argv = [records[name]['path'], '--version', '--verbose']
        versions[name] = {'argv': argv, **bounded_capture(argv, absolute_deadline, raw=True)}
    parsed = toolchains(versions['rustc']['stdout'], versions['cargo']['stdout'])
    return records, versions, parsed



def payload_deadline():
    require_workload_allowed()
    assert PAYLOAD_DEADLINE is not None and time.monotonic() < PAYLOAD_DEADLINE, 'Produced payload validation deadline expired'



def produced_payload(target, runner_temp, rustc, absolute_deadline):
    global PAYLOAD_DEADLINE
    PAYLOAD_DEADLINE = min(absolute_deadline, time.monotonic() + 90)
    payload_deadline()
    result = warm_target(target, runner_temp, rustc)
    payload_deadline()
    return result



def producer_main():
    global EVIDENCE_OUT, OWNERSHIP_INITIALIZED
    hosted = hosted_context()
    parser = argparse.ArgumentParser()
    parser.add_argument('--run-binding', required=True)
    parser.add_argument('--runner-sha256', required=True)
    parser.add_argument('--controller-review', required=True)
    parser.add_argument('--controller-review-sha256', required=True)
    args = parser.parse_args()
    assert args.run_binding == hosted['run_binding'], 'Original hosted run binding differs'
    runner = Path(__file__)
    assert runner.is_absolute() and not runner.is_symlink() and runner.resolve(strict=True) == runner
    runner_bytes = runner.read_bytes()
    assert re.fullmatch(r'[0-9a-f]{64}', args.runner_sha256)
    assert hashlib.sha256(runner_bytes).hexdigest() == args.runner_sha256
    plan_bytes = PLAN.read_bytes()
    assert hashlib.sha256(plan_bytes).hexdigest() == PLAN_SHA
    template = json.loads(plan_bytes)
    assert template['product_head'] == '375c37ff01aab0b226c58f547012af74e93a8643'
    assert template['product_tree'] == 'acb2cc6e049562469643e49a1ee5f7abc179c28b'
    assert template['packet_deadline_seconds'] == 2400 and template['planned_phase_count'] == 3
    assert template['counts']['qualifying_outer_parents'] == 0
    assert template['lane']['jobs'] == 2 and template['lane']['new_cache'] is True
    assert template['binary_checks'] == [{'label': 'cli-binary-check', 'argv': ['cargo', 'check', '--locked', '--no-default-features', '--bin', 'cagents'], 'seconds': 600}]
    assert [group['id'] for group in template['artifact_groups']] == ['installer_lifecycle_contract', 'installer_guard_contract']
    for group in template['artifact_groups']:
        assert group['features'] == [] and group['target_kind'] == ['test'] and group['target_name'] == group['id']
        assert group['compile_argv'] == ['cargo', 'test', '--locked', '--no-default-features', '--test', group['id'], '--no-run', '--message-format=json']
        assert group['compile_seconds'] == 900 and not group['selectors']
    assert template['planned_phase_order'] == ['cli-binary-check', 'compile-installer_lifecycle_contract', 'compile-installer_guard_contract']
    controller_path = Path(args.controller_review)
    assert controller_path == Path(control_path(template['source_review']))
    assert controller_path.is_absolute() and not controller_path.is_symlink() and controller_path.resolve(strict=True) == controller_path
    controller_bytes = controller_path.read_bytes()
    assert re.fullmatch(r'[0-9a-f]{64}', args.controller_review_sha256)
    assert hashlib.sha256(controller_bytes).hexdigest() == args.controller_review_sha256
    controller_review = validate_controller_review(controller_bytes, template['product_head'], template['product_tree'], args.runner_sha256, PLAN_SHA)
    case = json.loads(Path(control_path('.github/qualification/pr2015/case.json')).read_bytes())
    assert case['independent_review_path'] == template['source_review']
    assert case['runner_sha256'] == args.runner_sha256 and case['plan_sha256'] == PLAN_SHA
    for path, expected in case['source_sha256'].items():
        assert digest(control_path(path)) == expected, 'Selected producer input changed'
    out = Path(os.environ['RUNNER_TEMP']) / 'issue2015-hosted-evidence' / ('cache-producer-run-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '-' + str(os.getpid()))
    out.mkdir(mode=0o700)
    assert out.resolve(strict=True) == out and not out.is_symlink()
    save = lambda name, value: (out / name).write_text(json.dumps(value, indent=2) + '\n')
    EVIDENCE_OUT = out
    evidence_copy(out, 'executed-runner.py', runner_bytes, args.runner_sha256)
    evidence_copy(out, 'plan.json', plan_bytes, PLAN_SHA)
    evidence_copy(out, 'controller-review.json', controller_bytes, args.controller_review_sha256)
    save('hosted-context-before.json', hosted)
    # Before the first supervised rustup/tool/source child, including failures.
    signal.signal(signal.SIGINT, note_lane_cancellation)
    signal.signal(signal.SIGTERM, note_lane_cancellation)
    assert ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0, 'Dedicated subreaper required before the first child'
    OWNERSHIP_INITIALIZED = True
    try:
        plan = materialize_plan(template, hosted)
        assert plan['source_review_sha256'] == args.controller_review_sha256
        archive_bytes = []
        for binding in plan['archive_bindings']:
            payload = Path(binding['path']).read_bytes()
            assert hashlib.sha256(payload).hexdigest() == binding['sha256']
            archive_bytes.append((binding, payload))
        for binding, payload in archive_bytes:
            evidence_copy(out, binding['name'], payload, binding['sha256'])
        save('runtime-plan.json', plan)
        save('tool-setup-original.json', plan['actual_tool_observations'])
        save('created-target.json', plan['created_target'])
    except BaseException as startup_error:
        passes = []
        for _ in range(2):
            try:
                passes.append(settle_children())
            except BaseException as error:
                passes.append({'clean': False, 'forced': False, 'inspection_error': type(error).__name__ + ': ' + str(error)})
        custody = {'clean': all(custody_clean(row) for row in passes), 'forced': any(row.get('forced') for row in passes), 'passes': passes}
        record = {'state': 'PRODUCER_INITIALIZATION_FAILED', 'error': type(startup_error).__name__ + ': ' + str(startup_error),
                  'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES,
                  'supervision_failures': SUPERVISION_FAILURES, 'custody': custody,
                  'cache_preparation_complete': False, 'owning_passes': 0,
                  'global_C_return': 'NOT_MADE; complete original job and external RAW/return/acceptance required.'}
        save('initialization-failure.json', record)
        if not safe_lane_handoff(PROCESS_ATTEMPTS, PROCESS_STATES, [], custody, SUPERVISION_FAILURES):
            retain_uncertain_lane(save, record)
        print(json.dumps({'state': record['state'], 'evidence': str(out), 'cache_preparation_complete': False}), flush=True)
        raise
    save('controller-before-lease.json', {'hosted_run_binding': args.run_binding, 'runner_path': str(runner),
         'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA,
         'review_sha256': plan['source_review_sha256'], 'controller_review_path': str(controller_path),
         'controller_review_sha256': args.controller_review_sha256, 'head': plan['product_head'], 'tree': plan['product_tree'],
         'argv': sys.argv, 'execution': 'New standalone producer; original bytes retained before job-local custody fd. External Root C precedes whole hosted job.', 'at': now()})
    env = os.environ.copy()
    overrides = {'CARGO_TARGET_DIR': str(TARGET), 'CARGO_BUILD_JOBS': '2', 'RUSTC': str(TOOL / 'rustc'),
                 'RUSTDOC': str(TOOL / 'rustdoc'), 'RUSTUP_TOOLCHAIN': '1.93.0', 'RUSTUP_AUTO_INSTALL': '0',
                 'PATH': str(TOOL) + os.pathsep + env['PATH']}
    env.update(overrides)
    # Install cooperative retention before any source child or C acquisition.
    signal.signal(signal.SIGINT, note_lane_cancellation)
    signal.signal(signal.SIGTERM, note_lane_cancellation)
    phases, tests = PROCESS_PHASES, []
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
        assert hosted_context() == hosted, 'Hosted source/context/run binding changed'
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
        receipt = {'state': 'PRELEASE_SOURCE_FAILURE', 'qualified': False, 'lock_acquired': False,
                   'error_type': type(prelease_error).__name__, 'evidence': str(out),
                   'failure_path': str(out / 'prelease-source-failure.json'), 'at': now()}
        save('terminal-receipt.json', receipt)
        passes = [prelease_custody]
        try:
            passes.append(settle_children())
        except BaseException as error:
            passes.append({'clean': False, 'forced': False, 'inspection_error': type(error).__name__ + ': ' + str(error)})
        startup_custody = {'clean': all(custody_clean(row) for row in passes), 'forced': any(row.get('forced') for row in passes), 'passes': passes}
        if not safe_lane_handoff(PROCESS_ATTEMPTS, PROCESS_STATES, [], startup_custody, SUPERVISION_FAILURES):
            retain_uncertain_lane(save, {'stage': 'producer-prelease-source', 'record': receipt, 'custody': startup_custody, 'states': PROCESS_STATES})
        print(json.dumps({**receipt, 'receipt_path': str(out / 'terminal-receipt.json'),
                          'receipt_sha256': digest(out / 'terminal-receipt.json')}), flush=True)
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
             'policy': 'Natural hosted User account/auth/environment untouched; compiler overrides only',
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
        print(json.dumps({'phase': label, 'exit': state['exit'], 'seconds': record['seconds'], 'custody_clean': custody_clean(custody), 'custody_forced': custody.get('forced'), 'custody_observed_count': len(custody.get('observed', []))}), flush=True)
        if failure is not None:
            raise failure
        assert source(absolute_deadline=phase_deadline) == before and context() == before_context
        assert process_error is None and state['original_wait_complete'] and not expired and custody_clean(custody), 'Uncertain phase custody/timeout/process control'
        if state['exit'] != 0:
            raise RuntimeError('Actual nonzero phase exit: ' + label + ': ' + str(state['exit']))
        return d

    try:
        require_workload_allowed()
        lock_fd = os.open(LOCK, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_CREAT | os.O_EXCL, 0o600)
        info = os.fstat(lock_fd)
        assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
        require_workload_allowed()
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        acquired = True
        deadline = time.monotonic() + plan['packet_deadline_seconds']
        save('lease.json', {'hosted_run_binding': args.run_binding, 'lane': 'hosted-job-custody', 'global_C': 'External Root coordination only; this fd does not allocate or return it', 'target': str(TARGET), 'lock': str(LOCK),
             'nonblocking': True, 'device': info.st_dev, 'inode': info.st_ino, 'pid': os.getpid(), 'at': now()})
        save('tools.json', {'physical_paths': {name: str(TOOL / name) for name in plan['tool_sha256']},
                            'sha256': plan['tool_sha256'], 'version_processes': 'Original supervised raw physical1.93 observations are retained in tool-setup-original.json.'})
        print(json.dumps({'state': 'HOSTED_CUSTODY_FD_ACQUIRED', 'evidence': str(out), 'at': now()}), flush=True)
        artifacts = execute_contracts(plan, run, save, tests)
        payload = produced_payload(TARGET, Path(os.environ['RUNNER_TEMP']), plan['actual_tool_observations']['version_commands']['rustc']['stdout'], deadline)
        for artifact in artifacts.values():
            verify_elf(artifact['harness'])
            verify_elf(artifact['cli'])
        save('produced-payload.json', {'outcome': 'success', 'payload': payload, 'cache_publication': 'UNOBSERVED', 'future_restore': 'UNQUALIFIED', 'owning_passes': 0})
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
        summary = {'head': plan['product_head'], 'tree': plan['product_tree'], 'hosted_run_binding': args.run_binding,
             'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA, 'source_review_sha256': plan['source_review_sha256'],
             'controller_review_sha256': args.controller_review_sha256,
             'error': error, 'phases': phases, 'parents': tests, **accounting, 'raw_parent_results': raw,
             'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES, 'supervision_failures': SUPERVISION_FAILURES,
             'cancellation_signals': CANCELLATION_SIGNALS,
             'custody': custody, 'historical_handoff_clean': historical_handoff_clean,
             'lock_closed': lock_closed, 'lock_unlocked': lock_unlocked, 'lock_close_error': lock_close_error,
             'actual_release': actual_release,
             'release_policy': 'Original Cfd closes only after complete original waits/evidence and clean unforced phase plus both final custody observations. No LOCK_UN is issued. Unknown assessment/handoff or close retains no safe-return claim; ambiguous close is never retried.',
             'cache_preparation_complete': error is None and not CANCELLATION_SIGNALS and len(tests) == 0 and [phase['label'] for phase in phases] == plan['planned_phase_order'] and actual_release,
             'nonadditive_child_result_count': sum(len(parent['nonadditive_child_results']) for parent in tests),
             'finished': now(), 'limits': plan['limits']}
        save('hosted-context-after.json', hosted_context())
        summary['hosted_provenance'] = hosted
        summary['global_C_return'] = 'Not made by controller. Original full workflow terminal, independent RAW and external Root return/acceptance remain required.'
        save('summary.json', summary)
        terminal = {'state': 'FINISHED', 'head': summary['head'], 'tree': summary['tree'], 'hosted_run_binding': args.run_binding,
                    'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA,
                    'cache_preparation_complete': summary['cache_preparation_complete'], 'error_present': summary['error'] is not None,
                    'attempted_phase_count': summary['attempted_phase_count'],
                    'passed_parent_count': summary['passed_parent_count'],
                    'unknown_process_count': summary['unknown_process_count'],
                    'all_original_waits_complete': summary['all_original_waits_complete'],
                    'custody_clean': custody_clean(custody), 'custody_forced': custody['forced'],
                    'actual_release': summary['actual_release'], 'lock_closed': summary['lock_closed'],
                    'lock_unlocked': summary['lock_unlocked'], 'lock_close_error_present': summary['lock_close_error'] is not None,
                    'evidence': str(out), 'summary_path': str(out / 'summary.json'),
                    'summary_sha256': digest(out / 'summary.json'), 'finished': summary['finished']}
        save('terminal-receipt.json', terminal)
        files = sorted(path for path in out.rglob('*') if path.is_file())
        (out / 'SHA256SUMS').write_text(''.join(digest(path) + '  ' + str(path.relative_to(out)) + '\n' for path in files))
        # Full original ledger remains on disk; terminal output stays bounded.
        print(json.dumps({**terminal, 'receipt_path': str(out / 'terminal-receipt.json'),
                          'receipt_sha256': digest(out / 'terminal-receipt.json'),
                          'checksum_sha256': digest(out / 'SHA256SUMS')}), flush=True)
    return 0 if summary['cache_preparation_complete'] else 1


def main():
    try:
        return producer_main()
    except BaseException as original_error:
        if OWNERSHIP_INITIALIZED:
            passes = []
            for _ in range(2):
                try:
                    passes.append(settle_children())
                except BaseException as error:
                    passes.append({'clean': False, 'forced': False, 'inspection_error': type(error).__name__ + ': ' + str(error)})
            custody = {'clean': all(custody_clean(row) for row in passes),
                       'forced': any(row.get('forced') for row in passes), 'passes': passes}
            record = {'stage': 'outer-producer-exit-guard',
                      'error': type(original_error).__name__ + ': ' + str(original_error),
                      'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES,
                      'phases': PROCESS_PHASES, 'tool_captures': TOOL_CAPTURES,
                      'custody': custody, 'supervision_failures': SUPERVISION_FAILURES,
                      'cache_preparation_complete': False, 'global_C_return': 'NOT_MADE'}
            def emergency_save(name, value):
                assert EVIDENCE_OUT is not None
                (EVIDENCE_OUT / name).write_text(json.dumps(value, indent=2) + '\n')
            try:
                emergency_save('outer-exit-failure.json', record)
            except BaseException:
                pass
            # Evidence failures cannot bypass retention of an unknown original.
            try:
                handback_confirmed = safe_lane_handoff(PROCESS_ATTEMPTS, PROCESS_STATES, PROCESS_PHASES, custody, SUPERVISION_FAILURES)
            except BaseException:
                handback_confirmed = False
            if not handback_confirmed:
                retain_uncertain_lane(emergency_save, record)
        raise


if __name__ == '__main__':
    raise SystemExit(main())
