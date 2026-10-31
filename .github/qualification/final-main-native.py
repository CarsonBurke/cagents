"""One real chooser/peer-recipe flow on each exact revision, never full100.

Only observations retry stale accessibility references. Native input is issued
once. Real GTK Cancel and the portal Response drive the production callback.
"""
import ast
import ctypes
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import traceback

import pyatspi
from gi.repository import Gio, GLib
from PIL import Image

A = '00000000-0000-4000-8000-000000002018'
B = '00000000-0000-4000-8000-000000002019'
TEXT = 'Keep this unsent café image draft through the actual chooser.'
SUFFIX = ' Keyboard focus stayed here.'
BAD_TEXT = 'Rejected sibling keeps its saved café draft.\nNever submit or start it.'
RENDERER = {'ICED_BACKEND': 'wgpu', 'WGPU_BACKEND': 'gl', 'LIBGL_ALWAYS_SOFTWARE': '1'}
children = []
ownership_errors = []
cancellation = None
preparation = None
stopped = set()
out = None
arm = None
ledger = None


class DriverLedger:
    """Driver-created originals/writers only; never an outside-family census."""
    def __init__(self, clock, lock):
        self.clock,self.lock = clock,lock
        self.scope = 'global-driver'
        self.entered = []
        self.creations,self.writers,self.callbacks = [],[],[]
        self.streams,self.handles = {},{}
        self.original_streams = []
        self.monitors = []
        self.fenced = self.sealed = False
        self.binding = None
        self.kernel = None
        self.subreaper = False

    def enter(self, label):
        with self.lock:
            assert not self.fenced and label in ('before','after') and label not in self.entered
            self.entered.append(label)
            self.scope = label

    def attempt(self, argv, cwd, role, mount):
        with self.lock:
            assert not self.fenced and not self.sealed, 'Driver spawning phase is closed'
            assert role is not None and len(self.creations) < 4096, 'Creation role/count unresolved'
            row = dict(id='creation-'+str(len(self.creations)+1),scope=self.scope,role=role,
                argv=list(map(str,argv)),cwd=None if cwd is None else str(cwd),attempted_ns=self.clock(),
                pid=None,identity=None,pidfd=None,pidfd_note='Not acquired; authority is the original retained Popen.',
                expected_mount=None if mount is None else str(mount),returned=False,published=False,
                error=None,writers=[],waits=[],terminal=None,signals=[],forced=False)
            self.creations.append(row)
            return row

    def returned(self, process, row, options):
        with self.lock:
            self.handles[id(process)] = row
            row.update(returned=True,pid=process.pid)
        for name in ('stdin','stdout','stderr'):
            supplied = options.get(name)
            if id(supplied) in self.streams:
                self.inherit(supplied,row)
            stream = getattr(process,name,None)
            if stream is not None:
                self.retain_stream(stream,'<'+row['id']+'.'+name+'>','child-PIPE',row=row)

    def reserve_writer(self, path, kind):
        with self.lock:
            assert not self.sealed and len(self.writers) < 16384, 'Writer phase/count unresolved'
            row = dict(id='writer-'+str(len(self.writers)+1),scope=self.scope,path=str(path),kind=kind,
                       acquired=False,closed=False,error=None,inherited_by=[],callback=None)
            self.writers.append(row)
            return row

    def retain_stream(self, stream, path=None, kind=None, row=None, writer=None):
        self.original_streams.append((stream,writer))  # Before fallible ledger publication.
        with self.lock:
            if id(stream) not in self.streams:
                writer = writer or self.reserve_writer(path,kind)
                self.streams[id(stream)] = (stream,writer)
                writer['acquired'] = True
            if row is not None: self.inherit(stream,row)
            return self.streams[id(stream)][1]

    def inherit(self, stream, row):
        with self.lock:
            writer = self.streams[id(stream)][1]
            if row['id'] not in writer['inherited_by']: writer['inherited_by'].append(row['id'])
            if writer['id'] not in row['writers']: row['writers'].append(writer['id'])

    def record_wait(self, process, method, began, code=None, error=None):
        with self.lock:
            row = self.handles[id(process)]
            result = dict(method=method,start_ns=began,finish_ns=self.clock(),exit_code=code,
                          error=None if error is None else repr(error))
            row['waits'].append(result)
            if error is None and type(code) is int: row['terminal'] = result

    def callback(self, output):
        with self.lock:
            assert not self.fenced and not self.sealed
            row = dict(id='callback-'+str(len(self.callbacks)+1),scope=self.scope,path=str(output),
                accepting=False,inflight=0,closed=False,filter_id=None,filter_removed=False,
                monitor='uncreated',query='uncreated',initialized=False,errors=[])
            self.callbacks.append(row)
            return row

    def errors(self):
        errors = []
        if self.binding is None: errors.append('Driver source/instance binding UNKNOWN')
        by_id = {row['id']:row for row in self.creations}
        for row in self.creations:
            if not row['returned'] or row['identity'] is None or not row['published'] or row['error'] or row['terminal'] is None:
                errors.append(row['id']+' original creation/role/wait UNKNOWN')
            if row['forced']: errors.append(row['id']+' required forced termination')
        for row in self.writers:
            if not row['acquired'] or not row['closed'] or row['error']:
                errors.append(row['id']+' parent stream UNKNOWN')
            if any(by_id[child]['terminal'] is None for child in row['inherited_by']):
                errors.append(row['id']+' inherited writer terminal UNKNOWN')
        for row in self.callbacks:
            if not row['initialized'] or row['accepting'] or row['inflight'] or not row['closed'] or row['errors']:
                errors.append(row['id']+' callback writer UNKNOWN')
        return errors

    def snapshot(self, errors, work_error, requested_cancel):
        with self.lock:
            assert self.fenced and not self.sealed
            self.sealed = True
            return dict(kind='driver-role-writer-ledger-v1',scope='This original driver only; pre-exit statement.',
                binding=self.binding,entered_arms=list(self.entered),creations=self.creations,
                writers=self.writers,callbacks=self.callbacks,kernel=self.kernel,
                no_future_spawns=True,errors=list(errors),first_terminal_diagnostic=work_error,
                requested_cancel=requested_cancel,
                outside_scope=['Xvfb/private session-bus roots and activated families','Driver later exit and captured parent streams',
                    'Native capsule/staging, whole-job/executor return and independent acceptance'],
                final_publication=('PARTIAL UNKNOWN; unsettled writers may remain.' if errors else
                    'This sidecar is the final driver native-payload write; its own bytes are not self-hashed.'))


def open_parent(path, mode, kind='parent-log'):
    with cancellation.defer():
        row = ledger.reserve_writer(path,kind)
        try:
            stream = path.open(mode)
            ledger.retain_stream(stream,writer=row)
            return stream
        except BaseException as error:
            row['error'] = repr(error)
            cancellation.latch(error)
            raise


def close_parent(stream):
    if stream is None: return
    with cancellation.defer():
        if id(stream) not in ledger.streams:
            # Close the retained original object even when metadata publication
            # failed; do not convert that failure into a repaired ledger.
            stream.close()
            raise RuntimeError('Original stream publication UNKNOWN after actual close')
        row = ledger.streams[id(stream)][1]
        if row['closed']: return
        if row['callback'] is not None:
            callback = next(value for value in ledger.callbacks if value['id']==row['callback'])
            assert not callback['accepting'] and callback['inflight']==0, 'Callback stream still admitted/inflight'
        try:
            stream.close()
            row['closed'] = True
        except BaseException as error:
            row['error'] = repr(error)
            cancellation.latch(error)
            raise


def write_payload(path, data, append=False):
    with cancellation.defer():
        stream = open_parent(path,'ab' if append else 'wb','synchronous-payload')
        first = None
        try:
            stream.write(data.encode() if isinstance(data,str) else data)
        except BaseException as error:
            first = error; cancellation.latch(error)
            ledger.streams[id(stream)][1]['error'] = repr(error)
        finally:
            try: close_parent(stream)
            except BaseException as error:
                if first is None: first = error
        if first is not None: raise first


def original_wait(process, timeout, method='wait'):
    began = time.time_ns()
    try:
        code = process.wait(timeout=timeout)
    except BaseException as error:
        ledger.record_wait(process,method,began,error=error)
        raise
    ledger.record_wait(process,method,began,code=code)
    return code


def save_image(image, path):
    with cancellation.defer():
        stream = open_parent(path,'wb','synchronous-image')
        first = None
        try: image.save(stream,format='WEBP',lossless=True)
        except BaseException as error:
            first = error; cancellation.latch(error)
            ledger.streams[id(stream)][1]['error'] = repr(error)
        finally:
            try: close_parent(stream)
            except BaseException as error:
                if first is None: first = error
        if first is not None: raise first


