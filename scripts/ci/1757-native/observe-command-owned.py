#!/usr/bin/env python3
"""Observe one planned command with independent owned-tree resource snapshots.

The leader remains an unreaped child until cleanup finishes. Its pidfd and
start/session identity anchor every process-group signal. A subreaper adopts
orphan workers, including workers which deliberately create another session.
No environment or delegation authority is removed, copied or printed.
"""
from pathlib import Path
import argparse
import ctypes
import datetime
import errno
import json
import math
import os
import re
import select
import signal
import subprocess
import threading
import time

TERM_GRACE = 2.0
KILL_GRACE = 3.0
POLL = 0.05
METRIC_IO_FIELDS = ('rchar', 'wchar', 'syscr', 'syscw', 'read_bytes',
                    'write_bytes', 'cancelled_write_bytes')
LINKER_NAMES = frozenset(('ld', 'ld.bfd', 'ld.gold', 'ld.lld', 'lld', 'rust-lld', 'mold'))


def read(path):
    try:
        return Path(path).read_text()
    except OSError:
        return None


def sample(cgroup):
    ticks = (read('/proc/stat') or '').splitlines()
    return {
        'monotonic': time.monotonic(),
        'global_io': read('/proc/pressure/io'),
        'global_cpu': read('/proc/pressure/cpu'),
        'global_memory': read('/proc/pressure/memory'),
        'cpu_ticks': ticks[0] if ticks else None,
        'memory': read('/proc/meminfo'),
        'diskstats': read('/proc/diskstats'),
        'cgroup_io': read(cgroup / 'io.pressure'),
        'cgroup_cpu': read(cgroup / 'cpu.stat'),
        'cgroup_memory': read(cgroup / 'memory.current'),
        'cgroup_memory_events': read(cgroup / 'memory.events'),
        'cgroup_io_stat': read(cgroup / 'io.stat'),
    }


def process_stat(pid):
    try:
        # comm can contain spaces and parentheses; the last ')' ends it.
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return {'pid': pid, 'state': fields[0], 'parent': int(fields[1]),
                'group': int(fields[2]), 'session': int(fields[3]),
                'start_ticks': int(fields[19])}
    except (FileNotFoundError, ProcessLookupError):
        return None


def process_snapshot():
    """Untrusted observational hints, never numeric-ancestry authority."""
    rows = {}
    for entry in Path('/proc').iterdir():
        if entry.name.isdecimal():
            row = process_stat(int(entry.name))
            if row:
                rows[row['pid']] = row
    return rows


def direct_children():
    """Kernel child inventory for this observer, including adopted orphans."""
    children = set()
    for task in Path('/proc/self/task').iterdir():
        try:
            children.update(int(pid) for pid in (task / 'children').read_text().split())
        except FileNotFoundError:
            pass
    return children


def pidfd_pid(fd):
    # Pid becomes -1 after the referenced task is reaped. A reused numeric PID
    # cannot turn this descriptor into authority over the replacement task.
    for line in Path(f'/proc/self/fdinfo/{fd}').read_text().splitlines():
        if line.startswith('Pid:'):
            return int(line.split(':', 1)[1])
    raise RuntimeError('pidfd has no kernel task identity')


def metric_stat(root, pid):
    """Read independent metrics; never touch cleanup's pidfds or ownership map."""
    try:
        raw = (root / str(pid) / 'stat').read_text()
        if int(raw.split(' ', 1)[0]) != pid:
            return None
        fields = raw.rsplit(')', 1)[1].split()
        return {'pid': pid, 'state': fields[0], 'parent': int(fields[1]),
                'group': int(fields[2]), 'session': int(fields[3]),
                'start_ticks': int(fields[19]), 'user_ticks': int(fields[11]),
                'system_ticks': int(fields[12]), 'reaped_user_ticks': int(fields[13]),
                'reaped_system_ticks': int(fields[14]), 'rss_pages': max(0, int(fields[21]))}
    except (OSError, ValueError, IndexError):
        return None


def metric_io(root, pid):
    try:
        rows = dict(line.split(':', 1) for line in (root / str(pid) / 'io').read_text().splitlines())
        result = {name: int(rows[name]) for name in METRIC_IO_FIELDS}
        return result if all(value >= 0 for value in result.values()) else None
    except (OSError, ValueError, KeyError):
        return None


