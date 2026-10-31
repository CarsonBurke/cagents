"""Inert loader algorithms only; not preparation, native or producer evidence.

Only the fixed owning source/plan reads below touch real paths. Tests extract
actual loader definitions, use in-memory read suppliers and execute harmless
assignment/function fixtures. No authored module import, actual preparation
module execution, main, resources, subprocess, cache, GI, API or signal boundary
is entered. Run only after independent source/isolation review.
"""
import ast
import builtins
import copy
import hashlib
import json
from pathlib import Path, PurePosixPath
from types import SimpleNamespace
import unittest

DIRECTORY = Path(__file__).parent
NAMES = ('native','producer','checker')
FILES = {'native':'final-main-native.py','producer':'native-profile-producer.py',
         'checker':'final-main-check.py','supervisor':'final-main-supervisor.py',
         'preparation':'final-main-prepare.py'}
SOURCES = {name:(DIRECTORY / path).read_bytes() for name,path in FILES.items()}
PRODUCER_PLAN = json.loads((DIRECTORY / 'native-profile-producer-plan.json').read_bytes())
NATIVE_PLAN = json.loads((DIRECTORY / 'final-main-plan.json').read_bytes())
PIN = '289d578f7465b00b182a0f18f89b6861b6aaffce34d41d78ad1ff9c73b1236a6'
MODULE_PATH = '/algorithm-only/control/.github/qualification/final-main-prepare.py'
FIXTURE_PATH = '/algorithm-only/control/.github/qualification/final-main-fixture.rs'
PREPARED_PATH = '/algorithm-only/evidence/preparation/prepared.json'


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def parsed(name):
    return ast.parse(SOURCES[name], filename=FILES[name])


def function(name, target):
    matches = [node for node in parsed(name).body if isinstance(node,ast.FunctionDef) and node.name == target]
    assert len(matches) == 1
    return copy.deepcopy(matches[0])


def literal(name, target):
    matches = [node.value for node in parsed(name).body if isinstance(node,ast.Assign)
               and any(isinstance(value,ast.Name) and value.id == target for value in node.targets)]
    assert len(matches) == 1
    return ast.literal_eval(matches[0])


def call_name(node):
    return ast.unparse(node.func) if isinstance(node,ast.Call) else None


class MemoryPath:
    """No real Path methods are available inside extracted loader code."""
    def __init__(self, harness, value):
        self.harness,self.value = harness,str(PurePosixPath(value))

    def __truediv__(self, other):
        return MemoryPath(self.harness,str(PurePosixPath(self.value) / other))

    def __str__(self):
        return self.value

    def read_bytes(self):
        return self.harness.read(self.value)


def harmless_source(marker='retained first buffer'):
    return ("MARKER = " + repr(marker) + "\n"
            "OBSERVED_FILE = __file__\nOBSERVED_NAME = __name__\n"
            "OBSERVED_PACKAGE = __package__\nOBSERVED_CACHE = __cached__\n"
            "VERIFY_CALLS = []\n"
            "def verify_data(record, plan, control, module_sha, fixture_sha):\n"
            "    VERIFY_CALLS.append((record, plan, control, module_sha, fixture_sha))\n"
            "    raise RuntimeError('algorithm-only stop after verification binding')\n").encode()


class LoaderHarness:
    def __init__(self, site, raw=None, expected=None, extra=()):
        self.site = site
        self.raw = harmless_source() if raw is None else raw
        self.expected = digest(self.raw) if expected is None else expected
        self.reads,self.compiles,self.executions = [],[],[]
        self.second = harmless_source('UNVERIFIED second buffer')
        self.read_error = None
        self.other = {}
        self.module = None
        self.control = MemoryPath(self,'/algorithm-only/control')
        self.root = MemoryPath(self,'/algorithm-only/evidence')
        selected = {'preparation_module',*extra}
        if site in ('producer','checker'):
            selected |= {'need','digest'}
        nodes = [function(site,name) for name in sorted(selected)]
        # Deliberately hypothetical pin for harmless code. The separate source
        # invariant checks the actual production pins and unchanged module.
        self.namespace = dict(__builtins__=builtins.__dict__,hashlib=hashlib,SimpleNamespace=SimpleNamespace,
                              PREPARATION_SHA=self.expected,json=json,Path=PurePosixPath)
        builtins.exec(compile(ast.Module(body=nodes,type_ignores=[]),'<actual-local-loader-definitions>','exec'),self.namespace)
        self.namespace.update(compile=self.compile,exec=self.execute)

    def read(self, value):
        self.reads.append(value)
        if value == MODULE_PATH:
            if self.read_error is not None:
                raise self.read_error
            return self.raw if self.reads.count(MODULE_PATH) == 1 else self.second
        if value in self.other:
            return self.other[value]
        raise AssertionError('Unexpected inert read: ' + value)

    def compile(self, raw, filename, mode):
        self.compiles.append((raw,filename,mode))
        return builtins.compile(raw,filename,mode)

    def execute(self, code, namespace):
        self.executions.append(namespace)
        return builtins.exec(code,namespace)

    def load(self):
        if self.site == 'producer':
            value = self.namespace['preparation_module'](self.control,self.expected)
        else:
            value = self.namespace['preparation_module'](self.control)
        self.module = value[0] if self.site == 'checker' else value
        return value