def backup_database(source, clone):
    retained = []
    first = None
    with cancellation.defer():
        try:
            for path,options in [('file:'+str(source)+'?mode=ro',dict(uri=True)),(str(clone),{})]:
                row = ledger.reserve_writer(path,'synchronous-sqlite')
                try:
                    connection = sqlite3.connect(path,**options)
                    retained.append(connection)
                    ledger.retain_stream(connection,writer=row)
                except BaseException as error:
                    row['error'] = repr(error)
                    raise
            retained[0].backup(retained[1])
        except BaseException as error:
            first = error; cancellation.latch(error)
        finally:
            for connection in reversed(retained):
                try: close_parent(connection)
                except BaseException as error:
                    if first is None: first = error
        if first is not None: raise first


class NativeCancelled(RuntimeError):
    pass


class Cancellation:
    """Defer Python cancellation delivery, without masking child OS signals."""
    def __init__(self):
        self.depth = 0
        self.signum = None
        self.delivered = False
        self.finalizing = False
        self.interruptible = False
        self.failure = None
        self.cancel_error = None

    def latch(self, error):
        if not isinstance(error, NativeCancelled) and self.failure is None:
            self.failure = error

    def request(self, signum, frame):
        if self.signum is None:
            self.cancel_error = NativeCancelled('Native integration cancelled by signal ' + str(signum))
            self.signum = signum
        # An active exception may be recoverable observation churn, not a
        # terminal diagnostic. Defer delivery without classifying that error.
        # Actual work/unwind owners explicitly latch terminal failures.
        if sys.exc_info()[1] is None and self.interruptible:
            self.checkpoint()

    def checkpoint(self):
        if self.signum is not None and not self.delivered and not self.depth and not self.finalizing:
            self.delivered = True
            raise self.cancel_error

    @contextmanager
    def defer(self):
        self.depth += 1
        failed = False
        try:
            yield
        except BaseException as error:
            failed = True
            self.latch(error)
            raise
        finally:
            self.depth -= 1
            if not failed and sys.exc_info()[1] is None:
                self.checkpoint()

    @contextmanager
    def work(self):
        # Only work bodies may receive an asynchronous first exception. In
        # particular, the transition into a normal finally is latch-only.
        self.checkpoint()
        previous = self.interruptible
        self.interruptible = True
        try:
            yield
        except BaseException as error:
            self.latch(error)
            raise
        finally:
            self.interruptible = previous

    def install(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            signal.signal(signum, self.request)


def user():
    assert os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
    assert 'CAGENTS_AGENT_AUTHORITY' not in os.environ, 'Inherited authority is never removed'
    assert not os.environ.get('WAYLAND_DISPLAY') and not os.environ.get('WAYLAND_SOCKET')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def emit(event, **values):
    write_payload(out / 'events.jsonl',json.dumps(dict(event=event, arm=arm, unix_ns=time.time_ns(), **values)) + '\n',append=True)


def processes():
    values = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            values[int(path.parent.name)] = dict(state=fields[0], parent=int(fields[1]), group=int(fields[2]), stamp=fields[19])
        except (OSError, ValueError, IndexError):
            pass
    return values


def family(table, roots):
    result = set(roots)
    while True:
        expanded = result | {pid for pid, value in table.items() if value['parent'] in result}
        if expanded == result:
            return result
        result = expanded


def register(process, creation, options, *, resources=None, role=None, expected_mount=None):
    # Retain the actual Popen before any fallible identity lookup/publication.
    children.append(process)
    process.owned_identity = None
    process.ownership_error = 'Original child identity registration incomplete'
    process.publication_error = None
    process.owned_resources = resources
    process.owned_role = role
    process.owned_graceful = None
    process.expected_mount = expected_mount
    try:
        ledger.returned(process,creation,options)
        value = processes().get(process.pid)
        assert value is not None, 'Original child creation identity unavailable'
        assert value['group'] == process.pid, 'Owned Popen must have its own new session'
        process.owned_identity = dict(pid=process.pid, stamp=value['stamp'], group=value['group'])
        creation['identity'] = process.owned_identity
        process.ownership_error = None
    except BaseException as error:
        process.ownership_error = repr(error)
        creation['error'] = repr(error)
        cancellation.latch(error)
        raise
    finally:
        # Even a registration failure retains the role and original handle.
        # A pending signal cannot run cleanup until both publications finish.
        try:
            if resources is not None:
                resources[role] = process
                if expected_mount is not None:
                    resources['mount'] = expected_mount
            creation['published'] = True
        except BaseException as error:
            process.publication_error = repr(error)
            creation['error'] = creation['error'] or repr(error)
            cancellation.latch(error)
            raise
    return process


def start_owned(argv, *, resources=None, role=None, expected_mount=None, **options):
    assert resources is None or role is not None
    cancellation.checkpoint()
    with cancellation.defer():
        creation = ledger.attempt(argv,options.get('cwd'),role,expected_mount)
        try:
            for name in ('stdin','stdout','stderr'):
                stream = options.get(name)
                assert not hasattr(stream,'close') or id(stream) in ledger.streams, 'Unretained original parent descriptor'
            process = subprocess.Popen(argv, start_new_session=True, **options)
        except BaseException as error:
            creation['error'] = repr(error)
            ownership_errors.append('Child creation unresolved for ' + str(role or argv) + ': ' + repr(error))
            raise
        return register(process,creation,options,resources=resources,role=role,expected_mount=expected_mount)


def stop(process):
    identity = process.owned_identity
    key = (process.pid, identity['stamp'] if identity else None)
    if key in stopped:
        return
    if process.ownership_error is not None or identity is None:
        # The retained handle still supplies a genuine wait attempt, but no
        # missing/replaced generation authorizes a PID/group signal.
        original_wait(process,10,'identity-unresolved-wait')
        raise RuntimeError('Original child identity unresolved: ' + str(process.ownership_error))
    def terminal():
        original_wait(process,0)
        stopped.add(key)
    table = processes()
    current = table.get(process.pid)
    if identity is None:
        assert process.poll() is not None, 'No process-start receipt; refusing a borrowed PID'
        terminal()
        return
    # Never discover a new family through a reused/reaped root PID.
    original = current is not None and current['stamp'] == identity['stamp']
    if current is not None and not original:
        assert process.poll() is not None, 'Original process is unresolved after PID reuse'
        terminal()
        return
    owned = {pid: table[pid]['stamp'] for pid in family(table, {process.pid}) if pid in table} if original else {}
    def live_owned():
        current_table = processes()
        return {pid: stamp for pid, stamp in owned.items() if pid in current_table
                and current_table[pid]['stamp'] == stamp and current_table[pid]['state'] not in ('Z','X')}
    for signum in (signal.SIGTERM, signal.SIGKILL):
        if not live_owned():
            terminal()
            return
        current = processes().get(process.pid)
        if current and current['stamp'] == identity['stamp'] and current['group'] == identity['group']:
            try:
                row = ledger.handles[id(process)]
                row['signals'].append(dict(signal=signum,unix_ns=time.time_ns(),scope='original group'))
                if signum == signal.SIGKILL: row['forced'] = True
                os.killpg(identity['group'], signum)
            except ProcessLookupError:
                pass
        for pid, stamp in live_owned().items():
            value = processes().get(pid)
            if value and value['stamp'] == stamp and value['state'] not in ('Z', 'X'):
                try:
                    row = ledger.handles[id(process)]
                    row['signals'].append(dict(signal=signum,unix_ns=time.time_ns(),scope='original stamped descendant',pid=pid,stamp=stamp))
                    if signum == signal.SIGKILL: row['forced'] = True
                    os.kill(pid, signum)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and live_owned():
            process.poll()
            time.sleep(.05)
    if live_owned():
        raise RuntimeError('Owned descendants survived cleanup: ' + str(live_owned()))
    terminal()


def drain_adopted():
    assert ledger.fenced, 'Kernel drain requires a closed spawning phase'
    assert ledger.subreaper, 'Driver subreaper establishment UNKNOWN'
    assert all(row['returned'] and row['terminal'] is not None for row in ledger.creations), 'Original spawning roots remain unresolved'
    result = dict(scope='Original driver subreaper kernel children only',waits=[],echild=False,errors=[])
    deadline = time.monotonic()+10
    while time.monotonic() < deadline:
        try:
            pid,status = os.waitpid(-1,os.WNOHANG)
        except ChildProcessError:
            result['echild'] = True
            return result
        if pid:
            # The actual kernel parent relationship supplies this terminal
            # result, never signal authority from a PID/name/UID census.
            code = -(status & 0x7f) if status & 0x7f else (status >> 8) & 0xff
            result['waits'].append(dict(pid=pid,raw_status=status,exit_code=code,unix_ns=time.time_ns()))
            if any(row['pid']==pid for row in ledger.creations):
                result['errors'].append('Kernel PID overlaps an original terminal root; status not remapped')
            if code == -signal.SIGKILL: result['errors'].append('Adopted writer ended by SIGKILL')
        else:
            time.sleep(.05)  # 0 means a child still exists, never closure.
    result['errors'].append('Scoped kernel ECHILD not observed; adopted writer custody UNKNOWN')
    return result


def cleanup_arm(resources):
    errors = []
    for child in children:
        if child.owned_resources is resources:
            role = child.owned_role
            if resources.get(role) is not child:
                errors.append('Original child role publication unresolved: ' + str(role))
                resources.setdefault(role, child)
            if child.expected_mount is not None:
                if resources.get('mount', child.expected_mount) != child.expected_mount:
                    errors.append('Original Documents expected mount changed')
                resources.setdefault('mount', child.expected_mount)
            if child.ownership_error is not None:
                errors.append(str(role) + ' original identity: ' + child.ownership_error)
    for name in ('ui','clipboard','fixture'):
        if resources.get(name) is not None:
            try:
                stop(resources[name])
            except BaseException as error:
                cancellation.latch(error)
                errors.append(name + ': ' + repr(error))
    if resources.get('monitor') is not None:
        try:
            resources['monitor'].close()
        except BaseException as error:
            cancellation.latch(error)
            errors.append('monitor: ' + repr(error))
    for name in ('frontend','backend'):
        if resources.get(name) is not None:
            try:
                stop(resources[name])
            except BaseException as error:
                cancellation.latch(error)
                errors.append(name + ': ' + repr(error))
    documents = resources.get('documents')
    graceful = None
    if documents is not None:
        try:
            identity = documents.owned_identity
            actual = processes().get(documents.pid)
            assert identity and actual and actual['stamp'] == identity['stamp'], 'Documents owner disappeared before normal shutdown'
            ledger.handles[id(documents)]['signals'].append(dict(signal=signal.SIGTERM,unix_ns=time.time_ns(),scope='original Documents'))
            os.kill(documents.pid, signal.SIGTERM)
            code = original_wait(documents,15,'Documents-normal-TERM')
            assert code == 0, ('Documents did not complete normal signal/unmount cleanup', code)
            assert not resources['mount'].is_mount(), 'Documents mount survived graceful process exit'
            graceful = dict(exit_code=code, mount=str(resources['mount']), mounted_after=False, identity=identity)
            documents.owned_graceful = graceful
            stopped.add((documents.pid, identity['stamp']))
        except BaseException as error:
            cancellation.latch(error)
            errors.append('documents graceful-unmount: ' + repr(error))
            try:
                stop(documents)
            except BaseException as forced:
                cancellation.latch(forced)
                errors.append('documents forced cleanup: ' + repr(forced))
    if resources.get('permissions') is not None:
        try:
            stop(resources['permissions'])
        except BaseException as error:
            cancellation.latch(error)
            errors.append('permission-store: ' + repr(error))
    if resources.get('mount') is not None and resources['mount'].is_mount():
        errors.append('Documents FUSE mount remains active')
    names = {}
    for name in ('org.freedesktop.portal.Desktop','org.freedesktop.impl.portal.desktop.gtk',
                 'org.freedesktop.portal.Documents','org.freedesktop.impl.portal.PermissionStore'):
        try:
            names[name] = bus_owner(name)
            errors.append('Private portal owner survived cleanup: ' + name)
        except GLib.Error as error:
            if Gio.DBusError.get_remote_error(error) != 'org.freedesktop.DBus.Error.NameHasNoOwner':
                errors.append('Private owner cleanup query: ' + repr(error))
            names[name] = None
    table = processes()
    protected = set()
    for identity in resources.get('desktop_owners', []):
        value = table.get(identity['pid'])
        if value and value['stamp'] == identity['stamp']:
            protected |= family(table, {identity['pid']})
    remaining = sorted(pid for pid in family(table, {os.getpid()}) - {os.getpid()} - protected
                       if table[pid]['state'] not in ('Z','X'))
    if remaining:
        errors.append('Owned arm descendants remain before next arm: ' + repr(remaining))
    return dict(errors=errors, documents_graceful=graceful, private_bus_names=names, owned_descendants_remaining=remaining)



def spawn(argv, *, env=None, cwd=None, log=None, stdin=None, resources=None, role=None, expected_mount=None):
    user()
    with cancellation.defer():
        output = open_parent(log,'wb') if log is not None else None
        first = None
        try:
            process = start_owned(argv,env=env,cwd=cwd,stdin=stdin,stdout=output,stderr=subprocess.STDOUT,
                                  resources=resources,role=role,expected_mount=expected_mount)
            emit('process-started',argv=list(map(str,argv)),pid=process.pid,cwd=str(cwd or Path.cwd()))
        except BaseException as error:
            first = error; cancellation.latch(error)
        finally:
            try: close_parent(output)
            except BaseException as error:
                if first is None: first = error
        if first is not None: raise first
        return process


def command(argv, *, env=None, cwd=None, timeout=20, stdout=None, allowed_exit_codes=(0,)):
    user()
    started = time.time_ns()
    process = start_owned(argv, env=env, cwd=cwd, stdout=stdout or subprocess.PIPE,
                          stderr=subprocess.PIPE, role='command')
    closure_failure = None
    began = time.time_ns()
    try:
        with cancellation.work():
            output, errors = process.communicate(timeout=timeout)
            ledger.record_wait(process,'communicate',began,code=process.returncode)
    except BaseException as error:
        ledger.record_wait(process,'communicate',began,error=error)
        cancellation.latch(error)
        with cancellation.defer():
            try:
                stop(process)
            except BaseException as cleanup_error:
                ownership_errors.append('Command cleanup: ' + repr(cleanup_error))
        raise
    finally:
        for stream in (getattr(process,'stdin',None),getattr(process,'stdout',None),getattr(process,'stderr',None)):
            try: close_parent(stream)
            except BaseException as error:
                cancellation.latch(error)
                ownership_errors.append('Command parent pipe closure: '+repr(error))
                if closure_failure is None: closure_failure = error
    if closure_failure is not None: raise closure_failure
    emit('command', argv=list(map(str, argv)), cwd=str(cwd or Path.cwd()), exit_code=process.returncode,
         started_unix_ns=started, stderr=(errors or b'').decode(errors='replace'))
    if process.returncode not in allowed_exit_codes:
        raise subprocess.CalledProcessError(process.returncode, argv, output=output, stderr=errors)
    return output or b''


class IncompleteTree(RuntimeError):
    pass


def wait(query, timeout=30):
    cancellation.checkpoint()
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        cancellation.checkpoint()
        result = None
        try:
            result = query()
        except GLib.Error as error:
            if 'Unknown object' not in str(error) and not (error.domain == 'atspi_error' and error.code == 1
                    and re.fullmatch(r'/org/a11y/atspi/accessible/[0-9]+/[0-9]+', error.message)):
                raise
            last = error
        except IncompleteTree as error:
            last = error
        # Cancellation is distinct control, checked outside recoverable catches
        # and before even a positive observation can permit the next input.
        cancellation.checkpoint()
        if result:
            return result
        time.sleep(.15)
    cancellation.checkpoint()
    raise RuntimeError('Observation did not settle; no input was repeated') from last


def observation(query):
    return wait(lambda: (query(),))[0]  # False is an immediate caller assertion failure.


def tree(pid):
    os.kill(pid, 0)
    desktop = pyatspi.Registry.getDesktop(0)
    if desktop is None:
        raise IncompleteTree('No desktop')
    apps = [desktop.getChildAtIndex(index) for index in range(desktop.childCount)]
    if any(node is None for node in apps):
        raise IncompleteTree('Desktop changed')
    apps = [node for node in apps if node.get_process_id() == pid]
    assert len(apps) <= 1
    result, pending = [], apps
    while pending:
        node = pending.pop()
        if node is None:
            raise IncompleteTree('Child changed')
        result.append(node)
        assert len(result) < 10000 and node.childCount < 1000
        pending.extend(node.getChildAtIndex(index) for index in range(node.childCount))
    return result


def shown(node):
    return node.getState().contains(pyatspi.STATE_SHOWING)


def bounds(node):
    value = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    return [value.x, value.y, value.width, value.height]


def target(pid, name, role=pyatspi.ROLE_PUSH_BUTTON):
    values = [node for node in tree(pid) if node.name == name and node.getRole() == role and shown(node)]
    assert len(values) <= 1, ('Ambiguous actual native owner', name)
    return values[0] if values else None


def click(pid, name):
    # Retain only fresh scalar geometry across the native gesture.
    rectangle = wait(lambda: bounds(node) if (node := target(pid, name)) is not None
                     and node.getState().contains(pyatspi.STATE_ENABLED) else None)
    x, y, width, height = rectangle
    assert width > 0 and height > 0
    emit('native-click', target=name, pid=pid, bounds=rectangle)
    command(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])


