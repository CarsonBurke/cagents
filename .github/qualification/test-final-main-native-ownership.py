"""Algorithm-only cancellation tests; no process, signal, bus or native actions.

Only whitelisted definitions are AST-extracted from the actual driver. Imports,
module initialization, main(), GI, /proc reads and native work are never executed.
Fake handles and output sinks below are not a native/closure evidence packet.
Run only after independent source review and the lead's pure-data authorization.
"""
import ast
import copy
from contextlib import contextmanager
import json
from pathlib import Path
import re
import subprocess
import sys
from types import SimpleNamespace
import unittest


SOURCE = Path(__file__).with_name('final-main-native.py').read_text()
NAMES = {'NativeCancelled','Cancellation','family','register','start_owned','stop','cleanup_arm',
         'spawn','command','open_monitor','complete_arm_cleanup','finish_cleanup','IncompleteTree','wait',
         'DriverLedger','open_parent','close_parent','write_payload','original_wait','drain_adopted',
         'publish_ledger','PortalObserver','save_image','backup_database','wait_record','admit_work','send_owned_signal'}


class InertPath:
    def __new__(cls, *args, **kwargs):
        raise AssertionError('Extracted logic attempted ambient path access')

    @staticmethod
    def cwd():
        return '/algorithm-only-cwd'


class Output:
    def __init__(self):
        self.files = {}
        self.on_write = lambda name: None
        self.on_open = lambda name: None
        self.on_close = lambda name: None
        self.events = []

    def __truediv__(self, name):
        outer = self
        class File:
            def __init__(self, mode):
                self.closed = False
                self.mode = mode
                if 'x' in mode and name in outer.files:raise FileExistsError(name)
                if 'a' not in mode:outer.files[name] = ''
                outer.on_open(name)
            def write(self, data):
                assert not self.closed
                outer.on_write(name)
                outer.files[name] = outer.files.get(name,'')+(data.decode() if isinstance(data,bytes) else data)
            def flush(self):pass
            def fileno(self):return 17
            def close(self):
                outer.on_close(name);self.closed = True;outer.events.append(('stream-close',name))
        class Named:
            def __str__(self):return '/algorithm-only/'+name
            def open(self, mode):return File(mode)
            def exists(self):return name in outer.files
            def unlink(self, missing_ok=False):outer.files.pop(name,None)
            def __init__(self):self.name = name
        return Named()


class Lock:
    def __init__(self):self.depth=0
    def __enter__(self):self.depth+=1;return self
    def __exit__(self, *args):self.depth-=1


class Pipe:
    def __init__(self, events, name):self.closed=False;self.events=events;self.name=name
    def close(self):self.closed=True;self.events.append(('pipe-close',self.name))


class Mount:
    def __init__(self):
        self.mounted = True

    def is_mount(self):
        return self.mounted

    def __str__(self):
        return '/algorithm-only/runtime/doc'


class Handle:
    def __init__(self, harness, pid, options):
        self.harness, self.pid = harness, pid
        self.returncode = None
        self.state = 'S'
        self.communicate_code = 0
        self.wait_error = None
        self.wait_code = None
        self.on_wait = lambda: None
        self.on_communicate = lambda: None
        self.auto_unmount = True
        self.stdin = None
        self.stdout = Pipe(harness.events,str(pid)+'.stdout') if options.get('stdout') == -1 else None
        self.stderr = Pipe(harness.events,str(pid)+'.stderr') if options.get('stderr') == -1 else None

    def poll(self):
        return self.returncode

    def wait(self, timeout):
        self.harness.events.append(('wait',self.pid,timeout))
        if getattr(self,'owned_role',None) == 'documents' and timeout == 15:
            assert ('signal',self.pid,15) in self.harness.events, 'Documents must receive original TERM before wait'
        self.on_wait()
        if self.wait_error is not None:
            raise self.wait_error
        self.returncode = self.wait_code if self.wait_code is not None else (0 if self.returncode is None else self.returncode)
        self.state = 'Z'
        if getattr(self,'expected_mount',None) is not None and self.auto_unmount:
            self.expected_mount.mounted = False
        return self.returncode

    def communicate(self, timeout):
        self.harness.events.append(('communicate',self.pid))
        self.on_communicate()
        self.returncode, self.state = self.communicate_code, 'Z'
        return b'observed bytes', b''


class Harness:
    def __init__(self):
        self.events, self.handles, self.handlers = [], [], {}
        self.creation = lambda handle: None
        self.lookup = lambda: None
        self.on_signal = lambda: None
        self.output = Output()
        self.output.events = self.events
        self.kernel_answers = []
        def create(argv, **options):
            assert options['start_new_session'] is True
            assert 'preexec_fn' not in options  # No inherited signal mask trick.
            assert self.ns['cancellation'].depth > 0
            handle = Handle(self,10 + len(self.handles),options)
            self.handles.append(handle)
            self.events.append(('created',handle.pid,list(argv)))
            self.creation(handle)
            return handle
        def table():
            self.lookup()
            return {handle.pid:dict(state=handle.state,parent=1000,group=handle.pid,stamp=str(handle.pid) + '-original')
                    for handle in self.handles}
        def kill(pid, signum):
            self.events.append(('signal',pid,signum))
            self.on_signal()
            handle = next(handle for handle in self.handles if handle.pid == pid)
            handle.returncode, handle.state = 0, 'Z'
        tick = iter(range(10000))
        class NoOwner(Exception):
            pass
        def waitpid(pid, flags):
            assert (pid,flags)==(-1,1)
            self.events.append(('kernel-wait',pid,flags))
            if not self.kernel_answers:raise ChildProcessError()
            answer=self.kernel_answers.pop(0)
            if isinstance(answer,BaseException):raise answer
            return answer
        def replace(source,destination):
            self.output.files[destination.name]=self.output.files.pop(source.name)
            self.events.append(('ledger-rename',destination.name))
        namespace = dict(__builtins__=__builtins__,contextmanager=contextmanager,sys=sys,Path=InertPath,json=json,re=re,
            children=[],stopped=set(),ownership_errors=[],out=self.output,
            user=lambda:None,processes=table,bus_owner=lambda name:(_ for _ in ()).throw(NoOwner()),
            GLib=SimpleNamespace(Error=NoOwner),
            Gio=SimpleNamespace(DBusError=SimpleNamespace(get_remote_error=lambda error:'org.freedesktop.DBus.Error.NameHasNoOwner')),
            os=SimpleNamespace(getpid=lambda:1000,kill=kill,killpg=kill,WNOHANG=1,waitpid=waitpid,
                               fsync=lambda fd:self.events.append(('fsync',fd)),replace=replace),
            time=SimpleNamespace(time_ns=lambda:next(tick),monotonic=lambda:next(tick),sleep=lambda delay:None),
            signal=SimpleNamespace(SIGINT=2,SIGTERM=15,SIGKILL=9,signal=lambda signum,handler:self.handlers.__setitem__(signum,handler)),
            subprocess=SimpleNamespace(Popen=create,PIPE=-1,STDOUT=-2,CalledProcessError=subprocess.CalledProcessError),
            emit=lambda event,**values:self.events.append(('emit',event,values)))
        parsed = ast.parse(SOURCE)
        selected = [copy.deepcopy(node) for node in parsed.body
                    if isinstance(node,(ast.FunctionDef,ast.ClassDef)) and node.name in NAMES]
        assert {node.name for node in selected} == NAMES
        exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-driver-definitions>','exec'),namespace)
        namespace['cancellation'] = namespace['Cancellation']()
        namespace['ledger'] = namespace['DriverLedger'](namespace['time'].time_ns,Lock(),1000)
        namespace['ledger'].binding = dict(algorithm_only='not a native/source/closure evidence packet',
            logical_job='algorithm-only-logical-job',numeric_job_id=None,
            driver_instance=dict(pid=1000,observation=dict(state='R',parent=900,group=900,stamp='12345')))
        namespace['ledger'].subreaper = True
        self.ns = namespace

    def spawn(self, resources=None, role='ui', mount=None):
        return self.ns['spawn'](['algorithm-only-never-execute'],resources=resources,role=role,expected_mount=mount)

    def repeat(self):
        self.ns['cancellation'].request(2,None)
        self.ns['cancellation'].request(15,None)