class PreparationByteLoadingTests(unittest.TestCase):
    def test_source_pins_and_exact_supervisor_driver_binding(self):
        self.assertEqual(digest(SOURCES['preparation']),PIN)
        self.assertEqual(literal('native','PREPARATION_SHA'),PIN)
        self.assertEqual(literal('checker','PREPARATION_SHA'),PIN)
        self.assertEqual(literal('supervisor','PREPARATION_SHA'),PIN)
        self.assertEqual(PRODUCER_PLAN['preparation_module_sha256'],PIN)
        self.assertEqual(literal('supervisor','DRIVER_SHA'),digest(SOURCES['native']))
        for site in NAMES:
            with self.subTest(site=site):
                loader = function(site,'preparation_module')
                calls = [node for node in ast.walk(loader) if isinstance(node,ast.Call)]
                self.assertEqual(sum(call_name(node) == 'path.read_bytes' for node in calls),1)
                self.assertEqual(sum(call_name(node) == 'exec' for node in calls),1)
                execution = next(node for node in calls if call_name(node) == 'exec')
                self.assertEqual(call_name(execution.args[0]),'compile')
                self.assertEqual(ast.dump(execution.args[0].args[0]),ast.dump(ast.Name(id='raw',ctx=ast.Load())))
                self.assertFalse(any('importlib' in ast.unparse(node) or 'exec_module' in ast.unparse(node) for node in calls))

    def test_all_loaders_execute_the_verified_single_buffer(self):
        for site in NAMES:
            with self.subTest(site=site):
                harness = LoaderHarness(site)
                value = harness.load()
                self.assertEqual(harness.module.MARKER,'retained first buffer')
                self.assertEqual(harness.reads,[MODULE_PATH])
                self.assertEqual(harness.compiles,[(harness.raw,MODULE_PATH,'exec')])
                self.assertEqual(len(harness.executions),1)
                self.assertEqual(harness.executions[0]['MARKER'],'retained first buffer')
                if site == 'checker':
                    self.assertEqual(value[1],digest(harness.raw))
                # The supplier's next bytes differ; the loader must not select
                # them or ask any path/bytecode implementation to reopen source.
                self.assertNotEqual(harness.raw,harness.second)

    def test_digest_mismatch_refuses_before_compile_or_execution(self):
        for site in NAMES:
            for actual,expected in [(harmless_source(),'0'*64),(harmless_source('changed source'),digest(harmless_source()))]:
                with self.subTest(site=site,expected=expected):
                    harness = LoaderHarness(site,raw=actual,expected=expected)
                    with self.assertRaisesRegex(ValueError,'Frozen preparation module changed|preparation-source'):
                        harness.load()
                    self.assertEqual(harness.reads,[MODULE_PATH])
                    self.assertEqual(harness.compiles,[])
                    self.assertEqual(harness.executions,[])

    def test_module_identity_is_source_named_and_non_main(self):
        expected = {'native':'final_main_preparation','producer':'reviewed_preparation','checker':'checked_preparation'}
        for site in NAMES:
            with self.subTest(site=site):
                harness = LoaderHarness(site)
                harness.load()
                module = harness.module
                self.assertEqual(module.OBSERVED_FILE,MODULE_PATH)
                self.assertEqual(module.OBSERVED_NAME,expected[site])
                self.assertNotEqual(module.OBSERVED_NAME,'__main__')
                self.assertIsNone(module.OBSERVED_PACKAGE)
                self.assertIsNone(module.OBSERVED_CACHE)

    def test_read_failure_preserves_original_and_never_executes(self):
        for site in NAMES:
            with self.subTest(site=site):
                harness = LoaderHarness(site)
                original = OSError('algorithm-only unavailable source')
                harness.read_error = original
                with self.assertRaises(OSError) as observed:
                    harness.load()
                self.assertIs(observed.exception,original)
                self.assertEqual(harness.reads,[MODULE_PATH])
                self.assertEqual(harness.compiles,[])
                self.assertEqual(harness.executions,[])

    def test_checker_reuses_digest_for_receipt_and_verification(self):
        def baseline():
            harness = LoaderHarness('checker',extra={'check_preparation'})
            record = {'algorithm_only':'not a preparation/native evidence packet'}
            raw_record = json.dumps(record,sort_keys=True).encode()
            fixture = b'algorithm-only fixture source bytes'
            harness.other.update({PREPARED_PATH:raw_record,FIXTURE_PATH:fixture})
            source = dict(control_sha='a'*40,control_tree='b'*40)
            native = dict(preparation_sha256=digest(raw_record),preparation_module_sha256=digest(harness.raw))
            return harness,record,fixture,source,native
        harness,record,fixture,source,native = baseline()
        with self.assertRaisesRegex(RuntimeError,'^algorithm-only stop after verification binding$'):
            harness.namespace['check_preparation'](harness.root,harness.control,{},source,native)
        self.assertEqual(harness.reads,[MODULE_PATH,PREPARED_PATH,FIXTURE_PATH])
        self.assertEqual(harness.compiles,[(harness.raw,MODULE_PATH,'exec')])
        calls = harness.executions[0]['VERIFY_CALLS']
        self.assertEqual(calls,[(record,{},dict(sha=source['control_sha'],tree=source['control_tree']),digest(harness.raw),digest(fixture))])
        for field in ('preparation_sha256','preparation_module_sha256'):
            with self.subTest(field=field):
                harness,_,_,source,native = baseline()
                native[field] = '0'*64
                with self.assertRaisesRegex(ValueError,'^preparation-native-binding$'):
                    harness.namespace['check_preparation'](harness.root,harness.control,{},source,native)
                self.assertEqual(harness.reads,[MODULE_PATH,PREPARED_PATH])
                self.assertEqual(harness.executions[0]['VERIFY_CALLS'],[])

    def test_null_preparation_and_producer_admission_remain_closed(self):
        roles = {'before':None,'after':None}
        self.assertEqual(literal('preparation','PINNED_ROLES'),roles)
        self.assertEqual(NATIVE_PLAN['native_preparation']['roles'],roles)
        self.assertIsNone(literal('producer','PINNED_ADMISSION_INPUTS'))
        self.assertIsNone(PRODUCER_PLAN['admission_inputs'])
        # Execute only the actual pure admission definitions inside the inert
        # fixture, not the real preparation module or any of its top-level code.
        pure = [ast.Assign(targets=[ast.Name(id='PINNED_ROLES',ctx=ast.Store())],value=ast.parse(repr(roles),mode='eval').body),
                function('preparation','need'),function('preparation','admit_references')]
        pure_source = ast.unparse(ast.fix_missing_locations(ast.Module(body=pure,type_ignores=[]))).encode()
        for site in NAMES:
            with self.subTest(site=site):
                harness = LoaderHarness(site,raw=pure_source)
                harness.load()
                harness.executions[0]['reference'] = lambda value: self.fail('NULL admission inspected a reference')
                with self.assertRaisesRegex(ValueError,'^Native roles UNESTABLISHED;'):
                    harness.module.admit_references(roles,harness.module.PINNED_ROLES)
                self.assertEqual(harness.reads,[MODULE_PATH])
        namespace = dict(__builtins__=builtins.__dict__,PINNED_ADMISSION_INPUTS=None)
        definitions = [function('producer','need'),function('producer','admit_references')]
        builtins.exec(compile(ast.Module(body=definitions,type_ignores=[]),'<actual-pure-producer-admission>','exec'),namespace)
        with self.assertRaisesRegex(ValueError,'^SOURCE_ONLY: separate exact producer admission UNSUPPLIED$'):
            namespace['admit_references'](PRODUCER_PLAN)

    def test_producer_loader_remains_after_admission_in_produce_only_route(self):
        main = function('producer','main')
        calls = [node for node in ast.walk(main) if isinstance(node,ast.Call)]
        unique = {}
        for name in ('admit_references','admitted_contract','preparation_module'):
            matching = [node for node in calls if call_name(node) == name]
            self.assertEqual(len(matching),1)
            unique[name] = matching[0]
        self.assertLess(unique['admit_references'].lineno,unique['admitted_contract'].lineno)
        self.assertLess(unique['admitted_contract'].lineno,unique['preparation_module'].lineno)
        self.assertEqual(ast.unparse(unique['preparation_module'].args[1]),"plan['preparation_module_sha256']")
        prior_returns = [node for node in main.body if isinstance(node,ast.If)
                         and node.lineno < unique['preparation_module'].lineno
                         and any(isinstance(value,ast.Return) for value in ast.walk(node))]
        self.assertEqual(len(prior_returns),2)
        self.assertEqual(ast.unparse(prior_returns[0].test),"sys.argv[1] == '--admission-only'")
        self.assertEqual(ast.unparse(prior_returns[1].test),"sys.argv[1] in ('--pre-save-readback', '--post-save-readback')")
        self.assertTrue(any(isinstance(value,ast.Return) and call_name(value.value) == 'perform_readback'
                            for value in ast.walk(prior_returns[1])))
        self.assertFalse(any(isinstance(node,ast.Call) and 'exec_module' in call_name(node) for node in calls))


if __name__ == '__main__':
    unittest.main()