def clipboard_image(root, resources):
    with cancellation.defer():
        stream = open_parent(root / 'offered.png','rb','clipboard-input')
        try:
            process = spawn(['xclip', '-selection', 'clipboard', '-target', 'image/png', '-in', '-quiet'], stdin=stream,
                            log=(out / (arm + '-clipboard.log')),resources=resources,role='clipboard')
        finally: close_parent(stream)
    emit('clipboard-offer', bytes=(root / 'offered.png').stat().st_size, sha256=sha(root / 'offered.png'), pid=process.pid)
    command(['xdotool', 'key', 'ctrl+v'])
    return process


def bus_owner(name):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    def call(method, value, signature):
        return bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', method,
            GLib.Variant('(s)', (value,)), GLib.VariantType.new(signature), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
    owner = call('GetNameOwner', name, '(s)')
    return dict(name=name, owner=owner, pid=call('GetConnectionUnixProcessID', owner, '(u)'))


def database(root):
    path = Path((root / 'database-path').read_text())
    assert path.resolve(strict=True) == path and path.is_relative_to(root / 'data')
    return path


def storage(root):
    path = database(root)
    workspace = (root / 'workspace-id').read_text()
    prefix = 'workspace:' + workspace + ':navigation:'
    try:
        with sqlite3.connect('file:' + str(path) + '?mode=ro', uri=True, timeout=.2) as db:
            db.execute('BEGIN')
            metadata = {key.removeprefix(prefix): json.loads(value) for key, value in db.execute(
                'SELECT key,value FROM metadata WHERE key LIKE ?', (prefix + '%',))}
            cards = {card: dict(source=json.loads(content), existence=json.loads(existence)) for card, content, existence in db.execute(
                'SELECT card_id,content_json,existence_json FROM cards ORDER BY ordinal')}
            return dict(cards=cards, metadata=metadata)
    except (sqlite3.Error, FileNotFoundError):
        return None


