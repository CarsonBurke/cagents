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
         'spawn','command','open_monitor','complete_arm_cleanup','finish_cleanup','IncompleteTree','wait'}


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

    def __truediv__(self, name):
        def write(text):
            self.on_write(name)
            self.files[name] = text
        return SimpleNamespace(write_text=write,unlink=lambda missing_ok=False:self.files.pop(name,None))


class Mount:
    def __init__(self):
        self.mounted = True

    def is_mount(self):
        return self.mounted

    def __str__(self):
        return '/algorithm-only/runtime/doc'


class Handle:
    def __init__(self, harness, pid):
        self.harness, self.pid = harness, pid
        self.returncode = None
        self.state = 'S'
        self.communicate_code = 0
        self.wait_error = None
        self.wait_code = None
        self.on_wait = lambda: None
        self.on_communicate = lambda: None
        self.auto_unmount = True

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
        def create(argv, **options):
            assert options['start_new_session'] is True
            assert 'preexec_fn' not in options  # No inherited signal mask trick.
            handle = Handle(self,10 + len(self.handles))
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
        namespace = dict(__builtins__=__builtins__,contextmanager=contextmanager,sys=sys,Path=InertPath,json=json,re=re,
            children=[],stopped=set(),ownership_errors=[],out=self.output,
            user=lambda:None,processes=table,bus_owner=lambda name:(_ for _ in ()).throw(NoOwner()),
            GLib=SimpleNamespace(Error=NoOwner),
            Gio=SimpleNamespace(DBusError=SimpleNamespace(get_remote_error=lambda error:'org.freedesktop.DBus.Error.NameHasNoOwner')),
            os=SimpleNamespace(getpid=lambda:1000,kill=kill,killpg=kill),
            time=SimpleNamespace(time_ns=lambda:next(tick),monotonic=lambda:next(tick),sleep=lambda delay:None),
            signal=SimpleNamespace(SIGINT=2,SIGTERM=15,SIGKILL=9,signal=lambda signum,handler:self.handlers.__setitem__(signum,handler)),
            subprocess=SimpleNamespace(Popen=create,PIPE=-1,STDOUT=-2,CalledProcessError=subprocess.CalledProcessError),
            emit=lambda event,**values:self.events.append(('emit',event,values)),drain_adopted=lambda:[])
        parsed = ast.parse(SOURCE)
        selected = [copy.deepcopy(node) for node in parsed.body
                    if isinstance(node,(ast.FunctionDef,ast.ClassDef)) and node.name in NAMES]
        assert {node.name for node in selected} == NAMES
        exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-driver-definitions>','exec'),namespace)
        namespace['cancellation'] = namespace['Cancellation']()
        self.ns = namespace

    def spawn(self, resources=None, role='ui', mount=None):
        return self.ns['spawn'](['algorithm-only-never-execute'],resources=resources,role=role,expected_mount=mount)

    def repeat(self):
        self.ns['cancellation'].request(2,None)
        self.ns['cancellation'].request(15,None)


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
                    with self.assertRaisesRegex(RuntimeError,'Arm cleanup failed'): result(roles,'before',errors)
                result = h.ns['finish_cleanup'](errors)
                self.assertTrue(result['errors']); self.assertNotIn('cleanup-complete',h.output.files)

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
