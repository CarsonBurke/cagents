"""Issue2025 exact-two recovery-cache partial diagnostic. Fresh code review and scoped grant required; no full347 qualification."""
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

WT = Path('/home/marvin/Documents/repositories/cagents/.worktrees/fw16-pr1549-search-current-main')
TARGET = Path('/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-target')
LOCK = TARGET.parent / 'lane-c.lock'
TOOL = Path('/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin')
PLAN = Path('/home/marvin/Documents/repositories/cagents/.worktrees/coord16-pr1549-c28-host3-source/qualification-packets/pr1549-c28-host3/plan.json')
PROCESS_ATTEMPTS = []  # Append-only immutable successful-spawn tuples; never inferred from evidence files.
PROCESS_STATES = []    # Separate original-wait and evidence observations; null stays unknown.
SUPERVISION_FAILURES = []  # Sticky history, including failures after an otherwise successful wait.
CANCELLATION_SIGNALS = []  # Cooperative cancellation, never cleared by cleanup.
PLAN_SHA = '58256b3a2582d37326465feda15ec105626ae135241765334f99e295adaadba2'
SCOPE_PARENT = None  # No grouped contract enables the dormant donor staged-scope path.


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
    # Current parentage and a freshly opened pidfd do not establish original
    # custody or permission to stop an adopted provider descendant. Leave
    # unproven survivors unsignaled; the caller must retain its C/HOLD state.
    return {'clean': False, 'forced': forced, 'observed': remaining, 'remaining': remaining}


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


def cargo_helper_artifact(text, manifest_path, source_path):
    """Task-only same-root non-test daemon artifact from original phase1 JSON."""
    artifacts, finished = [], []
    for line in text.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'build-finished':
            finished.append(item.get('success'))
        if (item.get('reason') == 'compiler-artifact' and item.get('target', {}).get('kind') == ['bin']
                and item.get('target', {}).get('name') == 'cagentsd' and item.get('executable')
                and Path(item.get('manifest_path', '')).is_absolute()
                and Path(item['manifest_path']) == Path(manifest_path)):
            artifacts.append(item)
    assert finished == [True], 'Helper Cargo phase lacks one successful build-finished'
    assert len(artifacts) == 1, 'Same-root cagentsd artifact missing/ambiguous'
    artifact = artifacts[0]
    assert artifact['features'] == ['host'] and artifact['profile']['test'] is False
    assert artifact['target']['crate_types'] == ['bin'] and artifact['target']['test'] is False
    assert Path(artifact['manifest_path']).resolve(strict=True) == Path(manifest_path)
    assert Path(artifact['target']['src_path']).is_absolute() and Path(artifact['target']['src_path']) == Path(source_path)
    assert Path(artifact['target']['src_path']).resolve(strict=True) == Path(source_path)
    assert isinstance(artifact['package_id'], str) and artifact['package_id']
    assert isinstance(artifact['fresh'], bool)
    # package_id/profile/fresh stay original observed Cargo metadata. A cached
    # artifact is not a newly compiled one; no embedded --version proof is minted.
    return artifact


def task_elf_observation(path, absolute_deadline):
    """Read one admitted target ELF through its original no-follow descriptor."""
    path = Path(path)
    assert path.is_absolute() and '..' not in path.parts and path.is_relative_to(TARGET)
    assert TARGET == TARGET.resolve(strict=True) and path == path.resolve(strict=True)
    assert path.is_relative_to(TARGET)
    if time.monotonic() >= absolute_deadline:
        raise TimeoutError('Original packet deadline exhausted before ELF observation')
    def identity(info):
        return {'device': info.st_dev, 'inode': info.st_ino, 'mode': info.st_mode,
                'uid': info.st_uid, 'gid': info.st_gid, 'links': info.st_nlink,
                'size': info.st_size, 'mtime_ns': info.st_mtime_ns, 'ctime_ns': info.st_ctime_ns}
    named = path.lstat()
    assert stat.S_ISREG(named.st_mode) and named.st_uid == os.getuid() and not path.is_symlink()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    original_error, close_error, result = None, None, None
    try:
        opened = os.fstat(fd)
        assert stat.S_ISREG(opened.st_mode) and identity(opened) == identity(named)
        assert os.read(fd, 4) == b'\x7fELF', 'Cargo-reported artifact is not ELF'
        os.lseek(fd, 0, os.SEEK_SET)
        hashed = hashlib.sha256()
        total = 0
        while True:
            if time.monotonic() >= absolute_deadline:
                raise TimeoutError('Original packet deadline exhausted during ELF observation')
            block = os.read(fd, 1024 * 1024)
            if not block:
                break
            total += len(block)
            assert total <= opened.st_size, 'ELF grew while observed'
            hashed.update(block)
        assert total == opened.st_size and identity(os.fstat(fd)) == identity(opened)
        assert identity(path.lstat()) == identity(opened) and path.resolve(strict=True) == path
        result = {'path': str(path), 'sha256': hashed.hexdigest(), 'metadata': identity(opened),
                  'original_read_descriptor_closed': True}
    except BaseException as exc:
        original_error = exc
    finally:
        try:
            os.close(fd)
        except BaseException as exc:
            close_error = exc  # No uncertain-close retry or descriptor adoption.
    if original_error is not None:
        if close_error is not None:
            raise RuntimeError('Original ELF observation failed: ' + str(original_error)
                               + '; original descriptor close unconfirmed: ' + str(close_error)) from original_error
        raise original_error
    if close_error is not None:
        raise RuntimeError('Original ELF descriptor close unconfirmed: ' + str(close_error)) from close_error
    return result