def source_witness(root):
    rows = [json.loads(line) for line in (root / 'fixture.jsonl').read_text().splitlines()]
    assert not any(row['event'].startswith('UNEXPECTED') for row in rows), rows[-5:]
    sources = [row for row in rows if row['event'] == 'source']
    assert len(sources) == 1
    return sources[0], rows


def check_storage(root, expected_recipe, expected_text=None, image=None):
    value = storage(root)
    if value is None:
        return None
    source, _ = source_witness(root)
    meta = value['metadata']
    if value['cards'].get(A, {}).get('source') != expected_recipe:
        return None
    assert value['cards'][B]['source'] == source['rejected']
    assert value['cards'][A]['existence'] == value['cards'][B]['existence'] == 'open'
    assert meta['quarantined_panes'] == {B: dict(kind='chat', **source['rejected'])}
    assert meta['chat_drafts'].get(B) == BAD_TEXT
    bad_images = meta['chat_draft_attachments'][B]
    assert len(bad_images) == 1 and bad_images[0]['path'] == str(root / 'rejected-saved.png')
    if expected_text is not None and meta['chat_drafts'].get(A) != expected_text:
        return None
    images = meta['chat_draft_attachments'].get(A, [])
    if image is not None and images != [image]:
        return None
    return value


class PortalObserver:
    """Read-only monitor on this private bus; never supplies a portal response."""
    def __init__(self, output):
        self.output = output
        self.rows = []
        self.lock = threading.RLock()
        self.condition = threading.Condition(self.lock)
        self.monitor = self.query = self.stream = None
        self.filter_id = None
        self.accepting = False
        self.inflight = 0
        self.record['initialized'] = False
        try:
            self.stream = open_parent(output,'a','callback-log')
            ledger.streams[id(self.stream)][1]['callback'] = self.record['id']
            flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
            address = os.environ['DBUS_SESSION_BUS_ADDRESS']
            self.record['monitor'] = 'acquisition-attempted'
            self.monitor = Gio.DBusConnection.new_for_address_sync(address,flags,None,None)
            self.record['monitor'] = 'acquired'
            self.record['query'] = 'acquisition-attempted'
            self.query = Gio.DBusConnection.new_for_address_sync(address,flags,None,None)
            self.record['query'] = 'acquired'
            self.frontend_owner = self.bus_call('GetNameOwner','org.freedesktop.portal.Desktop','(s)')
            with self.condition:
                self.accepting = self.record['accepting'] = True
            self.filter_id = self.monitor.add_filter(self.receive,None)
            self.record['filter_id'] = self.filter_id
            self.monitor.call_sync('org.freedesktop.DBus','/org/freedesktop/DBus',
                'org.freedesktop.DBus.Monitoring','BecomeMonitor',
                GLib.Variant('(asu)', (["type='method_call',interface='org.freedesktop.portal.FileChooser',member='OpenFile'",
                    "type='signal',interface='org.freedesktop.portal.Request',member='Response'",
                    "type='method_return',sender='" + self.frontend_owner + "'"],0)),
                None,Gio.DBusCallFlags.NONE,5000,None)
            self.record['initialized'] = True
        except BaseException as error:
            self.record['errors'].append('Observer initialization: '+repr(error))
            cancellation.latch(error)
            raise

    def bus_call(self, method, name, signature):
        return self.query.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', method,
            GLib.Variant('(s)', (name,)), GLib.VariantType.new(signature), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]

    def enter_callback(self):
        with self.condition:
            if not self.accepting: return False
            self.inflight += 1
            self.record['inflight'] = self.inflight
            return True

    def leave_callback(self):
        with self.condition:
            self.inflight -= 1
            self.record['inflight'] = self.inflight
            self.condition.notify_all()

    def receive(self, connection, message, incoming, unused):
        if not self.enter_callback(): return message
        try:
            if not incoming: return message
            body = message.get_body()
            value = dict(type=int(message.get_message_type()),sender=message.get_sender(),destination=message.get_destination(),
                         interface=message.get_interface(),member=message.get_member(),path=message.get_path(),
                         serial=message.get_serial(),reply_serial=message.get_reply_serial(),
                         body=body.unpack() if body else None,unix_ns=time.time_ns())
            with self.lock:
                self.rows.append(value)
                self.stream.write(json.dumps(value)+'\n')
            return message
        except BaseException as error:
            with self.condition:
                self.record['errors'].append('Observer callback: '+repr(error))
            cancellation.latch(error)
            return message  # Gio must not silently erase the recorded failure.
        finally:
            self.leave_callback()

    def opened(self, ui_pid, frontend_pid):
        with self.lock:
            assert not self.record['errors'], 'Portal callback failed: '+repr(self.record['errors'])
            rows = list(self.rows)
        calls = [row for row in rows if row['interface'] == 'org.freedesktop.portal.FileChooser' and row['member'] == 'OpenFile']
        assert len(calls) <= 1, 'No repeated or extra chooser input'
        if not calls:
            return None
        call = calls[0]
        assert self.bus_call('GetConnectionUnixProcessID', call['sender'], '(u)') == ui_pid
        owner = self.bus_call('GetNameOwner', 'org.freedesktop.portal.Desktop', '(s)')
        assert self.bus_call('GetConnectionUnixProcessID', owner, '(u)') == frontend_pid
        parent, title, options = call['body']
        assert parent == '' and title == 'Attach files to chat' and options['modal'] is True and options['multiple'] is True
        token = options['handle_token']
        assert re.fullmatch(r'[A-Za-z0-9_]+', token)
        returns = [row for row in rows if row['reply_serial'] == call['serial'] and row['destination'] == call['sender'] and row['sender'] == owner]
        assert len(returns) <= 1
        if not returns:
            return None
        path = returns[0]['body'][0]
        assert path == '/org/freedesktop/portal/desktop/request/' + call['sender'][1:].replace('.', '_') + '/' + token
        return dict(call=call, returned=returns[0], frontend_owner=owner, request_path=path, ui_pid=ui_pid, frontend_pid=frontend_pid)

    def cancelled(self, opened):
        with self.lock:
            assert not self.record['errors'], 'Portal callback failed: '+repr(self.record['errors'])
            values = [row for row in self.rows if row['interface'] == 'org.freedesktop.portal.Request'
                      and row['member'] == 'Response' and row['path'] == opened['request_path']]
        assert len(values) <= 1
        if not values:
            return None
        assert values[0]['sender'] == opened['frontend_owner'] and values[0]['body'][0] == 1
        return values[0]

    def close(self):
        if self.record['closed']: return
        errors = []
        condition = getattr(self,'condition',None)
        if condition is not None:
            with condition:
                self.accepting = self.record['accepting'] = False
        else:
            self.record['accepting'] = False
        # Never hold the callback lock across a synchronous connection close.
        monitor = getattr(self,'monitor',None)
        query = getattr(self,'query',None)
        filter_id = getattr(self,'filter_id',None)
        if monitor is not None and filter_id is not None:
            try:
                monitor.remove_filter(filter_id)
                self.record['filter_removed'] = True
            except BaseException as error: errors.append('Filter removal: '+repr(error))
        elif not self.record['initialized']:
            self.record['filter_removed'] = True
        for name,connection in [('monitor',monitor),('query',query)]:
            if connection is not None:
                try:
                    connection.close_sync(None)
                    self.record[name] = 'closed'
                except BaseException as error: errors.append(name+' close: '+repr(error))
        if condition is not None:
            deadline = time.monotonic()+5
            try:
                with condition:
                    while self.inflight and time.monotonic() < deadline:
                        self.condition.wait(max(0,deadline-time.monotonic()))
                    if self.inflight: errors.append('Observer callbacks remain in flight')
            except BaseException as error: errors.append('Observer callback drain: '+repr(error))
        if not self.record['inflight']:
            try: close_parent(getattr(self,'stream',None))
            except BaseException as error: errors.append('Observer output close: '+repr(error))
        self.record['errors'].extend(errors)
        self.record['closed'] = (not errors and not self.record['inflight']
            and self.record['filter_removed'] and self.record['monitor'] in ('closed','uncreated')
            and self.record['query'] in ('closed','uncreated'))
        if errors: raise RuntimeError('Observer writer settlement: '+repr(errors))