class PortalHarness:
    """Actual observer methods with inert connections, locks and output files."""
    def __init__(self, harness, failure=None):
        self.h=harness;self.failure=failure;self.created=[];self.locks=[];self.resources={}
        self.on_wait=lambda:None
        outer=self
        class Condition:
            def __init__(self, lock):self.lock=lock
            def __enter__(self):return self.lock.__enter__()
            def __exit__(self, *args):return self.lock.__exit__(*args)
            def notify_all(self):pass
            def wait(self, timeout):
                assert self.lock.depth==1
                self.lock.depth=0
                try:outer.on_wait()
                finally:self.lock.depth=1
        def new_lock():
            lock=Lock();outer.locks.append(lock);return lock
        class Connection:
            def __init__(self, name):self.name=name
            def call_sync(self, *args):
                if args[3]=='BecomeMonitor' and failure=='become':raise RuntimeError('become failure')
                return SimpleNamespace(unpack=lambda:(':1.algorithm-frontend',))
            def add_filter(self, callback, unused):
                if failure=='filter':raise RuntimeError('filter failure')
                return 77
            def remove_filter(self, identifier):
                assert identifier==77 and not outer.resources['monitor'].record['accepting']
                assert all(lock.depth==0 for lock in outer.locks)
                harness.events.append(('remove-filter',))
            def close_sync(self, unused):
                assert all(lock.depth==0 for lock in outer.locks), 'No callback lock over sync close'
                harness.events.append(('connection-close',self.name))
                if failure=='close-monitor' and self.name=='monitor':raise RuntimeError('monitor close failure')
        def connect(*args):
            assert self.resources.get('monitor') in harness.ns['ledger'].monitors
            name='monitor' if not self.created else 'query'
            if failure==name:raise RuntimeError(name+' acquisition failure')
            value=Connection(name);self.created.append(value)
            if name=='query':assert self.resources['monitor'].monitor is self.created[0]
            return value
        old_gio=harness.ns['Gio'];old_glib=harness.ns['GLib']
        harness.ns['Gio']=SimpleNamespace(DBusConnectionFlags=SimpleNamespace(AUTHENTICATION_CLIENT=1,MESSAGE_BUS_CONNECTION=2),
            DBusConnection=SimpleNamespace(new_for_address_sync=connect),DBusCallFlags=SimpleNamespace(NONE=0),DBusError=old_gio.DBusError)
        harness.ns['GLib']=SimpleNamespace(Variant=lambda signature,value:value,
            VariantType=SimpleNamespace(new=lambda value:value),Error=old_glib.Error)
        harness.ns['threading']=SimpleNamespace(RLock=new_lock,Condition=Condition)
        harness.ns['os'].environ={'DBUS_SESSION_BUS_ADDRESS':'algorithm-only-private-bus'}
    def open(self):
        return self.h.ns['open_monitor'](self.resources,self.h.output/'portal.jsonl')


class Message:
    def get_body(self):return SimpleNamespace(unpack=lambda:[])
    def get_message_type(self):return 4
    def get_sender(self):return ':1.algorithm-only'
    def get_destination(self):return None
    def get_interface(self):return 'algorithm-only'
    def get_member(self):return 'algorithm-only'
    def get_path(self):return '/algorithm-only'
    def get_serial(self):return 1
    def get_reply_serial(self):return 0