def task_group_assets(helper, exe, lib_sha, absolute_deadline):
    observed = {'helper': None, 'library': None, 'errors': []}
    for name, path in [('helper', helper['file']['path']), ('library', exe)]:
        try:
            observed[name] = task_elf_observation(path, absolute_deadline)
        except BaseException as exc:
            # Preserve a successful observation of the other fixed asset and
            # keep read/descriptor uncertainty sticky, without inventing cleanup.
            observed['errors'].append(supervision_failure(None, exc, 'task-group-asset-' + name))
    observed['helper_matches_discovery'] = observed['helper'] == helper['file']
    observed['library_matches_original_sha256'] = bool(observed['library'] and observed['library']['sha256'] == lib_sha)
    return observed


def grouped_contracts(text, actual, group):
    """Three exact singleton contracts from this original full inventory."""
    benchmarks = []
    for line in text.splitlines():
        found = re.fullmatch(r'(\S+): benchmark', line)
        if found:
            benchmarks.append(found.group(1))
    assert len(actual + benchmarks) == len(set(actual + benchmarks)), 'Duplicate ordinary/benchmark inventory identity'
    names = [contract['name'] for contract in group['selectors']]
    assert len(names) == 3 and len(set(names)) == 3
    assert all(actual.count(name) == 1 for name in names)
    assert not any(name in names for name in benchmarks), 'Matching benchmark is outside this ordinary diagnostic task'
    total = len(actual) + len(benchmarks)
    return [{'label': 'parent-host-lib-' + str(index + 1), 'kind': 'host-singleton-exact', 'names': [row['name']],
             'filters': [row['name']], 'exact': True, 'seconds': row['seconds'],
             'actual_inventory_N': total, 'expected_filtered_out': total - 1}
            for index, row in enumerate(group['selectors'])]