def open_monitor(resources, output):
    with cancellation.defer():
        record = ledger.callback(output)
        monitor = PortalObserver.__new__(PortalObserver)
        monitor.record = record
        ledger.monitors.append(monitor)  # Retain before resource publication or constructor.
        resources['monitor'] = monitor
        monitor.__init__(output)
        return monitor


def preparation_module(control):
    path = control / '.github/qualification/final-main-prepare.py'
    spec = importlib.util.spec_from_file_location('final_main_preparation',path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)  # Stdlib-only; no native or GI import/init.
    return module


def exercise(control, checkout, label, helper, base, target, plan, resources):
    global arm
    arm = label
    root = base / label
    assert root.resolve(strict=True) == root and stat.S_IMODE(root.stat().st_mode) == 0o700
    inventory = plan['_prepared']['arms'][label]['inventory']
    env = preparation.environment(dict(os.environ),str(base),label,inventory)
    preparation.verify_environment(plan['_prepared']['arms'][label],env)
    preparation.check_arm_files(plan['_prepared']['arms'][label])
    descriptors = root / 'portal-descriptors'
    descriptors.mkdir(mode=0o700)
    official_descriptor = Path('/usr/share/xdg-desktop-portal/portals/gtk.portal')
    descriptor = descriptors / 'gtk.portal'
    write_payload(descriptor,official_descriptor.read_bytes())
    assert sha(descriptor) == sha(official_descriptor)
    env['XDG_DESKTOP_PORTAL_DIR'] = str(descriptors)
    write_payload(out / (label + '-gtk.portal'),descriptor.read_bytes())
    icon = root / 'config/cosmic/com.system76.CosmicTk/v1/icon_theme'
    icon.parent.mkdir(parents=True)
    write_payload(icon,'"Adwaita"\n')
    permissions = spawn(['/usr/libexec/xdg-permission-store', '--verbose'], env=env,
                        log=(out / (label + '-permission-store.log')), resources=resources, role='permissions')
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.impl.portal.PermissionStore'], env=env, timeout=35)
    permission_owner = bus_owner('org.freedesktop.impl.portal.PermissionStore')
    assert permission_owner['pid'] == permissions.pid
    documents = spawn(['/usr/libexec/xdg-document-portal', '--verbose'], env=env,
                      log=(out / (label + '-documents.log')),
                      resources=resources, role='documents', expected_mount=root / 'runtime/doc')
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.portal.Documents'], env=env, timeout=35)
    document_owner = bus_owner('org.freedesktop.portal.Documents')
    assert document_owner['pid'] == documents.pid
    mount_reply = Gio.bus_get_sync(Gio.BusType.SESSION, None).call_sync('org.freedesktop.portal.Documents',
        '/org/freedesktop/portal/documents', 'org.freedesktop.portal.Documents', 'GetMountPoint', None,
        GLib.VariantType.new('(ay)'), Gio.DBusCallFlags.NONE, 30000, None).unpack()[0]
    mount = Path(bytes(mount_reply).rstrip(b'\0').decode())
    assert mount == root / 'runtime/doc' and mount.is_mount()
    backend = spawn(['/usr/libexec/xdg-desktop-portal-gtk', '--verbose'], env=env, log=(out / (label + '-portal-gtk.log')), resources=resources, role='backend')
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.impl.portal.desktop.gtk'], env=env, timeout=35)
    frontend = spawn(['/usr/libexec/xdg-desktop-portal', '--verbose'], env=env, log=(out / (label + '-portal.log')), resources=resources, role='frontend')
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.portal.Desktop'], env=env, timeout=35)
    monitor = open_monitor(resources, out / (label + '-portal.jsonl'))
    assert monitor.bus_call('GetConnectionUnixProcessID', monitor.bus_call('GetNameOwner', 'org.freedesktop.portal.Desktop', '(s)'), '(u)') == frontend.pid
    backend_owner = monitor.bus_call('GetNameOwner', 'org.freedesktop.impl.portal.desktop.gtk', '(s)')
    assert monitor.bus_call('GetConnectionUnixProcessID', backend_owner, '(u)') == backend.pid
    fixture = spawn([str(helper), str(root)], env=env, log=(out / (label + '-fixture.log')), resources=resources, role='fixture')
    wait(lambda: (root / 'fixture.jsonl').exists() and any(json.loads(line)['event'] == 'ready' for line in (root / 'fixture.jsonl').read_text().splitlines()))
    ui = spawn(['scripts/run-development', '--browserless', '--debug', '--log-file', str(out / (label + '-ui.log'))],
               env=env, cwd=checkout, log=(out / (label + '-runner.log')), resources=resources, role='ui')
    def owned_window():
        assert ui.poll() is None, 'Development runner exited'
        result = command(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], timeout=5, allowed_exit_codes=(0, 1))
        values = result.decode().split()
        assert len(values) <= 1
        return values[0] if values else None
    window = wait(owned_window, 1800)
    preparation.check_arm_files(plan['_prepared']['arms'][label])  # Refuse changed normal-runner output.
    pid = int(command(['xdotool', 'getwindowpid', window]).strip())
    ui_identity = processes()[pid]
    assert pid in family(processes(), {ui.pid}), 'Native window must belong to this actual development runner'
    exe = Path('/proc') / str(pid) / 'exe'
    assert exe.resolve() == (target / 'dev-runtime/cagents-ui').resolve(strict=True)
    ui_sha = sha(exe)
    triplet = {name: dict(path=str(target / 'dev-runtime' / name), sha256=sha(target / 'dev-runtime' / name))
               for name in ('cagents', 'cagentsd', 'cagents-ui')}
    command(['xdotool', 'windowactivate', '--sync', window])  # Initial setup only.
    command(['xdotool', 'windowsize', '--sync', window, '1280', '800'])
    command(['xdotool', 'windowmove', '--sync', window, '20', '40'])
    def composer():
        fields = [node for node in tree(pid) if node.name == 'Message' and node.getRole() == pyatspi.ROLE_ENTRY
                  and shown(node) and node.getState().contains(pyatspi.STATE_EDITABLE)]
        assert len(fields) <= 1
        return fields[0] if fields else None
    def panels(present):
        nodes = tree(pid)
        names = ['Files panel tab', 'Details panel tab', 'Resize context inspector']
        if not all(sum(shown(node) and node.name == name for node in nodes) == int(present) for name in names):
            return None
        toggle = target(pid, 'Toggle inspector — Ctrl+Shift+I')
        return dict(present=present, toggle=bounds(toggle)) if toggle else None
    before_panel = wait(lambda: panels(True))
    command(['xdotool', 'key', 'ctrl+shift+i'])
    after_panel = wait(lambda: panels(False))
    wait(composer)
    def key_target(expected):
        field = composer()
        if field is None or field.queryText().getText(0, -1) != expected or not field.getState().contains(pyatspi.STATE_FOCUSED):
            return None
        native_focus = int(command(['xdotool', 'getwindowfocus']).strip())
        if native_focus != int(window):
            return None  # GTK can unmap before normal WM focus restoration.
        native_pid = int(command(['xdotool', 'getwindowpid', str(native_focus)]).strip())
        assert native_pid == pid
        assert processes()[pid]['stamp'] == ui_identity['stamp'], 'Native UI process generation changed'
        text = field.queryText()
        assert text.caretOffset == len(expected) and text.getNSelections() == 0
        return dict(window=native_focus, pid=native_pid, start_stamp=ui_identity['stamp'],
                    editor_bounds=bounds(field), text=expected, caret=text.caretOffset, selections=text.getNSelections())
    def caption(account):
        nodes = tree(pid)
        names = [node.name for node in nodes if shown(node)]
        return account if names.count(account) == 1 else None
    def snapshot(name, size):
        command(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
        wait(lambda: target(pid, 'Attach files') is not None)
        data = observation(lambda: [dict(name=node.name, role=node.getRoleName(), bounds=bounds(node),
            focused=node.getState().contains(pyatspi.STATE_FOCUSED)) for node in tree(pid) if shown(node)])
        if name in ('draft-image-wide', 'postcancel-wide'):
            assert any(node['name'] == 'Saved pane unavailable' for node in data), 'Rejected sibling must remain visibly Failed in the wide comparison'
        png = out / (label + '-' + name + '.png')
        command(['import', '-window', window, str(png)])
        with Image.open(png) as image:
            assert image.size == tuple(size)
            save_image(image,out / (label + '-' + name + '.webp'))
        write_payload(out / (label + '-' + name + '-a11y.json'),json.dumps(data, indent=2) + '\n')
        emit('capture', name=name, viewport=size, webp=label + '-' + name + '.webp')
        emit('client-geometry', phase=name, xwininfo=command(['xwininfo','-id',window,'-stats','-tree']).decode())
    source, rows = source_witness(root)
    wait(lambda: any(row['event'] == 'seed-A-only' for row in source_witness(root)[1]))
    channel_scope = source['full_model']['views'][0]['scope']
    wait(lambda: any(row['event'] == 'seed-ack' and row['scope'] == channel_scope for row in source_witness(root)[1]))
    wait(lambda: target(pid, 'Saved pane unavailable', pyatspi.ROLE_HEADING) or any(node.name == 'Saved pane unavailable' and shown(node) for node in tree(pid)))
    assert observation(lambda: composer().queryText().getText(0, -1)) == ''
    wait(lambda: caption('Synthetic initial account'))
    click(pid, 'Choose a model')
    wait(lambda: target(pid, 'Fixture model A') is not None and target(pid, 'Fixture model B') is not None)
    catalog_observed = observation(lambda: [dict(name=name, bounds=bounds(target(pid, name)))
                                           for name in ('Fixture model A', 'Fixture model B')])
    command(['xdotool', 'key', 'Escape'])  # Close only; model and effort remain constant.
    wait(lambda: target(pid, 'Fixture model A') is None and target(pid, 'Fixture model B') is None)
    rectangle = observation(lambda: bounds(composer()))
    x, y, width, height = rectangle
    command(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])
    wait(lambda: key_target(''))
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', TEXT])
    wait(lambda: key_target(TEXT))
    clipboard = clipboard_image(root, resources)
    def pasted():
        value = storage(root)
        images = value and value['metadata'].get('chat_draft_attachments', {}).get(A, [])
        return (images[0] if len(images or []) == 1 and target(pid, 'Remove attachment') is not None
            and any(node.name == 'Image 1' and shown(node) for node in tree(pid))
            and not any(node.name == 'Pasting…' and shown(node) for node in tree(pid)) else None)
    image = wait(pasted)
    expected_text = observation(lambda: composer().queryText().getText(0, -1))
    assert TEXT in expected_text and 'image' in expected_text.lower()
    assert image['reference']['name'] == 'Pasted image' and image['reference']['number'] == 1
    asset = Path(image['path'])
    assert asset.resolve(strict=True) == asset and asset.is_relative_to(root / 'data') and sha(asset) == sha(root / 'offered.png')
    wait(lambda: check_storage(root, source['initial'], expected_text, image))
    write_payload(out / (label + '-payload-storage.json'),json.dumps(
        wait(lambda: check_storage(root, source['initial'], expected_text, image)), indent=2) + '\n')
    snapshot('draft-image-wide', (1280, 800))
    click(pid, 'Attach files')
    opened = wait(lambda: monitor.opened(pid, frontend.pid))
    def dialog():
        values = [node for node in tree(backend.pid) if node.getRole() in (pyatspi.ROLE_DIALOG, pyatspi.ROLE_FILE_CHOOSER)
                  and node.name == 'Attach files to chat' and shown(node)]
        assert len(values) <= 1
        return values[0] if values else None
    wait(dialog)
    def gtk_key_owner():
        focus = int(command(['xdotool', 'getwindowfocus']).strip())
        if focus in (0, 1):  # None/PointerRoot have no process owner.
            return None
        owner_property = command(['xprop', '-id', str(focus), '_NET_WM_PID']).decode().strip()
        owner_match = re.fullmatch(r'_NET_WM_PID\(CARDINAL\) = ([0-9]+)', owner_property)
        if owner_match is None or int(owner_match[1]) != backend.pid:
            return None  # Root/other-window focus can precede GTK activation.
        owner = int(command(['xdotool', 'getwindowpid', str(focus)]).strip())
        assert owner == backend.pid
        assert processes()[owner]['stamp'] == backend.owned_identity['stamp'], 'GTK process generation changed'
        return focus
    wait(gtk_key_owner)  # Observe normal WM activation before chooser-only keys.
    # libcosmic's portal path omits parent and ignores Dialog.directory. Use
    # the real GTK location entry to prevent captures of private Home listings.
    command(['xdotool', 'key', 'ctrl+l'])
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', str(root / 'chooser-data') + '/'])
    command(['xdotool', 'key', 'Return'])
    wait(lambda: any(node.name == 'synthetic-file.txt' and shown(node) for node in tree(backend.pid)))
    dialog_focus = wait(gtk_key_owner)
    emit('chooser-pending', portal=opened, gtk_pid=backend.pid, window=dialog_focus,
         chooser_directory=str(root / 'chooser-data'))
    (root / 'peer-update').touch(exist_ok=False)
    wait(lambda: any(row['event'] == 'peer-delivered' for row in source_witness(root)[1]))
    peer_storage = wait(lambda: check_storage(root, source['chosen'], expected_text, image))
    assert observation(lambda: caption('Synthetic initial account'))
    # Capture the actual owned dialog, after navigation and the peer update.
    command(['import', '-window', str(dialog_focus), str(out / (label + '-chooser-peer.png'))])
    with Image.open(out / (label + '-chooser-peer.png')) as image_file:
        save_image(image_file,out / (label + '-chooser-peer.webp'))
    emit('chooser-open-peer', portal=opened, gtk_pid=backend.pid, window=dialog_focus,
         chooser_directory=str(root / 'chooser-data'), canonical=peer_storage, image=image)
    click(backend.pid, 'Cancel')  # Genuine GTK Cancel once; no WM close or Request.Close.
    cancelled = wait(lambda: monitor.cancelled(opened))
    wait(lambda: not dialog())
    # No focus repair follows Cancel. A missing focus restoration is a real
    # observed failure of this private desktop, not a reason to click A again.
    focus_after = wait(lambda: key_target(expected_text))
    if label == 'after':
        wait(lambda: caption('Synthetic peer account'))
    # Before observes its actual result, without waiting for an expected bad
    # label or manufacturing a timeout. Two equal read snapshots settle it.
    def footer():
        return observation(lambda: [node.name for node in tree(pid) if shown(node)
            and (node.name in ('Synthetic initial account', 'Synthetic peer account', 'Claude · Default model', 'Ask before edits'))])
    actual_footer = footer()
    assert actual_footer and footer() == actual_footer
    if label == 'after':
        assert 'Synthetic peer account' in actual_footer and 'Synthetic initial account' not in actual_footer
    assert 'Claude · Default model' in actual_footer and 'Ask before edits' in actual_footer
    headers = observation(lambda: [node.name for node in tree(pid) if shown(node)
        and node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name.startswith('Chat pane ')
        and not node.name.startswith('Chat pane Unavailable saved sibling, ')])
    if label == 'after':
        assert any(', ' + str(root / 'project/peer-directory') + ', focused' in header for header in headers)
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', SUFFIX])
    final_text = expected_text + SUFFIX
    final_focus = wait(lambda: key_target(final_text))
    emit('postcancel-continuation', before=focus_after, after=final_focus, suffix=SUFFIX)
    wait(lambda: check_storage(root, source['chosen'], final_text, image))
    snapshot('postcancel-wide', (1280, 800))
    snapshot('postcancel-narrow', (900, 700))
    assert observation(lambda: key_target(final_text))
    stop(ui)
    stop(clipboard)
    stop(fixture)
    source, rows = source_witness(root)
    assert len([row for row in rows if row['event'] == 'seed-A-only']) == 1
    assert len([row for row in rows if row['event'] == 'peer-delivered']) == 1
    inspections = root / 'inspection'
    inspections.mkdir(mode=0o700)
    clone = inspections / 'final.sqlite3'
    backup_database(database(root),clone)
    os.chmod(clone, 0o600)
    typed = command([str(helper), str(root), '--inspect', str(clone)], env=env)
    write_payload(out / (label + '-typed-inspection.json'),typed)
    write_payload(out / (label + '-fixture.jsonl'),(root / 'fixture.jsonl').read_bytes())
    final = wait(lambda: check_storage(root, source['chosen'], final_text, image))
    assert asset.read_bytes() == (root / 'offered.png').read_bytes()
    write_payload(out / (label + '-managed-image.png'),asset.read_bytes())
    asset_info = asset.lstat()
    assert stat.S_ISREG(asset_info.st_mode) and not asset.is_symlink() and asset_info.st_nlink == 1 and asset_info.st_uid == os.geteuid()
    write_payload(out / (label + '-storage.json'),json.dumps(final, indent=2) + '\n')
    write_payload(out / (label + '-source.json'),json.dumps(dict(revision=plan[label if label == 'before' else 'product'],
        ui_pid=pid, ui_start_stamp=ui_identity['stamp'], ui_sha256=ui_sha, window=int(window), matched_triplet=triplet,
        helper_sha256=sha(helper), fixture_sha256=sha(control / '.github/qualification/final-main-fixture.rs'),
        prepared_environment_sha256=plan['_prepared']['arms'][label]['environment_sha256'],
        portal_backend=dict(pid=backend.pid, owner=backend_owner, executable='/usr/libexec/xdg-desktop-portal-gtk',
                            sha256=sha(Path('/usr/libexec/xdg-desktop-portal-gtk')), identity=backend.owned_identity),
        portal_frontend=dict(pid=frontend.pid, owner=monitor.frontend_owner, executable='/usr/libexec/xdg-desktop-portal',
                             sha256=sha(Path('/usr/libexec/xdg-desktop-portal')), identity=frontend.owned_identity),
        descriptor=dict(original=str(official_descriptor), private=str(descriptor), sha256=sha(descriptor),
                        original_sha256=sha(official_descriptor)), catalog_observed=catalog_observed,
        portal_auxiliaries=dict(permission_store={**permission_owner,'identity':permissions.owned_identity,
            'executable':'/usr/libexec/xdg-permission-store','sha256':sha(Path('/usr/libexec/xdg-permission-store'))},
            documents={**document_owner,'identity':documents.owned_identity,'executable':'/usr/libexec/xdg-document-portal',
                       'sha256':sha(Path('/usr/libexec/xdg-document-portal'))}, mount=str(mount)),
        inspector_before=before_panel, inspector_after=after_panel, portal_open=opened, portal_cancel=cancelled,
        postcancel_footer=actual_footer, postcancel_headers=headers, restored_focus=focus_after, continued_input=final_focus,
        actual_image=image, final_text=final_text,
        asset=dict(path=str(asset), sha256=sha(asset), bytes=asset_info.st_size, device=asset_info.st_dev,
                   inode=asset_info.st_ino, uid=asset_info.st_uid, links=asset_info.st_nlink),
        scope='Synthetic recipe and exact raw wrong-issuer sibling; actual UI, clipboard worker, portal and cancel. No provider/actor started.'), indent=2) + '\n')
    for name, value in triplet.items():
        assert sha(Path(value['path'])) == value['sha256'], 'Matched executable changed during its native arm'
    preparation.check_arm_files(plan['_prepared']['arms'][label])