def metric_role(root, pid):
    try:
        # comm only decides a fixed role. Arbitrary process names are not copied.
        name = (root / str(pid) / 'comm').read_text().strip()
    except OSError:
        return 'unknown', None
    if name in LINKER_NAMES:
        return 'linker', name
    if name != 'rustc':
        return 'other', None
    try:
        with (root / str(pid) / 'cmdline').open('rb') as stream:
            # Ignore any final truncated argument, including a partial crate
            # name at the bound. Only a complete NUL-terminated name is reported.
            arguments = stream.read(16384).split(b'\0')[:-1]
        for index, value in enumerate(arguments[:-1]):
            if value == b'--crate-name':
                crate = arguments[index + 1]
                if re.fullmatch(rb'[A-Za-z_][A-Za-z0-9_]{0,127}', crate):
                    return 'rustc', crate.decode('ascii')
                break
    except OSError:
        pass
    return 'rustc', None


def owned_metrics(anchor, observer, root=Path('/proc')):
    """One non-atomic snapshot in this PID namespace, not whole-job accounting.

    Only the reserved leader session and current direct observer children are
    sampled. Numeric intermediate-parent joins are never used. These remain
    observational metrics, not individual-signal authority. Membership/epochs
    are checked again after IO/role reads; the sampler never touches cleanup's
    pidfds or OwnedCommand state.
    """
    if anchor is None:
        return {'status': 'not_started'}
    begin = time.monotonic()
    rows = {}
    unreadable = 0
    try:
        for entry in root.iterdir():
            if entry.name.isdecimal():
                row = metric_stat(root, int(entry.name))
                if row:
                    rows[row['pid']] = row
                else:
                    unreadable += 1
    except OSError:
        return {'status': 'unavailable', 'reason': 'proc_namespace_unreadable'}
    for identity in (anchor, observer):
        current = rows.get(identity['pid']) if identity else None
        if not current or current['start_ticks'] != identity['start_ticks']:
            return {'status': 'unavailable', 'reason': 'retained_epoch_not_visible'}
    owned = {pid for pid, row in rows.items()
             if pid == anchor['pid'] or row['session'] == anchor['pid']
             or row['parent'] == observer['pid']}
    result = {
        'status': 'sampled', 'process_count': 0, 'live_process_count': 0,
        'sum_rss_bytes': 0, 'self_user_ticks': 0, 'self_system_ticks': 0,
        'reaped_descendant_user_ticks': 0, 'reaped_descendant_system_ticks': 0,
        'io': {name: 0 for name in METRIC_IO_FIELDS}, 'io_readable_processes': 0,
        'rustc_processes': 0, 'rustc_crates': {}, 'rustc_unknown_crate': 0,
        'linker_processes': 0, 'linker_binaries': {}, 'unknown_roles': 0,
        'disappeared_or_reused_members': 0, 'unreadable_namespace_entries': unreadable,
        'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
        'scope': 'Observed reserved-session/direct-child subset; excludes escaped descendants until directly adopted and excludes observer. Not whole-job totals or signal authority.',
        'rss_caveat': 'Sum of per-process RSS; shared pages can be counted more than once; kernel RSS accounting is approximate.',
        'counter_caveat': 'Membership and reads are non-atomic. Self CPU and IO sums can fall when a process exits; CPU child fields include reaped descendants. Short-lived processes can be missed. IO is process accounting, not device throughput.',
    }
    page_bytes = os.sysconf('SC_PAGE_SIZE')
    for pid in sorted(owned - {observer['pid']}):
        row = rows.get(pid)
        if not row:
            continue
        io = metric_io(root, pid)
        role, detail = metric_role(root, pid) if row['state'] != 'Z' else ('other', None)
        current = metric_stat(root, pid)
        if (not current or current['start_ticks'] != row['start_ticks']
                or not (pid == anchor['pid'] or current['session'] == anchor['pid']
                        or current['parent'] == observer['pid'])):
            result['disappeared_or_reused_members'] += 1
            continue
        result['process_count'] += 1
        result['live_process_count'] += row['state'] != 'Z'
        result['sum_rss_bytes'] += row['rss_pages'] * page_bytes
        for output, field in [('self_user_ticks', 'user_ticks'), ('self_system_ticks', 'system_ticks'),
                              ('reaped_descendant_user_ticks', 'reaped_user_ticks'),
                              ('reaped_descendant_system_ticks', 'reaped_system_ticks')]:
            result[output] += row[field]
        if io is not None:
            result['io_readable_processes'] += 1
            for name in METRIC_IO_FIELDS:
                result['io'][name] += io[name]
        if role == 'rustc':
            result['rustc_processes'] += 1
            if detail is None:
                result['rustc_unknown_crate'] += 1
            else:
                result['rustc_crates'][detail] = result['rustc_crates'].get(detail, 0) + 1
        elif role == 'linker':
            result['linker_processes'] += 1
            result['linker_binaries'][detail] = result['linker_binaries'].get(detail, 0) + 1
        elif role == 'unknown':
            result['unknown_roles'] += 1
    result['sample_wall_seconds'] = time.monotonic() - begin
    return result