class AtomicOwnershipTests(unittest.TestCase):
    def test_creation_identity_and_role_are_one_deferred_publication(self):
        for point in ('creation','identity','role'):
            with self.subTest(point=point):
                h = Harness(); mount = Mount()
                class Roles(dict):
                    def __setitem__(self, key, value):
                        if point == 'role' and key == 'documents':
                            self_test.assertIsNotNone(value.owned_identity)
                            h.repeat()
                        super().__setitem__(key,value)
                self_test = self
                roles = Roles()
                permissions = h.spawn(roles,'permissions')
                if point == 'creation':
                    h.creation = lambda handle:h.repeat()
                if point == 'identity':
                    def lookup():
                        self.assertIs(h.ns['children'][-1],h.handles[-1])
                        h.repeat()
                    h.lookup = lookup
                h.ns['cancellation'].install()
                self.assertEqual(set(h.handlers),{2,15})
                with self.assertRaises(h.ns['NativeCancelled']): h.spawn(roles,'documents',mount)
                original = h.handles[-1]
                self.assertIs(h.ns['children'][-1],original)
                self.assertIs(roles['documents'],original)
                self.assertIs(roles['mount'],mount)
                self.assertIs(original.expected_mount,mount)
                self.assertIsNone(original.ownership_error)
                self.assertEqual(h.ns['cancellation'].signum,2)
                result = h.ns['complete_arm_cleanup'](roles,'before',[])
                self.assertEqual(result['errors'],[])
                doc_term = ('signal',original.pid,15)
                self.assertEqual(h.events.count(doc_term),1)
                doc_wait = h.events.index(('wait',original.pid,15))
                self.assertLess(h.events.index(doc_term),doc_wait)
                permission_signal = next(i for i,event in enumerate(h.events) if event[:2] == ('signal',permissions.pid))
                self.assertLess(doc_wait,permission_signal)

    def test_creation_failure_never_becomes_positive_absence(self):
        h = Harness(); original = RuntimeError('Popen did not return an original handle')
        def fail(handle): raise original
        h.creation = fail
        with self.assertRaisesRegex(RuntimeError,'Popen did not return'): h.spawn()
        self.assertEqual(h.ns['children'],[])
        self.assertIs(h.ns['cancellation'].failure,original)
        result = h.ns['finish_cleanup']([])
        self.assertTrue(any('creation unresolved' in error for error in result['errors']))
        self.assertNotIn('cleanup-complete',h.output.files)

    def test_registration_failure_keeps_original_handle_role_and_failure(self):
        h = Harness(); roles = {}; mount = Mount(); original = RuntimeError('identity lookup failed')
        def fail(): raise original
        h.lookup = fail
        with self.assertRaisesRegex(RuntimeError,'identity lookup failed'): h.spawn(roles,'documents',mount)
        self.assertIs(h.ns['children'][0],h.handles[0])
        self.assertIs(roles['documents'],h.handles[0]); self.assertIs(roles['mount'],mount)
        self.assertIs(h.ns['cancellation'].failure,original)
        h.lookup = lambda:None
        result = h.ns['finish_cleanup']([])
        self.assertTrue(result['errors']); self.assertNotIn('cleanup-complete',h.output.files)
        self.assertIs(h.ns['cancellation'].failure,original)
        self.assertIn(('wait',h.handles[0].pid,10),h.events)  # Retained original still waited despite unknown identity.
        self.assertFalse(any(event[0]=='signal' for event in h.events))

    def test_registration_error_precedes_a_secondary_publication_error(self):
        h = Harness(); original = RuntimeError('original identity lookup failure')
        def fail(): raise original
        h.lookup = fail
        class Roles(dict):
            def __setitem__(self,key,value): raise RuntimeError('secondary role publication failure')
        with self.assertRaisesRegex(RuntimeError,'secondary role publication failure'): h.spawn(Roles(),'ui')
        self.assertIs(h.ns['cancellation'].failure,original)
        self.assertIs(h.ns['children'][0],h.handles[0])
        self.assertIn('original identity',h.handles[0].ownership_error)
        self.assertIn('secondary role',h.handles[0].publication_error)

    def test_publication_failure_retains_original_role_and_mount_metadata(self):
        for role in ('ui','documents'):
            with self.subTest(role=role):
                h = Harness(); mount = Mount()
                class Roles(dict):
                    def __setitem__(self,key,value):
                        if key == ('mount' if role == 'documents' else 'ui'):
                            raise RuntimeError('role publication failed')
                        super().__setitem__(key,value)
                roles = Roles()
                with self.assertRaisesRegex(RuntimeError,'role publication failed'): h.spawn(roles,role,mount if role == 'documents' else None)
                child = h.ns['children'][0]
                self.assertEqual(child.owned_role,role)
                self.assertIn('role publication failed',child.publication_error)
                if role == 'documents':
                    self.assertIs(roles['documents'],child); self.assertIs(child.expected_mount,mount)
                result = h.ns['finish_cleanup']([])
                self.assertTrue(any('role/mount publication' in error for error in result['errors']))
                self.assertNotIn('cleanup-complete',h.output.files)

    def test_repeated_cleanup_cancel_preserves_documents_wait_and_order(self):
        h = Harness(); roles = {}; mount = Mount()
        permissions = h.spawn(roles,'permissions')
        documents = h.spawn(roles,'documents',mount)
        documents.on_wait = h.repeat
        h.on_signal = h.repeat
        h.output.on_write = lambda name:h.repeat()
        h.repeat()  # First signal at normal cleanup entry is latch-only.
        with self.assertRaises(h.ns['NativeCancelled']): h.ns['complete_arm_cleanup'](roles,'before',[])
        result = json.loads(h.output.files['before-cleanup.json'])
        self.assertEqual(result['errors'],[])
        self.assertEqual(result['documents_graceful']['exit_code'],0)
        self.assertFalse(mount.mounted)
        doc_term = ('signal',documents.pid,15)
        self.assertEqual(h.events.count(doc_term),1)
        doc_wait = h.events.index(('wait',documents.pid,15))
        self.assertLess(h.events.index(doc_term),doc_wait)
        permission_signal = next(i for i,event in enumerate(h.events) if event[:2] == ('signal',permissions.pid))
        self.assertLess(doc_wait,permission_signal)
        first = h.ns['cancellation'].cancel_error
        self.assertIsInstance(first,h.ns['NativeCancelled'])
        result = h.ns['finish_cleanup']([])
        self.assertEqual(result,dict(errors=[],owned_descendants_remaining=[]))
        self.assertIs(h.ns['cancellation'].cancel_error,first)
        self.assertIsNone(h.ns['cancellation'].failure)
        self.assertIn('cleanup-complete',h.output.files)  # Local settlement, never a native work success.

    def test_original_work_and_drain_failure_survive_repeated_cancel(self):
        h = Harness(); original = RuntimeError('original native oracle failure')
        h.ns['cancellation'].latch(original)
        def drain():
            h.repeat()
            raise RuntimeError('drain inspection failed')
        h.ns['drain_adopted'] = drain
        h.output.on_write = lambda name:h.repeat()
        result = h.ns['finish_cleanup']([])
        self.assertIs(h.ns['cancellation'].failure,original)
        self.assertTrue(any('drain inspection failed' in error for error in result['errors']))
        self.assertEqual(result['owned_descendants_remaining'],['UNKNOWN: descendant drain did not complete'])
        self.assertIn('cleanup.json',h.output.files); self.assertNotIn('cleanup-complete',h.output.files)
        # Fresh non-finalizing state isolates an original work exception from
        # both a later cancellation and a secondary arm-cleanup failure.
        h = Harness(); arm_errors = []
        def fail_arm(resources):
            h.repeat()
            raise RuntimeError('secondary arm cleanup failure')
        h.ns['cleanup_arm'] = fail_arm
        try:
            with h.ns['cancellation'].work():
                raise original
        except RuntimeError:
            result = h.ns['complete_arm_cleanup']({},'before',arm_errors)
            self.assertIs(h.ns['cancellation'].failure,original)
            self.assertTrue(result['errors']); self.assertTrue(arm_errors)
            self.assertFalse(h.ns['cancellation'].finalizing)

    def test_nonzero_documents_wait_cannot_become_graceful(self):
        h = Harness(); roles = {}; mount = Mount()
        h.spawn(roles,'permissions')
        documents = h.spawn(roles,'documents',mount)
        documents.wait_code = 7
        result = h.ns['cleanup_arm'](roles)
        self.assertIsNone(result['documents_graceful'])
        self.assertTrue(any('did not complete normal signal/unmount cleanup' in error for error in result['errors']))
        self.assertEqual(h.events.count(('signal',documents.pid,15)),1)
        self.assertIn(('wait',documents.pid,15),h.events)
        final = h.ns['finish_cleanup'](result['errors'])
        self.assertTrue(final['errors']); self.assertNotIn('cleanup-complete',h.output.files)

    def test_unresolved_wait_or_mount_never_publishes_complete(self):
        for point in ('wait','mount'):
            with self.subTest(point=point):
                h = Harness(); roles = {}; mount = Mount()
                child = h.spawn(roles,'documents' if point == 'mount' else 'ui',mount if point == 'mount' else None)
                if point == 'wait': child.wait_error = RuntimeError('original wait unresolved')
                else: child.auto_unmount = False
                errors = []
                if point == 'mount':
                    result = h.ns['complete_arm_cleanup']
                    with self.assertRaisesRegex(AssertionError,'^Documents mount survived graceful process exit$') as caught:
                        result(roles,'before',errors)
                    self.assertIs(type(caught.exception),AssertionError)
                    self.assertIs(caught.exception,h.ns['cancellation'].failure)
                    arm_result=json.loads(h.output.files['before-cleanup.json'])
                    self.assertIsNone(arm_result['documents_graceful'])
                    self.assertIn('documents graceful-unmount: '+repr(caught.exception),arm_result['errors'])
                    self.assertIn('before: documents graceful-unmount: '+repr(caught.exception),errors)
                    self.assertIn('Documents FUSE mount remains active',arm_result['errors'])
                    self.assertTrue(mount.mounted)
                result = h.ns['finish_cleanup'](errors)
                self.assertTrue(result['errors']); self.assertNotIn('cleanup-complete',h.output.files)
                if point=='mount':self.assertIs(h.ns['cancellation'].failure,caught.exception)

    def test_command_creation_and_missing_window_are_retained(self):
        h = Harness(); h.creation = lambda handle:h.repeat()
        with self.assertRaises(h.ns['NativeCancelled']): h.ns['command'](['algorithm-only-command'])
        self.assertIs(h.ns['children'][0],h.handles[0])
        self.assertEqual(h.handles[0].owned_role,'command')
        h = Harness()
        h.creation = lambda handle:setattr(handle,'on_communicate',h.repeat)
        with self.assertRaises(h.ns['NativeCancelled']): h.ns['command'](['interruptible-algorithm-only-command'])
        self.assertIs(h.ns['children'][0],h.handles[0])
        self.assertEqual(h.handles[0].returncode,0)  # Original retained handle settled after cancellation.
        search = ['xdotool','search','--onlyvisible','--class','cagents']
        for code, allowed in [(0,True),(1,True),(2,False)]:
            h = Harness(); h.creation = lambda handle,c=code:setattr(handle,'communicate_code',c)
            if allowed: self.assertEqual(h.ns['command'](search,allowed_exit_codes=(0,1)),b'observed bytes')
            else:
                with self.assertRaises(subprocess.CalledProcessError): h.ns['command'](search,allowed_exit_codes=(0,1))
            self.assertEqual(len(h.ns['children']),1)
        h = Harness(); h.creation = lambda handle:setattr(handle,'communicate_code',1)
        with self.assertRaises(subprocess.CalledProcessError): h.ns['command'](['another-command'])
        h = Harness()
        self.assertEqual(h.ns['command'](['ordinary-algorithm-command']),b'observed bytes')

    def test_observer_is_retained_before_constructor_cancellation_or_failure(self):
        for fail in (False,True):
            with self.subTest(fail=fail):
                h = Harness(); roles = {}
                class Observer:
                    def __init__(self, output):
                        self_test.assertIs(roles['monitor'],self)
                        self_test.assertGreater(h.ns['cancellation'].depth,0)
                        if fail: raise RuntimeError('partial monitor startup')
                        h.repeat()
                        self.initialized = True
                self_test = self
                h.ns['PortalObserver'] = Observer
                expected = RuntimeError if fail else h.ns['NativeCancelled']
                with self.assertRaises(expected): h.ns['open_monitor'](roles,'algorithm-only-output')
                self.assertIsInstance(roles['monitor'],Observer)
                if not fail: self.assertTrue(roles['monitor'].initialized)

    def test_recoverable_observation_cannot_swallow_cancellation(self):
        for variant in ('incomplete','stale','positive-after-handled-stale'):
            with self.subTest(variant=variant):
                # Positive control: the same transient exception remains retryable
                # without cancellation. No real GLib object or AX tree is used.
                h = Harness(); calls = []
                def transient():
                    calls.append('query')
                    if len(calls) == 1:
                        raise h.ns['IncompleteTree']('changed')
                    return True
                self.assertTrue(h.ns['wait'](transient))
                self.assertEqual(calls,['query','query'])
                self.assertIsNone(h.ns['cancellation'].failure)
                h = Harness(); calls = []; inputs = []
                error_type = h.ns['IncompleteTree'] if variant == 'incomplete' else h.ns['GLib'].Error
                def query():
                    calls.append('query')
                    try:
                        raise error_type('Unknown object algorithm-only stale tree')
                    except error_type:
                        h.repeat()  # Actual request() observes an active recoverable exception.
                        if variant == 'positive-after-handled-stale':
                            return True
                        raise
                with self.assertRaises(h.ns['NativeCancelled']) as caught:
                    with h.ns['cancellation'].work():
                        h.ns['wait'](query)
                        inputs.append('would issue the next native input')
                self.assertIs(caught.exception,h.ns['cancellation'].cancel_error)
                self.assertIsNone(h.ns['cancellation'].failure)
                self.assertEqual(calls,['query']); self.assertEqual(inputs,[])

    def test_terminal_unwind_keeps_explicit_diagnostic_separate(self):
        h = Harness(); original = RuntimeError('genuine terminal work error')
        with self.assertRaisesRegex(RuntimeError,'genuine terminal work error') as caught:
            with h.ns['cancellation'].work():
                try:
                    raise original
                except RuntimeError:
                    h.repeat()  # Defer control; work() explicitly latches the terminal unwind.
                    raise
        self.assertIs(caught.exception,original)
        self.assertIs(h.ns['cancellation'].failure,original)
        self.assertIsInstance(h.ns['cancellation'].cancel_error,h.ns['NativeCancelled'])
        self.assertIsNot(original,h.ns['cancellation'].cancel_error)
        result = h.ns['finish_cleanup']([])
        self.assertEqual(result['errors'],[])
        self.assertIs(h.ns['cancellation'].failure,original)

    def test_unique_creation_ids_scopes_attempts_and_spawn_fence_are_not_role_names(self):
        h=Harness();owner=h.ns['ledger']
        h.ns['command'](['same-algorithm-command'])
        owner.enter('before')
        h.ns['command'](['same-algorithm-command']);h.ns['command'](['same-algorithm-command'])
        self.assertEqual([row['scope'] for row in owner.creations],['global-driver','before','before'])
        self.assertEqual(len({row['id'] for row in owner.creations}),3)
        self.assertEqual(owner.entered,['before'])  # Unentered After is not inferred.
        result=h.ns['finish_cleanup']([])
        self.assertEqual(result['errors'],[])
        count=len(h.handles)
        with self.assertRaisesRegex(AssertionError,'spawning phase is closed'):h.ns['command'](['must-not-create'])
        self.assertEqual(len(h.handles),count)
        report=json.loads(h.output.files['driver-role-writer-ledger.json'])
        self.assertEqual(report['entered_arms'],['before']);self.assertTrue(report['no_future_spawns'])
        self.assertEqual(len(report['creations']),3)
        self.assertTrue(all(row['terminal']['method']=='wait' or row['terminal']['method']=='communicate' for row in report['creations']))

    def test_atomic_parent_log_and_child_pipes_close_on_cancel_or_creation_failure(self):
        for stage in ('normal','cancel-open','popen-failure','close-failure'):
            with self.subTest(stage=stage):
                h=Harness();original=RuntimeError(stage)
                def opened(name):
                    self.assertGreater(h.ns['cancellation'].depth,0)
                    if stage=='cancel-open':h.repeat()
                h.output.on_open=opened
                if stage=='popen-failure':
                    def fail(handle):raise original
                    h.creation=fail
                if stage=='close-failure':
                    def fail(name):
                        if name=='owned.log':raise original
                    h.output.on_close=fail
                if stage=='normal':h.ns['spawn'](['algorithm-only'],log=h.output/'owned.log',role='ui')
                else:
                    expected=h.ns['NativeCancelled'] if stage=='cancel-open' else RuntimeError
                    with self.assertRaises(expected):h.ns['spawn'](['algorithm-only'],log=h.output/'owned.log',role='ui')
                row=h.ns['ledger'].writers[0]
                self.assertTrue(row['acquired'])
                if stage!='close-failure':self.assertTrue(row['closed'])
                else:self.assertIsNotNone(row['error'])
                result=h.ns['finish_cleanup']([])
                if stage in ('popen-failure','close-failure'):self.assertTrue(result['errors'])
                else:
                    self.assertEqual(result['errors'],[])
                    self.assertEqual(row['inherited_by'],[h.ns['ledger'].creations[0]['id']])
                    self.assertIsNotNone(h.ns['ledger'].creations[0]['terminal'])
        h=Harness();h.ns['command'](['ordinary-algorithm-command'])
        pipes=[stream for stream,row in h.ns['ledger'].original_streams]
        self.assertEqual(len(pipes),2);self.assertTrue(all(stream.closed for stream in pipes))

    def test_kernel_wait_requires_original_roots_and_retains_status_zero_and_ECHILD(self):
        h=Harness();h.spawn();h.kernel_answers=[(77,0),(78,15),(0,0),ChildProcessError()]
        result=h.ns['finish_cleanup']([])
        self.assertEqual(result['errors'],[])
        kernel=h.ns['ledger'].kernel
        self.assertTrue(kernel['echild'])
        self.assertEqual([(row['pid'],row['raw_status'],row['exit_code']) for row in kernel['waits']],[(77,0,0),(78,15,-15)])
        first_kernel=next(i for i,event in enumerate(h.events) if event[0]=='kernel-wait')
        self.assertTrue(any(event[0]=='wait' for event in h.events[:first_kernel]))
        for signum in (15,9):
            h=Harness();child=h.spawn()
            def controlled(pid, signal):
                h.events.append(('signal',pid,signal))
                if signal==signum:child.returncode,child.state=-signal,'Z'
            h.ns['os'].kill=h.ns['os'].killpg=controlled
            result=h.ns['finish_cleanup']([])
            self.assertEqual(h.ns['ledger'].creations[0]['terminal']['exit_code'],-signum)
            self.assertEqual(bool(result['errors']),signum==9)  # Settled TERM is distinct from forced KILL.
            self.assertEqual('cleanup-complete' in h.output.files,signum==15)
        for stage in ('zero-only','root-unwaited','root-pid-overlap','subreaper-unknown'):
            with self.subTest(stage=stage):
                h=Harness();child=h.spawn()
                if stage=='zero-only':h.ns['os'].waitpid=lambda pid,flags:(0,0)
                if stage=='root-unwaited':child.wait_error=RuntimeError('original wait unresolved')
                if stage=='root-pid-overlap':h.kernel_answers=[(child.pid,0),ChildProcessError()]
                if stage=='subreaper-unknown':h.ns['ledger'].subreaper=False
                result=h.ns['finish_cleanup']([])
                self.assertTrue(result['errors']);self.assertNotIn('cleanup-complete',h.output.files)
                if stage=='root-unwaited':self.assertFalse(any(event[0]=='kernel-wait' for event in h.events))

    def test_actual_observer_partial_acquisitions_and_close_errors_keep_originals(self):
        for stage in ('monitor','query','filter','become','close-monitor'):
            with self.subTest(stage=stage):
                h=Harness();portal=PortalHarness(h,stage)
                if stage=='close-monitor':observer=portal.open()
                else:
                    with self.assertRaises(RuntimeError):portal.open()
                    observer=portal.resources['monitor']
                self.assertIn(observer,h.ns['ledger'].monitors)
                if stage=='close-monitor':
                    with self.assertRaisesRegex(RuntimeError,'Observer writer settlement'):observer.close()
                else:observer.close()
                for connection in portal.created:
                    self.assertIn(('connection-close',connection.name),h.events)
                self.assertTrue(h.ns['ledger'].errors())
                self.assertFalse(observer.record['accepting']);self.assertEqual(observer.record['inflight'],0)

    def test_actual_original_terminal_policy_distinguishes_unissued_kill_TERM_and_command_exits(self):
        h=Harness();child=h.spawn();child.returncode,child.state=0,'Z'
        self.assertEqual(h.ns['finish_cleanup']([])['errors'],[])
        for code in (-9,-15,7,-2):
            with self.subTest(code=code):
                h=Harness();child=h.spawn();child.returncode,child.state=code,'Z'
                result=h.ns['finish_cleanup']([])
                row=h.ns['ledger'].creations[0]
                self.assertFalse(row['forced']);self.assertEqual(row['signals'],[])
                self.assertEqual(row['terminal']['exit_code'],code)
                self.assertTrue(any('unexpected original terminal outcome' in error for error in result['errors']))
                self.assertNotIn('cleanup-complete',h.output.files)
        h=Harness();child=h.spawn()
        def controlled(pid, signum):
            h.events.append(('signal',pid,signum));child.returncode,child.state=-signum,'Z'
        h.ns['os'].kill=h.ns['os'].killpg=controlled
        self.assertEqual(h.ns['finish_cleanup']([])['errors'],[])
        self.assertEqual(h.ns['ledger'].creations[0]['terminal']['exit_code'],-15)
        for allowed in ((0,1),(0,)):
            h=Harness();h.creation=lambda handle:setattr(handle,'communicate_code',1)
            if 1 in allowed:self.assertEqual(h.ns['command'](['algorithm-search'],allowed_exit_codes=allowed),b'observed bytes')
            else:
                with self.assertRaises(subprocess.CalledProcessError):h.ns['command'](['algorithm-search'],allowed_exit_codes=allowed)
            self.assertEqual(h.ns['ledger'].creations[0]['exit_policy'],dict(kind='declared-command-exits',allowed_exit_codes=list(allowed)))
            self.assertEqual(bool(h.ns['finish_cleanup']([])['errors']),1 not in allowed)

    def test_wait_recording_failure_preserves_exact_first_error_and_original_cleanup(self):
        h=Harness();child=h.spawn()
        self.assertEqual(h.ns['original_wait'](child,1),0)
        self.assertEqual(h.ns['ledger'].creations[0]['terminal']['exit_code'],0)
        for stage in ('wait','communicate','successful-communicate'):
            with self.subTest(stage=stage):
                h=Harness();original=RuntimeError('actual original '+stage);secondary=RuntimeError('diagnostic metadata failed')
                def broken_clock():raise secondary
                def broken_record(*args, **kwargs):raise secondary
                if stage=='wait':
                    child=h.spawn();child.wait_error=original
                    h.ns['ledger'].clock=broken_clock  # Actual record_wait clock fails after the actual wait exception.
                    with self.assertRaises(RuntimeError) as caught:h.ns['original_wait'](child,1)
                    self.assertIs(caught.exception,original)
                else:
                    def setup(handle):
                        if stage=='communicate':
                            def fail():raise original
                            handle.on_communicate=fail
                    h.creation=setup;h.ns['ledger'].record_wait=broken_record
                    with self.assertRaises(RuntimeError) as caught:h.ns['command'](['algorithm-only-command'])
                    self.assertIs(caught.exception,original if stage=='communicate' else secondary)
                    child=h.handles[0]
                    self.assertTrue(any(event[0]=='wait' and event[1]==child.pid for event in h.events))
                    self.assertTrue(child.stdout.closed and child.stderr.closed)
                expected=secondary if stage=='successful-communicate' else original
                self.assertIs(h.ns['cancellation'].failure,expected)
                self.assertTrue(any('wait metadata UNKNOWN' in error for error in h.ns['ownership_errors']))
                result=h.ns['finish_cleanup']([])
                self.assertTrue(result['errors']);self.assertNotIn('cleanup-complete',h.output.files)
                self.assertIs(h.ns['cancellation'].failure,expected)

    def test_callback_failure_after_final_query_refuses_arm_and_new_work_but_settles_owners(self):
        h=Harness();portal=PortalHarness(h);observer=portal.open()
        observer.close();self.assertEqual(h.ns['finish_cleanup']([])['errors'],[])
        for stage in ('parse','write'):
            with self.subTest(stage=stage):
                h=Harness();portal=PortalHarness(h);child=h.spawn(portal.resources,'ui');observer=portal.open()
                class Response(Message):
                    def get_interface(self):return 'org.freedesktop.portal.Request'
                    def get_member(self):return 'Response'
                    def get_sender(self):return ':1.algorithm-frontend'
                    def get_body(self):return SimpleNamespace(unpack=lambda:[1,{}])
                opened=dict(request_path='/algorithm-only',frontend_owner=':1.algorithm-frontend')
                observer.receive(None,Response(),True,None)
                self.assertEqual(observer.cancelled(opened)['body'],[1,{}])  # Successful final query precedes failure.
                original=h.ns['IncompleteTree']('asynchronous terminal callback '+stage)
                if stage=='parse':
                    class Broken(Message):
                        def get_body(self):raise original
                    observer.receive(None,Broken(),True,None)
                else:
                    def fail(name):
                        if name=='portal.jsonl':raise original
                    h.output.on_write=fail;observer.receive(None,Message(),True,None)
                count=len(h.handles);attempts=len(h.ns['ledger'].creations)
                with self.assertRaises(h.ns['IncompleteTree']) as caught:h.ns['complete_arm_cleanup'](portal.resources,'before',[])
                self.assertIs(caught.exception,original)
                self.assertTrue(observer.stream.closed)
                self.assertTrue(all(('connection-close',connection.name) in h.events for connection in portal.created))
                with self.assertRaises(h.ns['IncompleteTree']) as caught:h.ns['command'](['forbidden-next-arm-input'])
                self.assertIs(caught.exception,original)
                queries=[]
                with self.assertRaises(h.ns['IncompleteTree']) as caught:h.ns['wait'](lambda:queries.append('forbidden') or True)
                self.assertIs(caught.exception,original);self.assertEqual(queries,[])
                self.assertEqual((len(h.handles),len(h.ns['ledger'].creations)),(count,attempts))
                self.assertTrue(h.ns['finish_cleanup']([])['errors'])
                self.assertTrue(any(event[0]=='wait' and event[1]==child.pid for event in h.events))
                self.assertIs(h.ns['cancellation'].failure,original)
        for kind in ('incomplete','stale'):
            h=Harness();self.assertTrue(h.ns['wait'](lambda:True))
            original=(h.ns['IncompleteTree'] if kind=='incomplete' else h.ns['GLib'].Error)('Unknown object terminal callback')
            queries=[];inputs=[]
            def query():
                queries.append('query');h.ns['cancellation'].latch(original);return True
            with self.assertRaises(type(original)) as caught:
                h.ns['wait'](query);inputs.append('forbidden-after-positive-query')
            self.assertIs(caught.exception,original);self.assertEqual(queries,['query']);self.assertEqual(inputs,[])

    def test_driver_binding_requires_actual_shaped_original_self_observation(self):
        h=Harness()
        self.assertEqual(h.ns['ledger'].errors(),[])
        self.assertEqual(h.ns['ledger'].binding['driver_instance']['pid'],1000)
        self.assertIsNone(h.ns['ledger'].binding['numeric_job_id'])
        self.assertEqual(h.ns['finish_cleanup']([])['errors'],[])
        for case in ('missing-instance','missing-observation','null-observation','missing-stamp','bad-stamp','zero-group','wrong-pid','numeric-job'):
            with self.subTest(case=case):
                h=Harness();binding=h.ns['ledger'].binding
                if case=='missing-instance':del binding['driver_instance']
                elif case=='missing-observation':del binding['driver_instance']['observation']
                elif case=='null-observation':binding['driver_instance']['observation']=None
                elif case=='missing-stamp':del binding['driver_instance']['observation']['stamp']
                elif case=='bad-stamp':binding['driver_instance']['observation']['stamp']=''
                elif case=='zero-group':binding['driver_instance']['observation']['group']=0
                elif case=='wrong-pid':binding['driver_instance']['pid']=1001
                else:binding['numeric_job_id']=123
                self.assertTrue(any('instance binding UNKNOWN' in error for error in h.ns['ledger'].errors()))
                self.assertTrue(h.ns['finish_cleanup']([])['errors']);self.assertNotIn('cleanup-complete',h.output.files)

    def test_controlled_TERM_requires_returned_group_send_not_ESRCH_attempt(self):
        for outcome in ('returned-TERM','ESRCH-TERM','ESRCH-KILL','returned-KILL'):
            with self.subTest(outcome=outcome):
                h=Harness();child=h.spawn();row=h.ns['ledger'].creations[0]
                self.assertEqual(child.state,'S');self.assertEqual(row['identity']['group'],child.pid)
                def send(target, signum):
                    self.assertEqual(target,child.pid)
                    attempt=row['signals'][-1]
                    self.assertEqual((attempt['returned'],attempt['outcome']),(False,'attempted'))
                    h.events.append(('signal',target,signum))
                    if outcome.endswith('KILL') and signum==15:return  # Retained live original until the KILL observation.
                    child.returncode,child.state=(-15 if outcome.endswith('TERM') else (0 if outcome=='ESRCH-KILL' else -9)),'Z'
                    if outcome.startswith('ESRCH'):raise ProcessLookupError(3,'algorithm-only ESRCH')
                h.ns['os'].kill=h.ns['os'].killpg=send
                result=h.ns['finish_cleanup']([])
                selected=next(item for item in row['signals'] if item['signal']==(15 if outcome.endswith('TERM') else 9))
                self.assertEqual(selected['scope'],'original group')
                self.assertEqual(selected['returned'],outcome.startswith('returned'))
                self.assertEqual(selected['outcome'],'returned' if outcome.startswith('returned') else 'ESRCH')
                self.assertEqual(selected['error'] is None,outcome.startswith('returned'))
                self.assertEqual(row['forced'],outcome=='returned-KILL')
                self.assertEqual(row['terminal']['exit_code'],-15 if outcome.endswith('TERM') else (0 if outcome=='ESRCH-KILL' else -9))
                negative=outcome in ('ESRCH-TERM','returned-KILL')
                self.assertEqual(bool(result['errors']),negative)
                self.assertEqual('cleanup-complete' in h.output.files,not negative)
                report=json.loads(h.output.files['driver-role-writer-ledger.json'])
                self.assertEqual(bool(report['errors']),negative)
        h=Harness();roles={};mount=Mount()
        h.spawn(roles,'permissions');documents=h.spawn(roles,'documents',mount)
        self.assertEqual(h.ns['cleanup_arm'](roles)['errors'],[])
        receipt=h.ns['ledger'].handles[id(documents)]['signals'][0]
        self.assertEqual((receipt['scope'],receipt['signal'],receipt['returned'],receipt['outcome']),('original Documents',15,True,'returned'))

    def test_callback_gate_drains_actual_admission_before_stream_close_and_rejects_late_work(self):
        h=Harness();portal=PortalHarness(h);observer=portal.open()
        self.assertTrue(observer.enter_callback())  # Actual admission of a hypothetical already-running callback.
        events=[]
        def finish_admitted():
            self.assertFalse(observer.record['accepting'])
            self.assertFalse(observer.stream.closed)
            observer.leave_callback()  # Actual finally operation; models the other callback thread returning.
            events.append('admitted-finished')
        portal.on_wait=finish_admitted
        observer.close()
        self.assertEqual(events,['admitted-finished']);self.assertTrue(observer.stream.closed)
        self.assertEqual(observer.record['inflight'],0);self.assertTrue(observer.record['closed'])
        before=dict(h.output.files)
        class Late(Message):
            def get_body(self):raise AssertionError('Late callback passed the closed admission gate')
        late=Late();self.assertIs(observer.receive(None,late,True,None),late)
        self.assertEqual(h.output.files,before)
        self.assertIsNone(h.ns['cancellation'].failure)

    def test_callback_parse_write_close_and_unfinished_work_refuse_positive_marker(self):
        for stage in ('parse','write','close','inflight'):
            with self.subTest(stage=stage):
                h=Harness();portal=PortalHarness(h);observer=portal.open();original=RuntimeError('callback '+stage)
                if stage=='parse':
                    class Broken(Message):
                        def get_body(self):raise original
                    observer.receive(None,Broken(),True,None)
                if stage=='write':
                    def fail(name):
                        if name=='portal.jsonl':raise original
                    h.output.on_write=fail
                    observer.receive(None,Message(),True,None)
                if stage=='close':
                    observer.receive(None,Message(),True,None)
                    def fail(name):
                        if name=='portal.jsonl':raise original
                    h.output.on_close=fail
                if stage=='inflight':self.assertTrue(observer.enter_callback())
                if stage in ('parse','write'):
                    self.assertEqual(observer.record['inflight'],0)
                    self.assertIs(h.ns['cancellation'].failure,original)
                result=h.ns['finish_cleanup']([])
                self.assertTrue(result['errors']);self.assertNotIn('cleanup-complete',h.output.files)
                if stage=='inflight':
                    self.assertFalse(observer.stream.closed)  # Never close under an admitted writer.
                    self.assertEqual(observer.record['inflight'],1)

    def test_final_sidecar_is_last_payload_write_and_failed_publication_removes_marker(self):
        h=Harness();h.spawn();result=h.ns['finish_cleanup']([])
        self.assertEqual(result['errors'],[])
        self.assertEqual(h.events[-1],('ledger-rename','driver-role-writer-ledger.json'))
        report=json.loads(h.output.files['driver-role-writer-ledger.json'])
        self.assertEqual(report['kind'],'driver-role-writer-ledger-v1')
        self.assertTrue(report['kernel']['echild']);self.assertEqual(report['errors'],[])
        self.assertTrue(any('Xvfb/private session-bus' in item for item in report['outside_scope']))
        with self.assertRaisesRegex(AssertionError,'Writer phase/count'):h.ns['write_payload'](h.output/'late.txt','no late writer')
        self.assertNotIn('late.txt',h.output.files)
        for stage in ('write','rename'):
            h=Harness();h.spawn();original=RuntimeError('original work failure');secondary=RuntimeError('ledger '+stage)
            h.ns['cancellation'].latch(original)
            if stage=='write':
                def fail(name):
                    if name=='.driver-role-writer-ledger.tmp':raise secondary
                h.output.on_write=fail
            else:
                def fail(source,destination):raise secondary
                h.ns['os'].replace=fail
            result=h.ns['finish_cleanup']([])
            self.assertTrue(result['errors']);self.assertIs(h.ns['cancellation'].failure,original)
            self.assertNotIn('cleanup-complete',h.output.files)
            self.assertNotIn('driver-role-writer-ledger.json',h.output.files)

    def test_image_and_SQLite_original_writers_close_on_partial_failure(self):
        h=Harness();seen=[]
        class Image:
            def save(self, stream, **options):
                self_test.assertEqual(options,dict(format='WEBP',lossless=True))
                seen.append(stream);stream.write(b'algorithm-only-image')
        self_test=self
        h.ns['save_image'](Image(),h.output/'image.webp')
        self.assertTrue(seen[0].closed)
        original=RuntimeError('second SQLite connection failed');connections=[]
        class Connection:
            closed=False
            def close(self):self.closed=True
            def backup(self, other):raise AssertionError('No backup after failed second acquisition')
        def connect(path, **options):
            if connections:raise original
            value=Connection();connections.append(value);return value
        h.ns['sqlite3']=SimpleNamespace(connect=connect)
        with self.assertRaises(RuntimeError) as caught:h.ns['backup_database']('source','clone')
        self.assertIs(caught.exception,original);self.assertTrue(connections[0].closed)
        self.assertIs(h.ns['cancellation'].failure,original)
        self.assertTrue(h.ns['ledger'].errors())

    def test_metadata_children_have_install_subreaper_and_outer_cleanup(self):
        main = next(node for node in ast.parse(SOURCE).body if isinstance(node,ast.FunctionDef) and node.name == 'main')
        install = next(node for node in ast.walk(main) if isinstance(node,ast.Call) and isinstance(node.func,ast.Attribute) and node.func.attr == 'install')
        guarded = next(node for node in main.body if isinstance(node,ast.Try))
        commands = [node for node in ast.walk(main) if isinstance(node,ast.Call) and isinstance(node.func,ast.Name) and node.func.id == 'command']
        guarded_calls = {node.lineno for node in ast.walk(guarded) if isinstance(node,ast.Call)}
        self.assertTrue(commands); self.assertTrue(all(node.lineno in guarded_calls for node in commands))
        subreaper = next(node for node in ast.walk(guarded) if isinstance(node,ast.Call) and isinstance(node.func,ast.Attribute) and node.func.attr == 'prctl')
        self.assertLess(install.lineno,subreaper.lineno)
        self.assertLess(subreaper.lineno,min(node.lineno for node in commands))
        self.assertTrue(any(isinstance(node,ast.Call) and isinstance(node.func,ast.Name) and node.func.id == 'finish_cleanup'
                            for statement in guarded.finalbody for node in ast.walk(statement)))
        # No raw subprocess.run hides a child outside the retained Popen path.
        self.assertFalse(any(isinstance(node,ast.Call) and isinstance(node.func,ast.Attribute) and isinstance(node.func.value,ast.Name)
                             and node.func.value.id == 'subprocess' and node.func.attr == 'run' for node in ast.walk(ast.parse(SOURCE))))


if __name__ == '__main__':
    unittest.main()