def complete_arm_cleanup(resources, label, errors):
    active_error = sys.exc_info()[1]
    with cancellation.defer():
        try:
            result = cleanup_arm(resources)
        except BaseException as error:
            result = dict(errors=['Arm cleanup: ' + repr(error)], documents_graceful=None,
                          private_bus_names={}, owned_descendants_remaining=['UNKNOWN: arm cleanup incomplete'])
        try:
            write_payload(out / (label + '-cleanup.json'),json.dumps(result, indent=2) + '\n')
        except BaseException as error:
            result['errors'].append('Cleanup receipt: ' + repr(error))
        errors.extend(label + ': ' + error for error in result['errors'])
        if result['errors'] and active_error is None:
            error = RuntimeError('Arm cleanup failed before another arm: ' + repr(result))
            cancellation.latch(error)
            raise error
    return result


def publish_ledger(errors):
    value = ledger.snapshot(errors,None if cancellation.failure is None else repr(cancellation.failure),
                            cancellation.signum)
    temporary = out / '.driver-role-writer-ledger.tmp'
    destination = out / 'driver-role-writer-ledger.json'
    assert not destination.exists(), 'A prior ledger cannot be replaced or restamped'
    # Deliberate final self-publication: not a recursive writer/hash record.
    # No driver native-payload write follows a successful atomic rename.
    with cancellation.defer():
        stream = temporary.open('xb')
        first = None
        try:
            stream.write((json.dumps(value,indent=2)+'\n').encode())
            stream.flush(); os.fsync(stream.fileno())
        except BaseException as error: first = error
        finally:
            try: stream.close()
            except BaseException as error:
                if first is None: first = error
        if first is not None: raise first
        os.replace(temporary,destination)