def become_subreaper():
    # PR_SET_CHILD_SUBREAPER is process-local. It neither changes another
    # process nor adopts unrelated host processes.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'cannot retain orphan worker ownership')
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)


class OwnedCommand:
    def __init__(self, command, stdout, stderr):
        self.proc = None
        self.leader_fd = None
        self.identity = None
        self.workers = {}
        self.worker_membership = {}
        self.membership_uncertain = []
        self.events = []
        self.census_initial = []
        self.remaining = []
        self.proc = subprocess.Popen(command, stdout=stdout, stderr=stderr,
                                     start_new_session=True)
        self.leader = self.proc.pid
        self.identity = process_stat(self.leader)
        self.leader_fd = os.pidfd_open(self.leader)
        if not self.identity or self.identity['group'] != self.leader or self.identity['session'] != self.leader:
            raise RuntimeError('new command session identity could not be verified')

    def finished(self):
        # WNOWAIT is essential: Popen.poll()/wait() would release the PID
        # which makes group signalling safe after the leader exits first.
        return os.waitid(os.P_PIDFD, self.leader_fd,
                         os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None

    def reserved_leader(self):
        current = process_stat(self.leader)
        if (not current or not self.identity or pidfd_pid(self.leader_fd) != self.leader
                or current['start_ticks'] != self.identity['start_ticks']
                or current['group'] != self.leader or current['session'] != self.leader):
            raise RuntimeError('retained command session identity was lost')
        # Success (including None for a live child) proves this exact pidfd is
        # still waitable by us. WNOWAIT preserves the leader and its SID/PGID.
        os.waitid(os.P_PIDFD, self.leader_fd, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        return current

    def wait(self, timeout, interrupted):
        deadline = time.monotonic() + timeout
        while True:
            if interrupted():
                return 'interrupted'
            if self.finished():
                return 'exited'
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return 'timeout'
            select.select([self.leader_fd], [], [], min(POLL, remaining))

    def census(self):
        leader = self.reserved_leader()
        rows = process_snapshot()
        # Numeric parent chains from this non-atomic scan are not proof.
        # Only the unreaped leader's reserved session, an already-proven
        # pidfd, or a kernel-proven CURRENT DIRECT child may be signalled.
        candidates = set(self.workers) | direct_children() | {
            pid for pid, row in rows.items()
            if row['session'] == self.leader or row['parent'] == os.getpid()}
        candidates.discard(os.getpid())
        candidates.discard(self.leader)
        result = [{**leader, 'membership': {'kind': 'retained-leader'}}]
        for pid in sorted(candidates):
            row = process_stat(pid)
            if row is None:
                continue
            previous = self.workers.get(pid)
            if previous and pidfd_pid(previous[0]) == pid and previous[1] == row['start_ticks']:
                # This descriptor was proved owned earlier and remains bound
                # to that same task even if it later changes session/parent.
                result.append({**row, 'membership': self.worker_membership[pid]})
                continue
            if previous:
                os.close(previous[0])
                del self.workers[pid]
                self.worker_membership.pop(pid, None)
            fd = None
            try:
                fd = os.pidfd_open(pid)
                current = process_stat(pid)
                if current is None or pidfd_pid(fd) == -1:
                    continue
                if pidfd_pid(fd) != pid or current['start_ticks'] != row['start_ticks']:
                    self.membership_uncertain.append({'pid': pid, 'reason': 'task-identity-changed'})
                    continue
                if current['session'] == self.leader:
                    self.reserved_leader()
                    proof = {'kind': 'reserved-leader-session', 'start_ticks': current['start_ticks']}
                elif current['parent'] == os.getpid():
                    try:
                        os.waitid(os.P_PIDFD, fd, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                    except ChildProcessError:
                        self.membership_uncertain.append({'pid': pid, 'reason': 'not-kernel-waitable-child'})
                        continue
                    proof = {'kind': 'kernel-direct-child', 'start_ticks': current['start_ticks']}
                else:
                    self.membership_uncertain.append({'pid': pid, 'reason': 'membership-not-proven'})
                    continue
                verified = process_stat(pid)
                if verified is None or pidfd_pid(fd) == -1:
                    continue
                if pidfd_pid(fd) != pid or verified['start_ticks'] != current['start_ticks']:
                    self.membership_uncertain.append({'pid': pid, 'reason': 'task-changed-after-proof'})
                    continue
                self.workers[pid] = (fd, verified['start_ticks'])
                self.worker_membership[pid] = proof
                fd = None
                result.append({**verified, 'membership': proof})
            except ProcessLookupError:
                pass
            finally:
                if fd is not None:
                    os.close(fd)
        return result

    def signal_owned(self, sig):
        current = self.reserved_leader()
        # A retained waitable child plus its epoch/session proves this is
        # still our group. Never signal a saved numeric group after reaping.
        if (not current or not self.identity
                or current['start_ticks'] != self.identity['start_ticks']
                or current['group'] != self.leader
                or current['session'] != self.leader):
            raise RuntimeError('retained command group identity was lost')
        try:
            os.killpg(self.leader, sig)
        except ProcessLookupError:
            pass
        for fd, _ in self.workers.values():
            try:
                # pidfds also fence children which escaped the original group.
                signal.pidfd_send_signal(fd, sig)
            except ProcessLookupError:
                pass
        self.events.append({'signal': sig, 'monotonic': time.monotonic()})

    def reap_workers(self, rows):
        for row in rows:
            if row['pid'] == self.leader or row['state'] != 'Z':
                continue
            try:
                worker = self.workers.get(row['pid'])
                if worker:
                    os.waitid(os.P_PIDFD, worker[0], os.WEXITED | os.WNOHANG)
            except ChildProcessError:
                # A still-live owned parent may not have reaped its child yet.
                pass

    def clean(self):
        self.census_initial = self.census()
        for sig, grace in [(signal.SIGTERM, TERM_GRACE), (signal.SIGKILL, KILL_GRACE)]:
            self.signal_owned(sig)
            deadline = time.monotonic() + grace
            while True:
                rows = self.census()
                self.reap_workers(rows)
                live = [row for row in rows if row['state'] != 'Z']
                if not live and self.finished():
                    # One fresh census after orphan reaping also catches
                    # children forked while their parent was terminating.
                    remaining = self.census()
                    self.reap_workers(remaining)
                    if (all(row['pid'] == self.leader or row['state'] == 'Z' for row in remaining)
                            and direct_children() <= {self.leader}):
                        self.proc.wait(timeout=0)
                        self.remaining = []
                        return not self.membership_uncertain
                if time.monotonic() >= deadline:
                    break
                # Catch new or escaped descendants by pidfd on every pass.
                if any(row['session'] != self.leader for row in live):
                    for row in live:
                        worker = self.workers.get(row['pid'])
                        if worker:
                            try:
                                signal.pidfd_send_signal(worker[0], sig)
                            except ProcessLookupError:
                                pass
                time.sleep(POLL)
        self.remaining = self.census()
        return False

    def close_descriptors(self):
        for fd in [self.leader_fd, *(fd for fd, _ in self.workers.values())]:
            if fd is not None:
                os.close(fd)


def artifacts(stdout, output):
    counts = {}
    rows = []
    with stdout.open(errors='replace') as stream:
        for line in stream:
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get('reason') != 'compiler-artifact':
                continue
            path = event.get('manifest_path', '')
            origin = 'checkout' if path.startswith(os.getcwd() + '/') else 'external'
            key = origin + ('_fresh' if event.get('fresh') else '_rebuilt')
            counts[key] = counts.get(key, 0) + 1
            rows.append({'package_id': event.get('package_id'), 'manifest_path': path,
                         'fresh': event.get('fresh'), 'target': event.get('target', {}).get('name')})
    (output / 'CARGO-ARTIFACTS.json').write_text(json.dumps({'counts': counts, 'artifacts': rows}, indent=2) + '\n')
    return counts


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True)
    parser.add_argument('--timeout', type=float, required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error('a command and positive timeout are required')
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=False)
    relative = next((line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines()
                     if line.startswith('0::')), None)
    cgroup = Path('/sys/fs/cgroup') / (relative or '').lstrip('/')
    metadata = {
        'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'command': command,
        'cwd': os.getcwd(), 'cgroup': str(cgroup), 'pid_namespace': os.readlink('/proc/self/ns/pid'),
        'source_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'source_tree': subprocess.check_output(['git', 'rev-parse', 'HEAD^{tree}'], text=True).strip(),
        'cargo_jobs': os.environ.get('CARGO_BUILD_JOBS'),
        'cargo_target_dir': os.environ.get('CARGO_TARGET_DIR'),
        'cargo_build_build_dir': os.environ.get('CARGO_BUILD_BUILD_DIR'), 'status': 'running',
        'authority': 'inherited unchanged; value not inspected',
        'owned_metrics_schema': 1,
        'owned_metrics_source': 'https://docs.kernel.org/filesystems/proc.html',
    }
    (output / 'COMMAND.json').write_text(json.dumps(metadata, indent=2) + '\n')
    done = threading.Event()
    sample_wake = threading.Event()
    metric_lock = threading.Lock()
    metric_anchor = [None]
    metric_observer = process_stat(os.getpid())
    interrupted = []
    # These handlers request cleanup rather than interrupting it mid-reap.
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda number, _frame: interrupted.append(number))
    def resource_sample():
        result = sample(cgroup)
        with metric_lock:
            anchor = metric_anchor[0].copy() if metric_anchor[0] is not None else None
        try:
            result['owned'] = owned_metrics(anchor, metric_observer)
        except Exception as error:
            # Metrics are observational. They cannot take ownership away from
            # the sole cleanup path or print raw argv/exception payloads.
            result['owned'] = {'status': 'unavailable', 'error_type': type(error).__name__}
        return result
    def observe():
        with (output / 'RESOURCE-SAMPLES.jsonl').open('w') as stream:
            while True:
                stream.write(json.dumps(resource_sample()) + '\n')
                stream.flush()
                if done.is_set():
                    break
                sample_wake.wait(1)
                sample_wake.clear()
    observer = threading.Thread(target=observe, daemon=True)
    observer.start()
    begin = time.monotonic()
    owned = None
    reason = 'exception'
    exit_code = 1
    try:
        become_subreaper()
        with (output / 'stdout.log').open('wb') as stdout, (output / 'stderr.log').open('wb') as stderr:
            if interrupted:
                reason = 'interrupted'
            else:
                # Assign ownership before initialization can fail, so every
                # exception after spawn still enters the same cleanup path.
                owned = OwnedCommand.__new__(OwnedCommand)
                owned.proc = None
                OwnedCommand.__init__(owned, command, stdout, stderr)
                metadata['leader'] = owned.identity
                with metric_lock:
                    metric_anchor[0] = owned.identity.copy()
                sample_wake.set()
                reason = owned.wait(args.timeout, lambda: bool(interrupted))
    except BaseException as error:
        metadata['exception_type'] = type(error).__name__
    finally:
        cleanup_ok = True
        if owned is not None and owned.proc is not None:
            try:
                cleanup_ok = owned.clean()
            except BaseException as error:
                cleanup_ok = False
                metadata['cleanup_exception_type'] = type(error).__name__
            metadata['cleanup'] = {'complete': cleanup_ok, 'signals': owned.events,
                                   'initial_census': owned.census_initial,
                                   'remaining': owned.remaining,
                                   'membership_uncertain': owned.membership_uncertain,
                                   'membership_proofs': owned.worker_membership,
                                   'leader_reaped': owned.proc.returncode is not None}
            metadata['command_exit'] = owned.proc.returncode
            owned.close_descriptors()
        metadata['exit_reason'] = reason
        metadata['timed_out'] = reason == 'timeout'
        if interrupted:
            metadata['interrupted_signal'] = interrupted[0]
            exit_code = 128 + interrupted[0]
        elif reason == 'timeout':
            exit_code = 124
        elif reason == 'exited' and owned is not None:
            command_exit = owned.proc.returncode
            exit_code = (command_exit if command_exit >= 0 else 128 - command_exit) if command_exit is not None else 1
        if not cleanup_ok:
            exit_code = 125
        metadata.update(status='closed' if cleanup_ok else 'cleanup_failed', actual_exit=exit_code,
                        wall_seconds=time.monotonic() - begin)
        done.set()
        sample_wake.set()
        observer.join(timeout=5)
        metadata['sampler_closed'] = not observer.is_alive()
        (output / 'COMMAND.json').write_text(json.dumps(metadata, indent=2) + '\n')
    counts = artifacts(output / 'stdout.log', output) if (output / 'stdout.log').exists() else {}
    print(json.dumps({'actual_exit': exit_code, 'wall_seconds': metadata['wall_seconds'],
                      'cleanup_complete': metadata.get('cleanup', {}).get('complete', True),
                      'cargo_artifacts': counts, 'records': str(output)}), flush=True)
    raise SystemExit(exit_code)


if __name__ == '__main__':
    main()
