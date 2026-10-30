"""Prepared Linux custody for this wrapper's NEW children only; UNRUN source."""
import ctypes, errno, fcntl, json, os, pathlib, select, signal, stat, subprocess, time
from types import SimpleNamespace

MAX_CHILDREN = 4096
MAX_THREADS = 512

def require(value, message):
    if not value: raise RuntimeError(message)

def process_state(pid):
    directory = pathlib.Path('/proc') / str(pid)
    text = (directory / 'stat').read_text()
    values = text.rpartition(') ')[2].split()
    require(len(values) > 19, 'Invalid owned-process stat')
    info = directory.stat()
    return {'pid': pid, 'state': values[0], 'ppid': int(values[1]),
            'start_ticks': int(values[19]), 'uid': info.st_uid}

def child_ids(pid):
    result = set()
    tasks = pathlib.Path('/proc') / str(pid) / 'task'
    try: threads = list(tasks.iterdir())
    except FileNotFoundError: return result
    require(len(threads) <= MAX_THREADS, 'Owned process exceeds thread bound')
    for thread in threads:
        try: result.update(int(v) for v in (thread / 'children').read_text().split())
        except FileNotFoundError: continue
    require(len(result) <= MAX_CHILDREN, 'Owned process exceeds child bound')
    return result

class Custody:
    def __init__(self, cancelled):
        self.cancelled = cancelled
        self.pid, self.uid = os.getpid(), os.getuid()
        self.boot_id = pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        self.wrapper = process_state(self.pid)
        require(not child_ids(self.pid), 'Wrapper already has children before custody admission')
        require(hasattr(os, 'pidfd_open') and hasattr(signal, 'pidfd_send_signal'), 'Linux pidfds required')
        libc = ctypes.CDLL(None, use_errno=True)
        require(libc.prctl(36, 1, 0, 0, 0) == 0, 'Cannot become child subreaper')
        actual = ctypes.c_int()
        require(libc.prctl(37, ctypes.byref(actual), 0, 0, 0) == 0 and actual.value == 1,
                'Child subreaper was not established')
        require(not child_ids(self.pid), 'Unexpected child during subreaper admission')
        self.nodes, self.direct = {}, {}
        self.daemon = None
        self.expected_daemon = None
        self.launches = 0
        self.closed = False

    def exited(self, node):
        if node.get('settled'): return True
        poller = select.poll()
        poller.register(node['pidfd'], select.POLLIN)
        events = poller.poll(0)
        require(not any(flags & select.POLLNVAL for _, flags in events), 'Owned pidfd became invalid')
        return any(flags & select.POLLIN for _, flags in events)

    def current(self, node, executable=False):
        value = process_state(node['pid'])
        require(value['start_ticks'] == node['start_ticks'] and value['uid'] == self.uid,
                'Owned process incarnation changed')
        if executable and value['state'] not in ('Z', 'X'):
            value['executable'] = os.readlink(f"/proc/{node['pid']}/exe")
            info = os.stat(f"/proc/{node['pid']}/exe")
            value['executable_identity'] = [info.st_dev, info.st_ino]
        return value

    def pin(self, pid, parent, direct=False, role=None):
        if pid in self.nodes:
            node = self.nodes[pid]
            require(not node.get('settled'), 'Observed a reused owned PID; refusing retargeting')
            self.current(node)
            return node
        require(len(self.nodes) < MAX_CHILDREN, 'Total owned-child inventory bound exceeded')
        descriptor = os.pidfd_open(pid, 0)  # Open before validating the kernel cookie.
        try:
            value = process_state(pid)
            require(value['uid'] == self.uid and value['start_ticks'] >= self.wrapper['start_ticks'],
                    'Child is not newly owned by this wrapper uid')
            require(value['ppid'] == self.pid or value['ppid'] in self.nodes,
                    'Child provenance is not an owned descendant/adoption')
            if value['ppid'] in self.nodes:
                self.current(self.nodes[value['ppid']])
            require(direct or parent == self.pid or parent in self.nodes, 'Unproved discovery parent')
            node = {**value, 'pidfd': descriptor, 'discovered_from': parent,
                    'provenance': 'direct Popen child' if direct else
                        ('adopted after verified empty-initial-child subreaper' if value['ppid'] == self.pid else 'observed owned descendant'),
                    'role': role, 'settled': False, 'signals': []}
            self.nodes[pid] = node
            current = self.current(node, executable=True)
            node.update({k: v for k, v in current.items() if k in ('executable', 'executable_identity')})
            return node
        except BaseException:
            os.close(descriptor)
            self.nodes.pop(pid, None)
            raise

    def reap(self, node):
        if node.get('settled'): return True
        if not self.exited(node): return False
        pid = node['pid']
        popen = self.direct.get(pid)
        if popen is not None:
            code = popen.poll()
            if code is not None:
                node.update(settled=True, exit_code=code, exit_status_source='original Popen.wait/poll reaping')
                return True
            return False
        try:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                node.update(settled=True, exit_code=os.waitstatus_to_exitcode(status),
                            wait_status=status, exit_status_source='exact adopted-child waitpid')
                return True
        except ChildProcessError:
            # ECHILD alone never proves completed reaping.
            try: current = process_state(pid)
            except FileNotFoundError: current = None
            if current is None or current['start_ticks'] != node['start_ticks']:
                node.update(settled=True, exit_code=None,
                            exit_status_source='original pidfd exit AND original-process disappearance; parent reaped, status unavailable')
                return True
        return False  # An exited unreaped zombie remains outstanding.

    def scan(self):
        # Only our all-thread child tree is enumerated; no global /proc/name scan.
        pending = [self.pid]
        seen = set()
        while pending:
            parent = pending.pop()
            if parent in seen: continue
            seen.add(parent)
            require(len(seen) <= MAX_CHILDREN + 1, 'Owned traversal exceeded bounds')
            if parent != self.pid:
                node = self.nodes[parent]
                if self.reap(node): continue
                try: self.current(node)
                except FileNotFoundError:
                    require(self.exited(node), 'Owned parent vanished without pidfd exit')
                    continue
            for pid in child_ids(parent):
                try: node = self.pin(pid, parent)
                except ProcessLookupError: continue
                except FileNotFoundError: continue
                if not self.reap(node): pending.append(pid)
        for node in self.nodes.values(): self.reap(node)

    def launch(self, argv, cwd, role, **kwargs):
        require(not self.cancelled() and not self.closed, 'Cancellation/closure refuses new workload')
        self.scan()
        require(not self.cancelled(), 'Cancellation refuses workload before Popen')
        child = subprocess.Popen(argv, cwd=cwd, close_fds=True,
                                 stdin=kwargs.pop('stdin', subprocess.DEVNULL), **kwargs)
        self.launches += 1
        self.direct[child.pid] = child
        try: self.pin(child.pid, self.pid, direct=True, role=role)
        except (FileNotFoundError, ProcessLookupError):
            code = child.poll()
            require(code is not None, 'New child vanished before custody without original wait status')
            self.nodes[child.pid] = {'pid': child.pid, 'role': role, 'settled': True,
                'exit_code': code, 'exit_status_source': 'direct Popen reaped before live pin',
                'provenance': 'direct Popen child', 'signals': []}
        self.scan()
        return child

    def signal_owned(self, node, signum):
        if self.reap(node): return
        current = self.current(node, executable=True)
        require(current['state'] not in ('Z', 'X'), 'Do not signal an exited child')
        if node.get('pinned_executable'):
            require(current.get('executable') == node['pinned_executable'] and
                    current.get('executable_identity') == node['pinned_executable_identity'],
                    'Pinned UI/daemon executable incarnation changed; no retargeting')
        if current.get('executable') == self.expected_daemon:
            require(self.daemon is node, 'Matched daemon lacks private held-lock corroboration')
        try: signal.pidfd_send_signal(node['pidfd'], signum, None, 0)
        except ProcessLookupError:
            require(self.exited(node), 'Owned signal lost its process without pidfd exit')
        node['signals'].append({'signal': signum, 'monotonic': time.monotonic(),
                                'current_executable': current.get('executable'),
                                'current_executable_identity': current.get('executable_identity')})

    def capture(self, argv, cwd, role, timeout, input=None, on_incomplete=None):
        child = self.launch(argv, cwd, role, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            stdin=subprocess.PIPE if input is not None else subprocess.DEVNULL)
        deadline, partial, payload = time.monotonic() + timeout, b'', input
        disposition = None
        while True:
            self.scan()
            if self.cancelled(): disposition = 'cancelled'; break
            remaining = deadline - time.monotonic()
            if remaining <= 0: disposition = 'timeout'; break
            try:
                output, _ = child.communicate(input=payload, timeout=min(remaining, 0.1))
                self.scan()
                return SimpleNamespace(stdout=output, returncode=child.returncode,
                    state='finished', original_exit=child.returncode, timeout_seconds=timeout,
                    cleanup_exit=None)
            except subprocess.TimeoutExpired as error:
                if error.output is not None: partial = error.output
                payload = None
        # Preserve the original unknown exit plus partial stdout. Termination is
        # owned-child cleanup, never a retry or a substitute successful command.
        node = self.nodes[child.pid]
        retention_error = None
        try:
            require(on_incomplete is not None, 'Incomplete-command retention callback required')
            on_incomplete(SimpleNamespace(stdout=partial, state=disposition, returncode=None,
                original_exit=None, timeout_seconds=timeout, cleanup_exit=None))
        except BaseException as error:
            retention_error = type(error).__name__ + ': ' + str(error)
        self.signal_owned(node, signal.SIGTERM)
        started = time.monotonic()
        while not self.reap(node):
            self.scan()
            if time.monotonic() - started >= 5 and not any(v['signal'] == signal.SIGKILL for v in node['signals']):
                self.signal_owned(node, signal.SIGKILL)
            time.sleep(0.1)
        try:
            output, _ = child.communicate(timeout=1)
            if output is not None: partial = output
        except subprocess.TimeoutExpired as error:
            if error.output is not None: partial = error.output
        return SimpleNamespace(stdout=partial, returncode=None, state=disposition,
            original_exit=None, timeout_seconds=timeout, cleanup_exit=child.returncode,
            retention_error=retention_error)

    def exact_matches(self, executable):
        matches = []
        for node in self.nodes.values():
            if self.reap(node): continue
            try:
                current = self.current(node, executable=True)
            except OSError as error:
                if error.errno not in (errno.ENOENT, errno.ESRCH): raise
                # Procfs may disappear after the live check. Only the original
                # pidfd's positive exit permits omitting this live candidate.
                require(self.exited(node), 'Owned metadata vanished without original pidfd exit')
                self.reap(node)  # Keep inventory and any remaining reaping obligation.
                continue
            if current.get('executable') == executable:
                expected = os.stat(executable)
                require(current['executable_identity'] == [expected.st_dev, expected.st_ino],
                        'Matched executable was replaced')
                matches.append((node, current))
        return matches

    def corroborate_daemon(self, runtime, executable):
        self.expected_daemon = executable
        self.scan()
        matches = self.exact_matches(executable)
        require(len(matches) <= 1, 'Ambiguous newly owned matched daemon children')
        base = runtime / 'cagents-development'
        if os.path.lexists(base):
            info = base.lstat()
            require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode) and
                    info.st_uid == self.uid and info.st_mode & 0o077 == 0,
                    'Unsafe private daemon directory')
        if not matches:
            # A held private lock without an already tracked owned candidate is
            # ambiguity, not permission to discover/signal its metadata PID.
            path = runtime / 'cagents-development/cagentsd.lock'
            if os.path.lexists(path):
                fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
                try:
                    info = os.fstat(fd)
                    require(stat.S_ISREG(info.st_mode) and info.st_uid == self.uid and
                            info.st_nlink == 1 and info.st_mode & 0o077 == 0,
                            'Unsafe unmatched private daemon lock')
                    try: fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        raise RuntimeError('Held private lock has no already owned live daemon candidate')
                    else: fcntl.flock(fd, fcntl.LOCK_UN)
                finally: os.close(fd)
            return None
        node, current = matches[0]
        if self.daemon is node: return node
        require(self.daemon is None, 'Daemon successor is not admitted by this one-attempt wrapper')
        base = runtime / 'cagents-development'
        base_info = base.lstat()
        require(stat.S_ISDIR(base_info.st_mode) and not stat.S_ISLNK(base_info.st_mode) and
                base_info.st_uid == self.uid and base_info.st_mode & 0o077 == 0,
                'Unsafe private daemon directory')
        path = base / 'cagentsd.lock'
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            info = os.fstat(fd)
            require(stat.S_ISREG(info.st_mode) and info.st_uid == self.uid and
                    info.st_nlink == 1 and info.st_mode & 0o077 == 0 and 0 < info.st_size <= 16384,
                    'Unsafe private daemon lock')
            data = os.read(fd, 16385)
            metadata = json.loads(data)
            require(type(metadata) is dict and set(metadata) == {'pid', 'proof'} and
                    type(metadata['pid']) is int and metadata['pid'] == node['pid'],
                    'Private lock owner differs from already owned daemon')
            proof = metadata['proof']
            require(type(proof) is dict and set(proof) == {'host_session_id', 'boot_id', 'host_incarnation'} and
                    all(type(value) is str and value for value in proof.values()) and
                    proof['boot_id'] == self.boot_id, 'Invalid private daemon proof')
            holder = None
            descriptors = list((pathlib.Path('/proc') / str(node['pid']) / 'fd').iterdir())
            require(len(descriptors) <= MAX_CHILDREN, 'Owned daemon descriptor bound exceeded')
            for descriptor in descriptors:
                try:
                    actual = descriptor.stat()
                    if (actual.st_dev, actual.st_ino) != (info.st_dev, info.st_ino): continue
                    contents = (descriptor.parent.parent / 'fdinfo' / descriptor.name).read_text()
                except FileNotFoundError: continue
                for line in contents.splitlines():
                    if not line.startswith('lock:'): continue
                    fields = line[5:].split()
                    if len(fields) != 8: continue
                    device = fields[5].split(':')
                    if (fields[1:4] == ['FLOCK', 'ADVISORY', 'WRITE'] and int(fields[4]) == node['pid'] and
                        fields[6:] == ['0', 'EOF'] and len(device) == 3 and
                        [int(device[0], 16), int(device[1], 16), int(device[2])] ==
                        [os.major(info.st_dev), os.minor(info.st_dev), info.st_ino]): holder = descriptor.name
            require(holder is not None, 'Owned daemon does not hold this exact private lock')
            after = os.fstat(fd)
            identity = lambda value: (value.st_dev, value.st_ino, value.st_uid, value.st_mode,
                                       value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
            named = path.lstat()
            require(identity(info) == identity(after) and identity(after) == identity(named),
                    'Private daemon lock changed during corroboration')
            checked = self.current(node, executable=True)
            require(checked.get('executable') == current.get('executable') and
                    checked.get('executable_identity') == current.get('executable_identity'),
                    'Daemon executable changed while corroborating its private lock')
            node['daemon_lock'] = {'path': str(path), 'dev': info.st_dev, 'ino': info.st_ino,
                                   'holder_fd': holder, 'metadata': metadata,
                                   'kernel_start_ticks': node['start_ticks'], 'boot_id': self.boot_id}
            node['role'] = 'matched-private-daemon'
            node['pinned_executable'] = current['executable']
            node['pinned_executable_identity'] = current['executable_identity']
            self.daemon = node
            return node
        finally: os.close(fd)

    def snapshot(self):
        self.scan()
        return {'subreaper': True, 'initial_children_empty': True, 'boot_id': self.boot_id,
                'wrapper': self.wrapper, 'launch_count': self.launches,
                'nodes': [{k: v for k, v in node.items() if k != 'pidfd'} for node in self.nodes.values()]}

    def settle_all(self, daemon_observer, report):
        self.closed = True
        phase = 'daemon-first'
        last_error = None
        while True:
            try:
                self.scan()
                daemon_observer()  # The lock is corroboration, never a PID selector.
                outstanding = [node for node in self.nodes.values() if not self.reap(node)]
                daemon = self.daemon
                if phase == 'daemon-first' and daemon is not None and not self.exited(daemon):
                    group = [daemon]
                else:
                    phase = 'nonserver'
                    group = [node for node in outstanding if node.get('role') != 'owned-Xvfb' and not self.exited(node)]
                    if not group:
                        phase = 'server-last'
                        group = [node for node in outstanding if node.get('role') == 'owned-Xvfb' and not self.exited(node)]
                for node in group:
                    if not node['signals']: self.signal_owned(node, signal.SIGTERM)
                    elif (time.monotonic() - node['signals'][0]['monotonic'] >= 5 and
                          not any(item['signal'] == signal.SIGKILL for item in node['signals'])):
                        self.signal_owned(node, signal.SIGKILL)
                self.scan()
                if not child_ids(self.pid) and all(self.reap(node) for node in self.nodes.values()):
                    return self.snapshot()
            except BaseException as error:
                # Refuse retargeting, new launches or C release. Repeated signals
                # only record cancellation; they cannot abandon this settlement.
                signature = (type(error).__name__, str(error))
                if signature != last_error:
                    report('owned-child-settlement-blocked', error)
                    last_error = signature
            time.sleep(0.1)

    def close_pidfds(self):
        require(not child_ids(self.pid) and all(node.get('settled') for node in self.nodes.values()),
                'Cannot close custody before positive child settlement')
        for node in self.nodes.values():
            descriptor = node.pop('pidfd', None)
            if descriptor is not None: os.close(descriptor)