def finish_cleanup(errors):
    """Settle only this driver's originals/parent writers/kernel children."""
    cancellation.finalizing = True
    ledger.fenced = True  # No later Popen, including transient commands.
    errors.extend(ownership_errors)
    for monitor in ledger.monitors:
        try: monitor.close()
        except BaseException as error:
            cancellation.latch(error); errors.append('Observer cleanup: '+repr(error))
    for child in children:
        if (getattr(child,'owned_role',None)=='documents' and getattr(child,'owned_graceful',None) is None
            and ledger.handles.get(id(child),{}).get('terminal') is None):
            try:
                result = cleanup_arm(child.owned_resources)
                errors.extend(result['errors'])
            except BaseException as error:
                cancellation.latch(error); errors.append('Original Documents role cleanup: '+repr(error))
    for child in reversed(children):
        try: stop(child)
        except BaseException as error:
            cancellation.latch(error)
            errors.append(str(getattr(child,'owned_role','unpublished-role'))+' cleanup: '+repr(error))
        for stream in (getattr(child,'stdin',None),getattr(child,'stdout',None),getattr(child,'stderr',None)):
            try: close_parent(stream)
            except BaseException as error:
                cancellation.latch(error); errors.append('Original child parent pipe: '+repr(error))
    for stream,row in ledger.original_streams:
        try: close_parent(stream)
        except BaseException as error:
            cancellation.latch(error); errors.append('Original parent stream closure: '+repr(error))
    try:
        ledger.kernel = drain_adopted()
        errors.extend(ledger.kernel['errors'])
        remaining = [] if ledger.kernel['echild'] else ['UNKNOWN: driver kernel children remain']
    except BaseException as error:
        cancellation.latch(error)
        errors.append('Owned descendant drain: '+repr(error))
        ledger.kernel = dict(echild=False,waits=[],errors=[repr(error)])
        remaining = ['UNKNOWN: descendant drain did not complete']
    for child in children:
        identity = getattr(child,'owned_identity',None)
        role = getattr(child,'owned_role',None)
        if getattr(child,'publication_error','unpublished') is not None:
            errors.append(str(role)+' original role/mount publication remains unresolved')
        if role=='documents':
            try:
                assert child.owned_graceful is not None, 'Documents normal wait/unmount was not verified'
                assert child.expected_mount is not None and not child.expected_mount.is_mount(), 'Documents mount unresolved'
            except BaseException as error:
                cancellation.latch(error); errors.append('Documents role settlement: '+repr(error))
    errors.extend(ledger.errors())
    cleanup = dict(errors=errors,owned_descendants_remaining=remaining)
    try: write_payload(out/'cleanup.json',json.dumps(cleanup,indent=2)+'\n')
    except BaseException as error:
        cancellation.latch(error); errors.append('Cleanup receipt publication: '+repr(error))
    if not errors and not remaining:
        try:
            write_payload(out/'cleanup-complete','Driver-owned role/writer streams and scoped kernel children settled. Outside Xvfb/private bus/entry/whole-job closure is NOT established.\n')
        except BaseException as error:
            cancellation.latch(error); errors.append('Cleanup marker publication: '+repr(error))
            try:
                (out/'cleanup-complete').unlink(missing_ok=True)
                write_payload(out/'cleanup.json',json.dumps(cleanup,indent=2)+'\n')
            except BaseException as secondary:
                cancellation.latch(secondary); errors.append('Failed marker removal/receipt: '+repr(secondary))
    # All receipt writers above have returned/closed. This is the last native
    # payload publication only when the ledger has no UNKNOWN/error outcome.
    try: publish_ledger(errors)
    except BaseException as error:
        cancellation.latch(error); errors.append('Final driver ledger publication: '+repr(error))
        try: (out/'cleanup-complete').unlink(missing_ok=True)
        except BaseException as secondary:
            cancellation.latch(secondary); errors.append('Failed stale marker removal: '+repr(secondary))
    return cleanup