def grouped_result(text, contract):
    """Observe original stdout only; negative/ambiguous syntax never earns credit."""
    names = contract['names']
    assert names and len(names) == len(set(names))
    events, completions, running, footers, errors, warnings = [], [], [], [], [], []
    warned_names = set()
    for line in text.splitlines():
        if not line:
            continue
        if line.startswith('test result:'):
            found = re.fullmatch(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out(?:; finished in \d+(?:\.\d+)?s)?', line)
            if not found:
                errors.append('Malformed original footer')
            else:
                status, *counts = found.groups()
                footers.append({'status': status, **dict(zip(('passed', 'failed', 'ignored', 'measured', 'filtered_out'), map(int, counts)))})
                events.append('footer')
        elif line.lstrip().startswith('test result:'):
            errors.append('Indented or interleaved footer')
        elif line.startswith('running '):
            found = re.fullmatch(r'running (\d+) tests?', line)
            if found:
                running.append(int(found.group(1)))
                events.append('running')
            else:
                errors.append('Malformed running record')
        elif line.startswith('test '):
            warning = re.fullmatch(r'test (\S+) has been running for over \d+ seconds', line)
            if warning:
                name = warning.group(1)
                # A source-supported warning belongs to one still-pending test
                # inside the sole run. Repeated warnings for a name refuse.
                if (name not in names or running != [len(names)] or footers
                        or not events or events[0] != 'running'
                        or any(item['name'] == name for item in completions)):
                    errors.append('Progress warning outside its pending test lifecycle')
                if name in warned_names:
                    errors.append('Duplicate progress warning for one test name')
                warned_names.add(name)
                warnings.append(line)
                continue
            found = re.fullmatch(r'test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored(?:, .*)?)', line)
            if not found or found.group(1) not in names:
                errors.append('Unexpected named result shape or full name')
            else:
                completions.append({'name': found.group(1), 'result': found.group(2)})
                events.append('completion')
        elif line.lstrip().startswith(('running ', 'test ')) or line in ('ok', 'FAILED') or line.startswith('ignored'):
            errors.append('Malformed or standalone result record')
        else:
            errors.append('Unsupported nonempty original stdout line')
    count = len(names)
    if running != [count]:
        errors.append('Original running count missing/duplicate/wrong')
    seen = [item['name'] for item in completions]
    if len(seen) != count or set(seen) != set(names) or len(seen) != len(set(seen)):
        errors.append('Selected completion missing/duplicate/unexpected')
    if any(item['result'] != 'ok' for item in completions):
        errors.append('Failure or ignored result is not a pass')
    expected = {'status': 'ok', 'passed': count, 'failed': 0, 'ignored': 0, 'measured': 0,
                'filtered_out': contract['expected_filtered_out']}
    if footers != [expected]:
        errors.append('Original footer does not equal actual group/inventory contract')
    if events != ['running'] + ['completion'] * count + ['footer']:
        errors.append('Missing/duplicate/out-of-order outer result records')
    return {'qualified_stdout': not errors, 'selected_names': names, 'completions': completions,
            'running_counts': running, 'footers': footers, 'qualification_errors': errors,
            'progress_warnings': warnings, 'nonadditive_child_results': [],
            'child_limit': 'Existing Rust self-child assertions are nonadditive; successful child streams are not archived here.'}


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
        contracts = grouped_contracts((inventory / 'stdout.log').read_text() + '\n' + (inventory / 'stderr.log').read_text(), actual, group)
        save('group-contracts.json', contracts)
        for contract in contracts:
            argv = [str(exe), *contract['filters']]
            if contract['exact']:
                argv.append('--exact')
            argv += ['--format=pretty', '--color=never']
            observations = {'label': contract['label'], 'before': None, 'after': None,
                            'invocation_call_requested': False, 'original_phase_attempts': [],
                            'primary_error': None, 'diagnostic_errors': []}
            primary, diagnostic, d = None, None, None
            try:
                try:
                    observations['before'] = task_elf_observation(exe, invoke.issue2025_deadline())
                except BaseException as exc:
                    supervision_failure(None, exc, 'task-group-pre-asset-observation')
                    raise
                assert observations['before']['sha256'] == elf['sha256'], 'Group assets differ from original discoveries'
                assert digest(exe) == elf['sha256']
                observations['invocation_call_requested'] = True
                d = invoke(contract['label'], argv, contract['seconds'], contract)
                assert digest(exe) == elf['sha256'], 'ELF changed during owning group'
            except BaseException as exc:
                primary = exc
                observations['primary_error'] = {'type': type(exc).__name__, 'message': str(exc)}
            # Attempt post-use observations even when original run failed. No new
            # deadline, workload rerun, fabricated parent wait or error replacement.
            if observations['invocation_call_requested']:
                try:
                    observations['after'] = task_elf_observation(exe, invoke.issue2025_deadline())
                    if observations['after'] != observations['before']:
                        diagnostic = RuntimeError('Post-group asset observation incomplete')
                        observations['diagnostic_errors'].append('Library identity differs from its original before-use observation')
                except BaseException as exc:
                    diagnostic = exc
                    observations['diagnostic_errors'].append(supervision_failure(None, exc, 'task-group-post-asset-observation'))
            observations['original_phase_attempts'] = [
                {'label': label, 'pid': pid} for stage, label, _argv, pid in PROCESS_ATTEMPTS
                if stage == 'phase' and label == contract['label']]
            try:
                save('library-use-' + contract['label'] + '.json', observations)
            except BaseException as exc:
                if diagnostic is None:
                    diagnostic = exc
                supervision_failure(None, exc, 'task-group-asset-evidence')
            if primary is not None:
                raise primary  # Original nonzero/control failure keeps priority.
            if diagnostic is not None:
                raise diagnostic
            assert observations['after'] == observations['before'] and observations['after']['sha256'] == elf['sha256'], 'Post-group asset identity refused'
            proof = grouped_result((d / 'stdout.log').read_text(), contract)
            save(contract['label'] + '/group-result.json', proof)
            assert proof['qualified_stdout'], 'Original group did not satisfy strict selected-name/footer contract'
            parents.append({'group': gid, 'invocation': contract['label'], 'names': contract['names'],
                            'passed': len(contract['names']), 'failed': 0, 'ignored': 0,
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
    raw, errors, contracts = [], [], {}
    if attempted:
        try:
            values = json.loads((out / 'group-contracts.json').read_bytes())
            assert [value['label'] for value in values] == ['parent-host-lib-1', 'parent-host-lib-2', 'parent-host-lib-3']
            contracts = {value['label']: value for value in values}
        except Exception as exc:
            errors.append('Group contract evidence: ' + type(exc).__name__ + ': ' + str(exc))
    for stage, label, argv, pid in attempted:
        state = next((state for state in states if state['stage'] == stage and state['label'] == label and state['pid'] == pid), None)
        record = next((phase for phase in phases if phase['label'] == label and phase['pid'] == pid), None)
        row = {'phase': label, 'pid': pid, 'exit': state['exit'] if state else None,
               'original_wait_complete': bool(state and state['original_wait_complete']),
               'phase_recorded': record is not None, 'evidence_complete': bool(state and state['evidence_complete']),
               'supervision_error': state['supervision_error'] if state else 'Spawn has no supervision state',
               'summaries': [], 'output_read_error': None, 'selected_names': None, 'group_qualified': False}
        try:
            payload = (out / label / 'stdout.log').read_bytes()
            output = payload.decode('utf-8')
            row['summaries'] = [line for line in output.splitlines() if line.lstrip().startswith('test result:')]
            row['stdout_matches_original_phase_hash'] = bool(record
                and record['stdout_sha256'] == hashlib.sha256(payload).hexdigest())
            if label in contracts:
                contract = contracts[label]
                row['selected_names'] = contract['names']
                row['observed_results'] = grouped_result(output, contract)
                earlier = [parent for parent in parents if parent['invocation'] == label]
                row['historical_result_matches_raw'] = bool(len(earlier) == 1
                    and earlier[0]['names'] == contract['names']
                    and all(earlier[0].get(key) == value for key, value in row['observed_results'].items()))
                row['group_qualified'] = bool(row['observed_results']['qualified_stdout'] and state
                    and state['original_wait_complete'] and state['exit'] == 0 and state['evidence_complete']
                    and not state['supervision_error'] and row['stdout_matches_original_phase_hash']
                    and row['historical_result_matches_raw'])
            else:
                row['contract_error'] = 'Original selected-name contract unavailable; no parent credit'
        except Exception as exc:
            row['output_read_error'] = type(exc).__name__ + ': ' + str(exc)
            errors.append(row['output_read_error'])
        raw.append(row)
    assert len({row['phase'] for row in raw}) == len(raw) and 0 <= len(parents) <= len(attempted) <= 3
    phase_attempts = [attempt for attempt in attempts if attempt[0] == 'phase']
    phase_states = [state for state in states if state['stage'] == 'phase']
    source_attempts = [attempt for attempt in attempts if attempt[0] == 'source']
    source_states = [state for state in states if state['stage'] == 'source']
    settled_phases = sum(state['original_wait_complete'] for state in phase_states)
    settled_sources = sum(state['original_wait_complete'] for state in source_states)
    requested = sorted({name for value in contracts.values() for name in value['names']}) if contracts else None
    # Earlier parsed parents remain historical observations. Final credit comes
    # only from this raw reread reconciled with original bytes and those records.
    qualified_rows = [row for row in raw if row['group_qualified']]
    qualified = sorted({name for row in qualified_rows for name in row['selected_names']})
    expected_labels = ['parent-host-lib-1', 'parent-host-lib-2', 'parent-host-lib-3']
    semantic_refusals = [row['phase'] for row in raw if not row['group_qualified']]
    final_group_semantics_qualified = bool([row['phase'] for row in raw] == expected_labels
        and [parent['invocation'] for parent in parents] == expected_labels
        and list(contracts) == expected_labels and len(qualified_rows) == 3)
    known_occurrences = sum(len(row['selected_names']) for row in raw if row['selected_names'] is not None)
    return ({'attempted_process_count': len(attempts),
             'original_wait_settled_process_count': settled_phases + settled_sources,
             'unknown_process_count': len(attempts) - settled_phases - settled_sources,
             'attempted_phase_count': len(phase_attempts), 'recorded_phase_count': len(phases),
             'original_wait_settled_phase_count': settled_phases,
             'unknown_phase_count': len(phase_attempts) - settled_phases, 'unrun_phase_count': 6 - len(phase_attempts),
             'attempted_source_check_count': len(source_attempts),
             'original_wait_settled_source_check_count': settled_sources,
             'unknown_source_check_count': len(source_attempts) - settled_sources,
             'all_original_waits_complete': len(attempts) == len(states) and all(state['original_wait_complete'] for state in states),
             'requested_test_invocation_count': 3, 'attempted_test_invocation_count': len(attempted),
             'qualified_test_invocation_count': len(qualified_rows), 'unrun_test_invocation_count': 3 - len(attempted),
             'historical_qualified_test_invocation_count': len(parents),
             'final_group_semantics_qualified': final_group_semantics_qualified,
             'semantic_refused_invocations': semantic_refusals,
             'semantic_refusal_policy': 'FAIL_STOP without automatic supervision/HOLD; read/decode/control exceptions retain raw_errors',
             'original_wait_settled_test_invocation_count': sum(row['original_wait_complete'] for row in raw),
             'unknown_test_invocation_count': sum(not row['original_wait_complete'] for row in raw),
             'requested_unique_parent_names': requested, 'requested_unique_parent_count': len(requested) if requested is not None else None,
             'qualified_unique_parent_names': qualified, 'qualified_unique_parent_count': len(qualified),
             'selected_parent_occurrence_count': known_occurrences if all(row['selected_names'] is not None for row in raw) else None,
             'qualified_parent_occurrence_count': sum(len(row['selected_names']) for row in qualified_rows),
             'parent_wait_receipts_fabricated': False, 'overlap_and_children_nonadditive': True}, raw, errors)


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
    assert plan['runtime_admission']['state'] == 'PR1549_C28_HOST3_SOURCE_REQUIRES_FROZEN_CODE_REVIEW_AND_FRESH_GRANT'
    assert plan['runtime_admission']['execution_authorized'] is False and plan['runtime_admission']['helper_binding'] is None
    assert plan['readiness'] is False and all(value is None for value in plan['actual'].values())
    assert not any(name in os.environ for name in (
        'CAGENTS_UPGRADE_STATE_FIXTURE_SELECTOR', 'RUST_TEST_NOCAPTURE', 'RUST_TEST_THREADS',
        'CAGENTS_TEST_RETAINED_ACTOR_HELPER', 'CAGENTS_RETAINED_ACTOR_FIXTURE')), 'Preexisting fixture/capture/scheduler/helper variable name refuses this task'
    assert digest(plan['source_review']) == plan['source_review_sha256']
    review = json.loads(Path(plan['source_review']).read_bytes())
    assert review['kind'] == 'AUTHOR_ASSEMBLED_PR1549_C28_HOST3_SOURCE_BINDING_NOT_APPROVAL'
    assert (review['head'], review['tree'], review['parent']) == ('c28e76c72425a59abd32d977cef2c7995e8095c3', '164a8860b6740bf36734fc6bfa5d812aceafd751', '9e63cde8e9e6b1e80e128c7e1aa22681840f690d')
    assert (plan['product_head'], plan['product_tree'], plan['product_parent']) == (review['head'], review['tree'], review['parent'])
    assert review['product_pr'] == plan['product_pr'] == 1549
    assert review['current_main'] == plan['current_main'] == '4429728a1f179ea7f7eaca9667156553209a46ac'
    def read_bound(key, as_json=True):
        ref = review[key]
        payload = Path(ref['path']).read_bytes()
        assert hashlib.sha256(payload).hexdigest() == ref['sha256'], 'Changed source/report input: ' + key
        return json.loads(payload) if as_json else payload.decode('utf-8')

    data = read_bound('command_data')
    source_map = read_bound('current_source_map')
    freeze = read_bound('product_freeze')
    root_adoption = read_bound('product_adoption')
    correction = read_bound('product_correctness')
    conservation = read_bound('product_conservation')
    ux_source = read_bound('product_ux_source')
    main_assembly = read_bound('product_main_assembly')
    publication = read_bound('product_publication')
    read_bound('prior_ee7_UI_negative', False)  # Preserve original outcome, never reinterpret its RAW.
    assert freeze['schema'] == 'pr1549_current_main_search_source_freeze_v3'
    assert (freeze['head'], freeze['tree'], freeze['parents']) == (review['head'], review['tree'], [review['parent']])
    assert freeze['current_main'] == review['current_main'] and freeze['merge_commit'] == review['parent']
    assert freeze['outside_feature_entries_exact_main'] == 2205 and len(freeze['feature_paths_against_main']) == 18
    assert set(freeze['pins']) == set(freeze['feature_paths_against_main'])
    assert all(plan['source_paths_sha256'][path] == row['sha256'] for path, row in freeze['pins'].items())
    assert freeze['independent_reviews'] is None and freeze['public_write'] is None  # Honest original snapshot.
    assert root_adoption['schema'] == 'pr1549_c28_current_main_root_source_adoption_v1'
    assert (root_adoption['head'], root_adoption['tree'], root_adoption['merge_commit'], root_adoption['current_main']) == (review['head'], review['tree'], review['parent'], review['current_main'])
    assert root_adoption['qualification']['merge_ready'] is False and root_adoption['qualification']['C_grant'] is None
    for role, ref_key in (('freeze', 'product_freeze'), ('correctness', 'product_correctness'), ('conservation', 'product_conservation'), ('ux_source', 'product_ux_source'), ('main_assembly', 'product_main_assembly'), ('preserved_negative', 'prior_ee7_UI_negative')):
        matching = [row for row in root_adoption['source_artifacts'] if row['role'] == role]
        assert len(matching) == 1 and matching[0]['sha256'] == review[ref_key]['sha256']
    assert review['product_reviews'] == {role: review['product_' + role]['sha256'] for role in ('correctness', 'conservation', 'ux_source', 'main_assembly')}
    assert correction['schema'] == 'pr1549_c28e76c7_drop_state_fixture_correction_independent_source_review_v1'
    assert correction['result'] == 'BOUNDED_SOURCE_CLEAR_KNOWN_E0509_CAUSE_REMOVED_NO_NEW_DELTA_BLOCKER' and correction['findings'] == []
    assert (correction['head'], correction['tree'], correction['parent']) == (review['head'], review['tree'], review['parent'])
    assert correction['freeze']['sha256'] == review['product_freeze']['sha256']
    assert correction['verified_conservation']['all_production_bytes_exact_parent'] is True
    assert correction['verified_conservation']['all_other_tree_entries_exact_parent_modes_types_blobs'] == 2222
    assert conservation['schema'] == 'pr1549_c28e76c7_two_fixture_correction_independent_conservation_review_v1'
    assert conservation['verdict'] == 'BOUNDED_FIXTURE_CORRECTION_SOURCE_CLEAR_CURRENT_COMPILE_UNRUN'
    assert (conservation['source']['head'], conservation['source']['tree'], conservation['source']['parents']) == (review['head'], review['tree'], [review['parent']])
    assert conservation['source']['freeze']['sha256'] == review['product_freeze']['sha256']
    assert conservation['delta']['all2205_outside18_entries_exact_main442'] is True
    assert conservation['delta']['all_other2222_entries_modes_types_blobs_exact9e63'] is True
    assert conservation['delta']['all_production_bytes_before_cfg_test_module_exact9e63'] is True
    assert main_assembly['schema'] == 'pr1549_9e63_current_main_actual_merge_independent_assembly_conservation_review_v1'
    assert main_assembly['source']['head'] == review['parent']
    assert main_assembly['source']['parents'] == [freeze['feature_parent'], review['current_main']]
    assert main_assembly['verdict'] == 'ASSEMBLY_CONSERVATION_CLEAR_WHOLEHEAD_UI_TEST_COMPILE_BLOCKED'
    assert main_assembly['known_whole_head_blocker']['status'] == 'BLOCKED'  # Historical9e63, source cause corrected separately; no compiler pass.
    assert main_assembly['parent_conservation']['actual_total_entries'] == 2223
    assert main_assembly['parent_conservation']['all2205_outside_feature_entries_exact_main'] is True
    assert main_assembly['parent_conservation']['exactly18_changed_paths_vs_main'] == freeze['feature_paths_against_main']
    assert all(plan['source_paths_sha256'][path] == row['sha256'] for path, row in main_assembly['CI_source_pins'].items())
    assert ux_source['schema'] == 'pr1549_current_main_c28_fixture_correction_independent_ux_source_evidence_refresh_v1'
    assert ux_source['verdict'] == 'BOUNDED_FIXTURE_SOURCE_CAUSE_FIXED; PRODUCTION_UX_SOURCE_ASSESSMENT_RETAINED; EXECUTION_AND_NATIVE_EVIDENCE_HOLD; NOT_READY'
    assert ux_source['ready'] is False and ux_source['merge_ready'] is False and ux_source['limits']['grant'] is None
    assert (ux_source['source']['head'], ux_source['source']['tree'], ux_source['source']['parent']) == (review['head'], review['tree'], review['parent'])
    assert ux_source['source']['freeze']['sha256'] == review['product_freeze']['sha256']
    assert ux_source['current_source_findings']['new_production_ux_blockers'] == []
    assert ux_source['full_pr_native_evidence_contract']['before_revision'] == review['current_main']
    assert ux_source['full_pr_native_evidence_contract']['after_revision'] == review['head']
    assert all(ux_source['source']['pins'][path]['sha256'] == row['sha256'] for path, row in freeze['pins'].items())
    assert publication['schema'] == 'pr1549_c28_current_main_reviewed_source_publication_v1'
    assert (publication['head'], publication['tree']) == (review['head'], review['tree'])
    assert publication['source_adoption']['sha256'] == review['product_adoption']['sha256']
    assert publication['separate_branch']['exit'] == 0 and publication['existing_PR_fastforward']['exit'] == 0 and publication['existing_PR_fastforward']['pr'] == 1549
    assert review['publication'] == {'source_publication_sha256': review['product_publication']['sha256'], 'source_adoption_sha256': review['product_adoption']['sha256'], 'published_source_branch': publication['separate_branch']['branch']}
    adoption_freeze = read_bound('adoption_freeze')
    adoption_semantic = read_bound('adoption_semantic')
    adoption_conservation = read_bound('adoption_conservation')
    adoption_pure4 = read_bound('adoption_pure4_reference')
    adoption_intake = read_bound('adoption_intake')
    adoption_definition = read_bound('adoption_function', False)
    for history_key in ('adoption_original_finding', 'prior_ca_task_hold', 'prior_ca_conservation', 'prior_ca_additive_hold', 'adoption_old_function', 'prior_ca_author_conservation'):
        read_bound(history_key, False)
    adoption = review['adopted_child_settlement']
    assert adoption == data['adopted_child_settlement'] == source_map['adopted_child_settlement']
    assert (adoption['canonical_head'], adoption['canonical_tree'], adoption['canonical_parent']) == (
        'fef275ca853f01cb8fb6dfc698127920e4bcc9b5', 'dec517c58adc9bfc8d0eb466e66498dc5e205e1d',
        '347c2adc5ec7820477c49947b4906e2dc35f3986')
    assert adoption['function_sha256'] == review['adoption_function']['sha256'] == '9004ca5e53e46b257791bb7ccf23c92a450b9bc957e391408e5729f41e307483'
    assert runner_bytes.count(adoption_definition.encode('utf-8')) == 1, 'Live canonical definition differs'
    assert adoption['historical_function_sha256'] == review['adoption_old_function']['sha256'] == '8cb801225fac1539666e597ae5ff0517a8da4bc2a1173156716e528fe1e8537b'
    assert adoption['natural_grace_seconds_unchanged'] == 3
    assert adoption['unproven_adopted_survivors'] == 'UNSIGNALED_UNCLEAN_UNFORCED_RETAIN_ACQUIRED_C_HOLD'
    assert adoption['original_direct_Popen_timeout_wait_pidfd_paths'] == 'UNCHANGED'
    assert adoption['fresh_successor_code_caller_conservation_review'] is None and adoption['execution_authorized'] is False
    assert adoption_freeze['kind'] == 'CANONICAL_EXISTING_FUNCTION_SOURCE_CORRECTION_NOT_EXECUTION_PACKET'
    assert (adoption_freeze['head'], adoption_freeze['tree'], adoption_freeze['parent']) == (
        adoption['canonical_head'], adoption['canonical_tree'], adoption['canonical_parent'])
    assert adoption_freeze['canonical_function_sha256'] == adoption['function_sha256']
    assert adoption_freeze['published'] is False and adoption_freeze['independent_reviews'] == 'PENDING'  # Original immutable snapshot.
    assert adoption_freeze['pure_unit_source']['execution'] == 'UNRUN'  # Later actual pure4 is a separate reference.
    assert adoption_semantic['schema'] == 'bounded-canonical-correction-semantic-caller-review-v1'
    assert adoption_semantic['disposition'] == 'SCOPED_SOURCE_CLEAR' and adoption_semantic['findings'] == []
    assert (adoption_semantic['pins']['head'], adoption_semantic['pins']['tree'], adoption_semantic['pins']['parent']) == (
        adoption['canonical_head'], adoption['canonical_tree'], adoption['canonical_parent'])
    assert adoption_semantic['pins']['freeze']['sha256'] == review['adoption_freeze']['sha256']
    assert adoption_conservation['schema'] == 'canonical-unproven-adoption-policy-conservation-and-pure-unit-source-review-v1'
    assert adoption_conservation['verdict'] == 'BOUNDED_SOURCE_CLEAR_FOR_CONSERVATION_AND_FOUR_PURE_UNITS' and adoption_conservation['findings'] == []
    assert (adoption_conservation['pins']['head'], adoption_conservation['pins']['tree'], adoption_conservation['pins']['parent']) == (
        adoption['canonical_head'], adoption['canonical_tree'], adoption['canonical_parent'])
    assert adoption_conservation['freeze']['sha256'] == review['adoption_freeze']['sha256']
    assert {name: row['sha256'] for name, row in adoption_conservation['files'].items()} == adoption_freeze['source_files']
    assert adoption_conservation['files']['settle_children.py']['sha256'] == adoption['function_sha256']
    assert adoption_intake['original_message']['id'] == 'f2a060a4-4d7b-495c-af5a-e52413c2f9c1'
    assert adoption_pure4['exit_code'] == 0 and adoption_pure4['original_tool_chunk'] == '20cee8'
    assert adoption_pure4['source_pins_after'][adoption_freeze['canonical_function_file']] == adoption['function_sha256']
    assert 'No runnable packet/caller custody' in adoption_pure4['scope']  # No current pass/admission transfer.
    assert data['schema'] == 'pr1549-c28-host3-controller-task-data-v1'
    assert data['source'] == source_map['product'] == {'head': review['head'], 'tree': review['tree'], 'parent': review['parent'], 'worktree': str(WT)}
    assert source_map['source_paths_sha256'] == review['source_paths_sha256'] == plan['source_paths_sha256']
    assert data['runtime_admission'] == review['runtime_admission'] == plan['runtime_admission']
    assert data['actual'] == review['actual'] == plan['actual'] and all(value is None for value in plan['actual'].values())
    assert len(plan['artifact_groups']) == 1 and data['artifact_groups'] == plan['artifact_groups']
    assert data['binary_checks'] == plan['binary_checks'] == [{'label': 'host-binary-check', 'argv': ['cargo', 'check', '--locked', '--no-default-features', '--features', 'host', '--bin', 'cagentsd'], 'seconds': 600}]
    group = plan['artifact_groups'][0]
    assert group['id'] == 'host-lib' and group['features'] == ['host'] and group['manifest'] == 'Cargo.toml'
    assert group['target_name'] == 'cagents' and group['target_kind'] == ['lib']
    assert group['compile_argv'] == ['cargo', 'test', '--locked', '--no-default-features', '--features', 'host', '--lib', '--no-run', '--message-format=json']
    names = [row['name'] for row in group['selectors']]
    assert names == data['selected_names'] == root_adoption['required_fresh_owning_parents']['HOST3']
    assert len(names) == len(set(names)) == 3 and all(row['seconds'] == 90 and row['expected_stdout_child_results'] == 0 for row in group['selectors'])
    assert group['source_predicted_compiled_names'] == names and group['source_predicted_absent_names'] == []
    assert (group['compile_seconds'], group['inventory_seconds']) == (900, 30)
    assert plan['planned_phase_order'] == data['planned_phase_order'] == ['host-binary-check', 'compile-host-lib', 'inventory-host-lib', 'parent-host-lib-1', 'parent-host-lib-2', 'parent-host-lib-3']
    assert plan['planned_phase_count'] == 6 and plan['packet_deadline_seconds'] == 2400 and plan['phase_caps_sum_seconds'] == 1800
    assert plan['lane']['target'] == str(TARGET) and plan['lane']['lock'] == str(LOCK)
    assert plan['lane']['jobs'] == 2 and plan['lane']['new_cache'] is False
    assert plan['required_absent_environment_names'] == ['CAGENTS_UPGRADE_STATE_FIXTURE_SELECTOR', 'RUST_TEST_NOCAPTURE', 'RUST_TEST_THREADS', 'CAGENTS_TEST_RETAINED_ACTOR_HELPER', 'CAGENTS_RETAINED_ACTOR_FIXTURE']
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
    out = Path('/tmp/coord16-pr1549-c28-host3-validation-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '-' + str(os.getpid()))
    out.mkdir(mode=0o700)
    save = lambda name, value: (out / name).write_text(json.dumps(value, indent=2) + '\n')
    # Custody and original bytes are archived before touching the lane lock.
    evidence_copy(out, 'executed-runner.py', runner_bytes, args.runner_sha256)
    evidence_copy(out, 'plan.json', plan_bytes, PLAN_SHA)
    for binding, payload in archive_bytes:
        relative = Path(binding['name'])
        assert relative.parts and not relative.is_absolute() and all(part not in ('.', '..') and re.fullmatch(r'[A-Za-z0-9_.-]+', part) for part in relative.parts), 'Unsafe archive destination'
        archive_parent = out
        for component in relative.parts[:-1]:
            archive_parent = archive_parent / component
            archive_parent.mkdir(mode=0o700, exist_ok=True)
            assert archive_parent.is_dir() and not archive_parent.is_symlink(), 'Unsafe archive parent'
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

    def invoke_fixture_group(label, argv, seconds, contract, helper):
        nonlocal env, before_context
        key = 'CAGENTS_TEST_RETAINED_ACTOR_HELPER'
        original_env, original_context = env, before_context
        assert context() == original_context, 'Context changed before the authorized fixture transition'
        assert key not in original_env and key not in original_context['inherited_cagents_names']
        assert contract['fixture_environment_exception']['value'] == helper['file']['path']
        scoped_env = original_env.copy()
        scoped_env[key] = helper['file']['path']  # Sole added key; never os.environ.
        expected_context = {**original_context,
                            'inherited_cagents_names': sorted([*original_context['inherited_cagents_names'], key])}
        assert expected_context['configs'] == original_context['configs']
        assert expected_context['build_input_hashes'] == original_context['build_input_hashes']
        primary, restoration_error, result = None, None, None
        entered_run = False
        env, before_context = scoped_env, expected_context
        try:
            assert context() == expected_context, 'Fixture binding changed more than its authorized single name'
            save('fixture-enter-' + label + '.json', {
                'label': label, 'fixture_key': key, 'helper_path': helper['file']['path'],
                'helper_sha256': helper['file']['sha256'], 'only_added_environment_key': key,
                'all_original_config_and_build_hash_fields_unchanged': True,
                'scope': 'Inherited environment COPY plus declared fixture key; opaque authority neither inspected nor serialized.'})
            entered_run = True
            result = run(label, argv, seconds, contract)
        except BaseException as exc:
            primary = exc
            if not entered_run:
                supervision_failure(None, exc, 'task-fixture-transition-evidence')
        finally:
            env, before_context = original_env, original_context
            try:
                assert context() == original_context, 'Original environment/context restoration refused'
                save('fixture-restore-' + label + '.json', {
                    'label': label, 'fixture_key_absent_again': key not in env,
                    'original_context_exact': True,
                    'primary_error': {'type': type(primary).__name__, 'message': str(primary)} if primary else None})
            except BaseException as exc:
                restoration_error = exc
                supervision_failure(None, exc, 'task-fixture-context-restoration')
        if primary is not None:
            raise primary
        if restoration_error is not None:
            raise restoration_error
        return result

    run.issue2025_fixture_group = invoke_fixture_group
    run.issue2025_deadline = lambda: deadline
    run.issue2025_controller_binding = {
        'runner_sha256': args.runner_sha256, 'plan_sha256': PLAN_SHA,
        'independent_controller_review_sha256': args.controller_review_sha256,
        'source_binding_sha256': plan['source_review_sha256'],
        'product_head': plan['product_head'], 'product_tree': plan['product_tree'],
        'product_worktree': str(WT), 'embedded_version_qualification': 'NOT_ESTABLISHED'}

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
             'error': error, 'phases': phases, 'test_invocations': tests, 'parent_records_are': 'Logical memberships within original test invocations; no per-name waits', **accounting, 'raw_parent_results': raw,
             'process_attempts': PROCESS_ATTEMPTS, 'process_states': PROCESS_STATES, 'supervision_failures': SUPERVISION_FAILURES,
             'cancellation_signals': CANCELLATION_SIGNALS,
             'custody': custody, 'historical_handoff_clean': historical_handoff_clean,
             'lock_closed': lock_closed, 'lock_unlocked': lock_unlocked, 'lock_close_error': lock_close_error,
             'actual_release': actual_release,
             'release_policy': 'Original Cfd closes only after complete original waits/evidence and clean unforced phase plus both final custody observations. No LOCK_UN is issued. Unknown assessment/handoff or close retains no safe-return claim; ambiguous close is never retried.',
             'owning_commands_qualified': error is None and not CANCELLATION_SIGNALS and accounting['final_group_semantics_qualified'] and len(tests) == 3 and [parent['invocation'] for parent in tests] == ['parent-host-lib-1', 'parent-host-lib-2', 'parent-host-lib-3'] and [phase['label'] for phase in phases] == plan['planned_phase_order'] and actual_release,
             'qualified_scope': 'PR1549_C28_HOST3_PARTIAL_ONLY',
             'full_issue2025_qualification': False,
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
        print(json.dumps({
            'kind': 'ISSUE2025_COMPACT_TERMINAL_V1',
            'head': summary['head'], 'grant': summary['grant'],
            'owning_commands_qualified': summary['owning_commands_qualified'],
            'actual_release': summary['actual_release'], 'lock_closed': summary['lock_closed'],
            'error': None if summary['error'] is None else {
                'preview': summary['error'][:512], 'full_characters': len(summary['error']),
                'abbreviated': len(summary['error']) > 512, 'full_summary_path': str(out / 'summary.json')},
            'phase_record_count': len(phases), 'phase_rows_abbreviated': len(phases) > 5,
            'phases': [{'label': phase['label'], 'exit': phase['exit'],
                        'direct_wait_complete': phase['direct_wait_complete'], 'timed_out': phase['timed_out']}
                       for phase in phases[:5]],
            'counts': {key: summary[key] for key in (
                'attempted_process_count', 'original_wait_settled_process_count', 'unknown_process_count',
                'attempted_phase_count', 'original_wait_settled_phase_count', 'unknown_phase_count', 'unrun_phase_count',
                'attempted_test_invocation_count', 'qualified_test_invocation_count', 'unrun_test_invocation_count',
                'requested_unique_parent_count', 'qualified_unique_parent_count',
                'selected_parent_occurrence_count', 'qualified_parent_occurrence_count')},
            'summary_json': str(out / 'summary.json'), 'sha256sums': str(out / 'SHA256SUMS'),
            'runner_sha256': summary['runner_sha256'], 'plan_sha256': summary['plan_sha256']
        }), flush=True)
        print('EVIDENCE ' + str(out), flush=True)
        print('RELEASE_STATUS ' + ('ACTUAL_CLEAN_C_RELEASE' if summary['actual_release'] else 'NO_SAFE_RELEASE_CLAIM'), flush=True)
    return 0 if summary['owning_commands_qualified'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