def main():
    global out, cancellation, preparation, ledger
    user()
    assert len(sys.argv) == 4
    control, before, after = [Path(value).resolve(strict=True) for value in sys.argv[1:]]
    plan = json.loads((control / '.github/qualification/final-main-plan.json').read_text())
    out = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'native'
    out.mkdir(parents=True, exist_ok=True)
    cancellation = Cancellation()
    cancellation.install()  # Before metadata command() creates the first child.
    ledger = DriverLedger(time.time_ns,threading.RLock())
    errors = []
    arm_cleanups = {}
    try:
        # Adopt escaped descendants before any originally retained child exists.
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.prctl(36, 1, 0, 0, 0) != 0:
            raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER failed')
        flag = ctypes.c_int()
        assert libc.prctl(37,ctypes.byref(flag),0,0,0)==0 and flag.value==1, 'Driver subreaper establishment UNKNOWN'
        ledger.subreaper = True
        identity = dict(sha=command(['git','-C',str(control),'rev-parse','HEAD']).decode().strip(),
                        tree=command(['git','-C',str(control),'rev-parse','HEAD^{tree}']).decode().strip())
        preparation = preparation_module(control)
        prepared,prepared_sha = preparation.load_prepared(control,plan,{'before':before,'after':after},dict(os.environ),identity)
        plan['_prepared'] = prepared  # Runtime-only; source plan remains immutable.
        write_payload(out / 'source.json',json.dumps(dict(product=plan['product'],before=plan['before'],
            control_sha=identity['sha'],control_tree=identity['tree'],
            driver_sha256=sha(Path(__file__)),fixture_sha256=sha(control / '.github/qualification/final-main-fixture.rs'),
            preparation_sha256=prepared_sha,preparation_module_sha256=sha(control / '.github/qualification/final-main-prepare.py'),
            late_environment_sha256={key:preparation.digest(os.environ[key].encode()) for key in preparation.LATE if key in os.environ},
            workflow_sha256=sha(control / '.github/workflows/qualify-final-main-integration.yml'),
            run=os.environ['GITHUB_RUN_ID'],attempt=os.environ['GITHUB_RUN_ATTEMPT'],logical_job=os.environ['GITHUB_JOB'],
            numeric_job_id=None,numeric_job_note='No actual supported numeric job supplier is implemented.',
            plan_sha256=sha(control/'.github/qualification/final-main-plan.json'),
            driver_instance=dict(pid=os.getpid(),observation=processes().get(os.getpid()))),indent=2) + '\n')
        ledger.binding = json.loads((out/'source.json').read_text())
        base = Path(prepared['base'])
        launcher = next(path for path in [Path('/usr/libexec/at-spi-bus-launcher'), Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')] if path.exists())
        spawn([str(launcher), '--launch-immediately'], log=(out / 'atspi.log'), role='accessibility-launcher')
        for key in ('IsEnabled', 'ScreenReaderEnabled'):
            command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
                     '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', key, '<true>'])
        bus = command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus', '--method', 'org.a11y.Bus.GetAddress'])
        os.environ['AT_SPI_BUS_ADDRESS'] = ast.literal_eval(bus.decode())[0]
        registry = next(path for path in [Path('/usr/libexec/at-spi2-registryd'), Path('/usr/lib/at-spi2-core/at-spi2-registryd')] if path.exists())
        spawn([str(registry)], env=dict(os.environ, DBUS_SESSION_BUS_ADDRESS=os.environ['AT_SPI_BUS_ADDRESS']), log=(out / 'atspi-registry.log'), role='accessibility-registry')
        rc = base / 'rc.xml'
        write_payload(rc,'<openbox_config xmlns="http://openbox.org/3.4/rc"><focus><focusNew>yes</focusNew><followMouse>no</followMouse></focus></openbox_config>')
        write_payload(out / 'openbox-rc.xml',rc.read_bytes())
        wm = spawn(['/usr/bin/openbox', '--config-file', str(rc)], log=(out / 'openbox.log'), role='window-manager')
        wait(lambda: b'_NET_SUPPORTING_WM_CHECK(WINDOW)' in command(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK']))
        write_payload(out / 'desktop.json',json.dumps(dict(openbox_pid=wm.pid, config_sha256=sha(rc),
            supporting_wm=command(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK']).decode(),
            versions=command(['dpkg-query', '-W', '-f=${Package} ${Version}\n', 'openbox', 'xdg-desktop-portal', 'xdg-desktop-portal-gtk']).decode(),
            renderer=RENDERER, virtual_screen=[1600,1000], dpi=96,
            system_bus_limit='Normal frontend startup may query or activate RealtimeKit; failure is nonfatal. No system service, policy or system bus is modified or owned by this fixture.',
            parent_identifier='omitted by the actual libcosmic portal implementation',
            directory='not supplied by libcosmic portal; normal GTK location entry is used'), indent=2) + '\n')
        for label, checkout in [('before', before), ('after', after)]:
            ledger.enter(label)  # Explicit scope includes this arm's source checks/transient commands.
            pin = plan['before' if label == 'before' else 'product']
            assert command(['git', '-C', str(checkout), 'rev-parse', 'HEAD']).decode().strip() == pin['sha']
            assert command(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}']).decode().strip() == pin['tree']
            assert not command(['git', '-C', str(checkout), 'status', '--porcelain=v1']).strip()
            prepared_arm = prepared['arms'][label]
            target_dir = Path(prepared_arm['inventory']['target'])
            helper = Path(prepared_arm['helper'])
            preparation.check_arm_files(prepared_arm)
            resources = {'desktop_owners':[child.owned_identity for child in children
                         if child.owned_identity is not None and child.poll() is None]}
            try:
                with cancellation.work():
                    exercise(control, checkout, label, helper, base, target_dir, plan, resources)
            except BaseException as error:
                cancellation.latch(error)
                raise
            finally:
                arm_cleanups[label] = complete_arm_cleanup(resources, label, errors)
            ledger.scope = 'global-driver'
    except BaseException as error:
        cancellation.finalizing = True
        cancellation.latch(error)
        try:
            write_payload(out / 'failure-traceback.txt',traceback.format_exc())
        except BaseException as publication_error:
            errors.append('Original failure publication: ' + repr(publication_error))
    finally:
        cleanup = finish_cleanup(errors)
    if cancellation.failure is not None:
        raise cancellation.failure
    if cancellation.cancel_error is not None:
        raise cancellation.cancel_error
    if cleanup['errors'] or cleanup['owned_descendants_remaining']:
        raise RuntimeError('Native cleanup incomplete: ' + repr(cleanup))



if __name__ == '__main__':
    main()
