"""Algorithm-only preparation tests, not producer inventories or run evidence.

Only pure definitions are AST-extracted from the actual stdlib preparation
module. Path is PurePosixPath; inventory reads, resource inspection, Cargo,
rustc and runtime file reads are injected inert functions. No process, cache,
GI, /proc, bus, mount, signal, helper or provider operation is performed.
Run only after independent source/isolation review and the lead's authorization.
"""
import ast
import copy
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
from types import SimpleNamespace
import unittest


SOURCE = Path(__file__).with_name('final-main-prepare.py').read_text()
FUNCTIONS = {'need','digest','encoded','path_text','reference','admit_references',
    'validate_inventories','admitted','environment','environment_binding','cargo_argv',
    'select_binary','select_libraries','fixture_argv','compile_matrix','verify_data',
    'verify_artifacts','verify_environment','check_arm_files','verify_original_children',
    'decode_inventory','verify_inventory_bytes','live_inventories'}
CONSTANTS = {'PINNED_ROLES','FEATURES','SOURCES','PRIVATE','RENDERER','LATE','ENV_KEYS','ENV_PREFIXES'}


def pure_namespace():
    parsed = ast.parse(SOURCE)
    selected = []
    for node in parsed.body:
        if isinstance(node,ast.FunctionDef) and node.name in FUNCTIONS:
            selected.append(copy.deepcopy(node))
        elif isinstance(node,ast.Assign) and any(isinstance(target,ast.Name) and target.id in CONSTANTS for target in node.targets):
            selected.append(copy.deepcopy(node))
    assert {node.name for node in selected if isinstance(node,ast.FunctionDef)} == FUNCTIONS
    namespace = dict(__builtins__=__builtins__,Path=PurePosixPath,re=re,json=json,hashlib=hashlib,
                     HEX=re.compile('[0-9a-f]{64}'),os=SimpleNamespace(pathsep=':'))
    exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-pure-preparation-definitions>','exec'),namespace)
    return namespace


class Harness:
    """Hypothetical algorithm data; none of these paths/producers exist."""
    def __init__(self):
        self.api = pure_namespace()
        self.events = []
        self.plan = dict(before=dict(sha='b'*40,tree='c'*40),product=dict(sha='d'*40,tree='e'*40))
        ref = lambda path:dict(path=path,sha256='a'*64,bytes=17)
        self.inventories = {}
        for label in ('before','after'):
            self.inventories[label] = dict(kind='native-role-producer-inventory-v1',role=label,
                source=copy.deepcopy(self.plan['before' if label == 'before' else 'product']),
                checkout='/algorithm-only/' + label,target='/algorithm-only/native-' + label,
                producer=dict(run=101,attempt=1,job=202,head='f'*40,workflow_sha256='1'*64,
                              custody=ref('/algorithm-only/producer-custody-' + label)),
                cache=dict(key='synthetic-' + label,version=None,publication=ref('/algorithm-only/publication-' + label),
                           service_archive_sha256=None),
                profiles=dict(profile='dev-runtime',graphs=copy.deepcopy(self.api['FEATURES'])),
                tools={name:ref('/algorithm-only/tools/' + name) for name in ('cargo','rustc','zig','clang','libclang')},
                inputs={name:'2'*64 for name in self.api['SOURCES']})
        self.refs = {label:dict(path='/algorithm-only/inventory-' + label + '.json',
            sha256=self.api['digest'](self.api['encoded'](value)),bytes=len(self.api['encoded'](value)))
            for label,value in self.inventories.items()}
        self.raw_inventories = {label:self.api['encoded'](value) for label,value in self.inventories.items()}
        self.plan['native_preparation'] = dict(roles=copy.deepcopy(self.refs))
        self.parent = dict(HOME='/algorithm-only/parent-home',PATH='/algorithm-only/bin',
            CARGO_HOME='/algorithm-only/cargo-home',RUSTUP_HOME='/algorithm-only/rustup-home',
            RUSTUP_TOOLCHAIN='1.93.0',CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR='/algorithm-only/debug-T',
            CAGENTS_AGENT_AUTHORITY='synthetic-preservation-token',UNRELATED_SECRET='not-published')
        self.base = '/tmp/h2018.algorithm'
        self.envs = {label:self.api['environment'](self.parent,self.base,label,inventory)
                     for label,inventory in self.inventories.items()}
        self.compiled = {(label,graph):self.rows(label,graph) for label in ('before','after') for graph in ('cli','host','ui')}
        self.records = []

    def read(self, ref):
        label = next(label for label in ('before','after') if self.refs[label] == ref)
        self.events.append(('read-inventory',label))
        return self.inventories[label]

    def inspect(self, values, debug_target):
        assert set(values) == {'before','after'} and debug_target == '/algorithm-only/debug-T'
        self.events.append(('inspect-both-native-roots',))

    def admit(self, pins=None):
        return self.api['admitted'](self.plan,self.refs if pins is None else pins,self.read,self.inspect,'/algorithm-only/debug-T')

    def rows(self, label, graph):
        checkout,target = self.inventories[label]['checkout'],self.inventories[label]['target']
        name = {'cli':'cagents','host':'cagentsd','ui':'cagents-ui'}[graph]
        artifact = dict(reason='compiler-artifact',package_id='path+file://' + checkout + '#cagents@0.1.0',
            manifest_path=checkout + '/Cargo.toml',target=dict(name=name,kind=['bin'],
            src_path=checkout + ('/src/main.rs' if graph == 'cli' else '/src/bin/' + name + '.rs')),
            features=copy.deepcopy(self.api['FEATURES'][graph]),profile=dict(test=False),fresh=True,
            executable=target + '/dev-runtime/' + name,filenames=[target + '/dev-runtime/' + name])
        rows = [artifact]
        if graph == 'ui':
            own = copy.deepcopy(artifact)
            own.update(target=dict(name='cagents',kind=['lib'],src_path=checkout + '/src/lib.rs'),
                executable=None,filenames=[target + '/dev-runtime/deps/libcagents-ui.rlib'])
            serde = dict(reason='compiler-artifact',package_id='registry+algorithm-only#serde_json@1',
                target=dict(name='serde_json',kind=['lib']),features=[],profile=dict(test=False),executable=None,
                filenames=[target + '/dev-runtime/deps/libserde_json-test.rlib'])
            rows += [own,serde,dict(reason='build-script-executed',linked_paths=['native=' + target + '/dev-runtime/build/fixture-out'])]
        return rows

    def run(self, label, graph, argv, cwd, env, timeout):
        assert cwd == self.inventories[label]['checkout'] and env == self.envs[label]
        self.events.append(('run',label,graph))
        self.records.append(dict(arm=label,graph=graph,argv=list(argv),exit_code=0))
        return self.compiled.get((label,graph),[])

    def build(self):
        arms = self.api['compile_matrix'](self.inventories,self.envs,'/algorithm-only/control/fixture.rs',self.base,self.run)
        for arm in arms.values():
            paths = {name:value['executable'] for name,value in arm['triplet'].items()}
            paths.update({'library-' + name:path for name,path in arm['libraries'].items()})
            paths['fixture'] = arm['helper']
            arm['files'] = {name:dict(path=path,sha256='3'*64,bytes=100) for name,path in paths.items()}
        self.record = dict(kind='both-heads-prepared-v1',product=self.plan['product'],before=self.plan['before'],
            control=dict(sha='4'*40,tree='5'*40),module_sha256='6'*64,fixture_sha256='7'*64,
            roles=copy.deepcopy(self.refs),base=self.base,arms=arms,commands=copy.deepcopy(self.records))
        self.record['inventory_members'] = {label:dict(path='preparation/' + label + '-inventory.json',
            sha256=ref['sha256'],bytes=ref['bytes']) for label,ref in self.refs.items()}
        summary = dict(errors=[],streams_closed=True,children=[dict(argv=row['argv'],
            cwd=self.inventories[row['arm']]['checkout'],identity=dict(pid=10+i,group=10+i,stamp='algorithm-only'),
            published=True,error=None,waited=True,exit_code=0) for i,row in enumerate(self.records)],
            scope='Algorithm-only original-root fixture, not closure evidence.')
        self.record['original_children'] = summary
        raw = self.api['encoded'](summary) + b'\n'
        self.record['original_children_member'] = dict(path='preparation/original-children.json',sha256=self.api['digest'](raw),bytes=len(raw))
        return self.record

    def synchronize_algorithm_child(self, index):
        # Internal consistency of HYPOTHETICAL data only, never a genuine
        # child/provenance receipt. Let the intended later argv guard decide.
        summary = self.record['original_children']
        summary['children'][index]['argv'] = copy.deepcopy(self.record['commands'][index]['argv'])
        raw = self.api['encoded'](summary) + b'\n'
        self.record['original_children_member'] = dict(path='preparation/original-children.json',
            sha256=self.api['digest'](raw),bytes=len(raw))

    def verify(self):
        self.api['verify_data'](self.record,self.plan,dict(sha='4'*40,tree='5'*40),'6'*64,'7'*64,self.refs)
        self.api['verify_inventory_bytes'](self.record,self.raw_inventories,self.refs)
        self.api['verify_artifacts'](self.record,self.compiled,'/algorithm-only/control/fixture.rs')


class PreparationAlgorithms(unittest.TestCase):
    def test_current_null_roles_and_missing_or_unpinned_metadata_refuse_before_any_action(self):
        current = json.loads(Path(__file__).with_name('final-main-plan.json').read_text())
        self.assertEqual(current['native_preparation']['roles'],dict(before=None,after=None))
        self.assertEqual(pure_namespace()['PINNED_ROLES'],dict(before=None,after=None))
        cases = ['current-null','missing-role','one-role','bad-reference-hash','unpinned-reference']
        for case in cases:
            with self.subTest(case=case):
                h = Harness()
                pins = copy.deepcopy(h.refs)
                if case == 'current-null': h.plan['native_preparation']['roles'] = current['native_preparation']['roles']; pins = dict(before=None,after=None)
                if case == 'missing-role': del h.plan['native_preparation']['roles']['after']
                if case == 'one-role': h.plan['native_preparation']['roles']['after'] = None
                if case == 'bad-reference-hash': h.plan['native_preparation']['roles']['before']['sha256'] = 'not-a-hash'
                if case == 'unpinned-reference': h.plan['native_preparation']['roles']['before']['sha256'] = '8'*64
                with self.assertRaises(ValueError): h.admit(pins)
                self.assertEqual(h.events,[])  # No inventory read, cache inspection, resource or build callback.

    def test_both_complete_inventories_are_validated_before_resource_inspection(self):
        valid = Harness(); valid.admit()
        self.assertEqual(valid.events,[('read-inventory','before'),('read-inventory','after'),('inspect-both-native-roots',)])
        mutations = [
            ('producer',lambda h:h.inventories['after']['producer'].__setitem__('job',0)),
            ('source',lambda h:h.inventories['after']['source'].__setitem__('sha','9'*40)),
            ('profile',lambda h:h.inventories['after']['profiles'].__setitem__('profile','dev')),
            ('feature-graph',lambda h:h.inventories['after']['profiles']['graphs'].__setitem__('ui',['development','ui','host'])),
            ('invented-archive-hash',lambda h:h.inventories['after']['cache'].__setitem__('service_archive_sha256','0'*64)),
            ('tool-hash',lambda h:h.inventories['after']['tools']['zig'].__setitem__('sha256','bad')),
            ('same-native-root',lambda h:h.inventories['after'].__setitem__('target',h.inventories['before']['target'])),
            ('debug-root',lambda h:h.inventories['after'].__setitem__('target','/algorithm-only/debug-T')),
            ('nested-root',lambda h:h.inventories['after'].__setitem__('target',h.inventories['before']['target'] + '/nested')),
        ]
        for case, mutate in mutations:
            with self.subTest(case=case):
                h = Harness(); mutate(h)
                with self.assertRaises(ValueError): h.admit()
                self.assertEqual(h.events,[('read-inventory','before'),('read-inventory','after')])

    def test_exact_global_serial_six_cargo_and_two_fixture_matrix(self):
        h = Harness(); h.admit(); record = h.build(); h.verify()
        expected = [(label,graph) for label in ('before','after') for graph in ('cli','host','ui','fixture')]
        self.assertEqual([(row['arm'],row['graph']) for row in h.records],expected)
        self.assertEqual(h.events[:3],[('read-inventory','before'),('read-inventory','after'),('inspect-both-native-roots',)])
        self.assertEqual(h.events[3:],[('run',label,graph) for label,graph in expected])
        for label in ('before','after'):
            for graph, feature, targets in [('cli','development',['--bin','cagents']),
                    ('host','development,host',['--bin','cagentsd']),('ui','development,ui',['--lib','--bin','cagents-ui'])]:
                row = next(row for row in h.records if (row['arm'],row['graph']) == (label,graph))
                self.assertEqual(row['argv'],['cargo','build','--locked','--profile','dev-runtime','--no-default-features',
                    '--features',feature,*targets,'--message-format=json'])
            fixture = next(row['argv'] for row in h.records if (row['arm'],row['graph']) == (label,'fixture'))
            arm = record['arms'][label]
            self.assertEqual(fixture[:3],['rustc','--edition=2024','/algorithm-only/control/fixture.rs'])
            self.assertIn('cagents=' + arm['libraries']['cagents'],fixture)
            self.assertIn('serde_json=' + arm['libraries']['serde_json'],fixture)
            self.assertEqual(fixture[-2:],['-L','native=' + h.inventories[label]['target'] + '/dev-runtime/build/fixture-out'])
            self.assertEqual(set(arm['files']),{'cagents','cagentsd','cagents-ui','library-cagents','library-serde_json','fixture'})

    def test_canonical_JSON_roundtrip_keeps_role_objects_unordered_and_command_array_ordered(self):
        h=Harness();h.build()
        raw=h.api['encoded'](h.record)
        h.record=json.loads(raw)
        self.assertEqual(list(h.record['arms']),['after','before'])  # Sorted JSON object order is not role order.
        h.verify()  # Actual data, pinned raw inventories, original-child and artifact readers all accept it.
        for case in ('missing','extra','wrong-role','list','null'):
            with self.subTest(case=case):
                h.record=json.loads(raw)
                h.verify()
                if case=='missing':del h.record['arms']['before']
                if case=='extra':h.record['arms']['extra']=copy.deepcopy(h.record['arms']['before'])
                if case=='wrong-role':h.record['arms']['typo']=h.record['arms'].pop('before')
                if case=='list':h.record['arms']=['before','after']
                if case=='null':h.record['arms']=None
                with self.assertRaisesRegex(ValueError,'^prepared-both-heads$'):
                    h.api['verify_data'](h.record,h.plan,dict(sha='4'*40,tree='5'*40),'6'*64,'7'*64,h.refs)
        h.record=json.loads(raw)
        h.record['commands'][0],h.record['commands'][1]=h.record['commands'][1],h.record['commands'][0]
        with self.assertRaisesRegex(ValueError,'^prepared-command-order$'):
            h.api['verify_data'](h.record,h.plan,dict(sha='4'*40,tree='5'*40),'6'*64,'7'*64,h.refs)

    def test_actual_shaped_artifact_feature_source_target_and_library_negatives(self):
        cases = ['features','profile','source','target','manifest','duplicate','library-target','ui-library-features','package']
        for case in cases:
            with self.subTest(case=case):
                h = Harness(); h.build(); h.verify()  # Establish the valid positive before each independent mutation.
                h = Harness()
                artifact = h.compiled['after','ui'][0]
                if case == 'features': artifact['features'].append('host')
                if case == 'profile': artifact['profile']['test'] = True
                if case == 'source': artifact['target']['src_path'] = '/algorithm-only/wrong/src/bin/cagents-ui.rs'
                if case == 'target': artifact['executable'] = '/algorithm-only/native-before/dev-runtime/cagents-ui'
                if case == 'manifest': artifact['manifest_path'] = '/algorithm-only/wrong/Cargo.toml'
                if case == 'duplicate': h.compiled['after','ui'].append(copy.deepcopy(artifact))
                if case == 'library-target': h.compiled['after','ui'][1]['filenames'] = ['/algorithm-only/native-before/dev-runtime/deps/libcagents-ui.rlib']
                if case == 'ui-library-features': h.compiled['after','ui'][1]['features'].append('host')
                if case == 'package': h.compiled['after','ui'][1]['package_id'] = 'another-package'
                with self.assertRaises(ValueError): h.build()
                self.assertNotIn(('run','after','fixture'),h.events)

    def test_prepared_receipt_order_helper_artifact_and_source_mismatch_refuse(self):
        cases = ['missing-arm','command-order','failed-command','source','controller','module','fixture-source','roles',
                 'command-argv','selected','library','helper','fixture-link','file-set']
        for case in cases:
            with self.subTest(case=case):
                h = Harness(); h.build(); h.verify()
                arm = h.record['arms']['after']
                if case == 'missing-arm': del h.record['arms']['before']
                if case == 'command-order': h.record['commands'][3],h.record['commands'][4] = h.record['commands'][4],h.record['commands'][3]
                if case == 'failed-command': h.record['commands'][0]['exit_code'] = 1
                if case == 'source': h.record['product'] = dict(sha='0'*40,tree='0'*40)
                if case == 'controller': h.record['control']['tree'] = '0'*40
                if case == 'module': h.record['module_sha256'] = '0'*64
                if case == 'fixture-source': h.record['fixture_sha256'] = '0'*64
                if case == 'roles': h.record['roles']['after']['sha256'] = '0'*64
                if case == 'command-argv': h.record['commands'][4]['argv'].append('--features=host')
                if case == 'selected':
                    arm['triplet']['cagents-ui'] = copy.deepcopy(arm['triplet']['cagents-ui'])
                    arm['triplet']['cagents-ui']['fresh'] = False
                if case == 'library': arm['libraries']['cagents'] = '/algorithm-only/native-before/dev-runtime/deps/libother.rlib'
                if case == 'helper': arm['helper'] = '/tmp/h2018.algorithm/before-fixture'
                if case == 'fixture-link': h.record['commands'][7]['argv'][-1] = 'native=/algorithm-only/wrong'
                if case == 'file-set': arm['files']['fixture']['path'] = '/algorithm-only/wrong-fixture'
                if case in ('command-argv','fixture-link'):
                    h.synchronize_algorithm_child(4 if case == 'command-argv' else 7)
                    self.assertIsNone(h.api['verify_data'](h.record,h.plan,
                        dict(sha='4'*40,tree='5'*40),'6'*64,'7'*64,h.refs))
                    self.assertEqual(h.api['verify_inventory_bytes'](h.record,h.raw_inventories,h.refs),h.inventories)
                    reason = 'prepared-command-argv' if case == 'command-argv' else 'prepared-helper'
                    with self.assertRaisesRegex(ValueError,'^' + reason + '$'):
                        h.api['verify_artifacts'](h.record,h.compiled,'/algorithm-only/control/fixture.rs')
                else:
                    with self.assertRaises(ValueError): h.verify()

    def test_final_private_environment_preserves_authority_and_cargo_homes_without_secret_values(self):
        h = Harness()
        for label, env in h.envs.items():
            self.assertEqual(env['CAGENTS_AGENT_AUTHORITY'],h.parent['CAGENTS_AGENT_AUTHORITY'])
            for key in ('CARGO_HOME','RUSTUP_HOME','RUSTUP_TOOLCHAIN','CARGO_BUILD_JOBS'):
                self.assertEqual(env[key],h.parent[key])
            self.assertEqual(env['HOME'],h.base + '/' + label + '/home')
            self.assertEqual(env['CARGO_TARGET_DIR'],h.inventories[label]['target'])
            self.assertEqual(env['TMPDIR'],h.base + '/' + label + '/tmp')
            self.assertEqual(env['PATH'],'/algorithm-only/tools:' + h.parent['PATH'])
            binding = h.api['environment_binding'](env)
            self.assertNotIn('UNRELATED_SECRET',binding)
            self.assertNotIn('synthetic-preservation-token',json.dumps(binding))
            self.assertTrue({'CAGENTS_AGENT_AUTHORITY','TMPDIR','ICED_BACKEND','WGPU_BACKEND','LIBGL_ALWAYS_SOFTWARE'} <= set(binding))
        self.assertEqual(h.parent['HOME'],'/algorithm-only/parent-home')  # Caller mapping was not changed.

    def test_changed_prepared_runtime_environment_or_file_bytes_cannot_be_repaired(self):
        h = Harness(); h.build()
        arm = h.record['arms']['after']; env = h.envs['after']
        h.api['verify_environment'](arm,env)
        for key in ('HOME','TMPDIR','CARGO_TARGET_DIR','CARGO_HOME','RUSTUP_HOME','CAGENTS_LOCAL_DEBUG',
                    'ICED_BACKEND','WGPU_BACKEND','LIBGL_ALWAYS_SOFTWARE','RUSTUP_TOOLCHAIN','CC','RUSTFLAGS'):
            changed = dict(env); changed[key] = '/algorithm-only/changed-value'
            with self.subTest(key=key), self.assertRaises(ValueError): h.api['verify_environment'](arm,changed)
        late = dict(env,DISPLAY=':99',DBUS_SESSION_BUS_ADDRESS='algorithm-only-bus')
        h.api['verify_environment'](arm,late)  # Desktop endpoints exist only after preparation, not build inputs.
        actual = {value['path']:copy.deepcopy(value) for value in arm['files'].values()}
        reads = []
        def read(path):
            reads.append(path)
            return actual[path]
        h.api['file_binding'] = read
        h.api['check_arm_files'](arm)
        self.assertEqual(set(reads),set(actual))
        before = copy.deepcopy(arm)
        actual[arm['files']['fixture']['path']]['sha256'] = '0'*64
        with self.assertRaises(ValueError): h.api['check_arm_files'](arm)
        self.assertEqual(arm,before)  # Refusal never restamps or relinks a changed result.

    def test_source_wiring_keeps_global_preparation_before_the_first_desktop_and_no_current_override(self):
        nodes = ast.parse(SOURCE).body
        main = next(node for node in nodes if isinstance(node,ast.FunctionDef) and node.name == 'main')
        calls = [node for node in ast.walk(main) if isinstance(node,ast.Call)]
        named = lambda call,name:isinstance(call.func,ast.Name) and call.func.id == name
        attr = lambda node,owner,name:(isinstance(node,ast.Attribute) and isinstance(node.value,ast.Name)
                                      and node.value.id == owner and node.attr == name)
        admission = [call for call in calls if named(call,'admit_references')]
        self.assertEqual(len(admission),1)
        # Source coordinates belong to executable AST nodes, never comments
        # containing e.g. "subprocess.". Require every expected boundary to exist.
        boundaries = {
            'path resolve':[call for call in calls if isinstance(call.func,ast.Attribute) and call.func.attr == 'resolve'],
            'directory creation':[call for call in calls if isinstance(call.func,ast.Attribute) and call.func.attr == 'mkdir'],
            'original child owner':[call for call in calls if named(call,'PreparationChildren')],
            'owned metadata child':[call for call in calls if attr(call.func,'owner','capture')],
            'owned build child adapter':[call for call in calls if named(call,'command')],
            'temporary root factory':[call for call in calls if attr(call.func,'tempfile','mkdtemp')],
            'Popen factory reference':[node for node in ast.walk(main) if attr(node,'subprocess','Popen')],
        }
        for name, values in boundaries.items():
            self.assertTrue(values,name + ' boundary must exist')
            for boundary in values:
                self.assertLess(admission[0].lineno,boundary.lineno,name + ' must follow source-pinned admission')
        self.assertNotIn('allow-current',SOURCE)
        workflow = Path(__file__).parent.parent.joinpath('workflows/qualify-final-main-integration.yml').read_text()
        self.assertLess(workflow.index('id: native_admission'),workflow.index('uses: ./control/.github/actions/setup-linux'))
        self.assertLess(workflow.index('id: native_admission'),workflow.index('id: cache'))
        self.assertLess(workflow.index('--prepare '),workflow.index('exec xvfb-run'))
        self.assertLess(workflow.index('--verify '),workflow.index('exec xvfb-run'))
        driver = Path(__file__).with_name('final-main-native.py').read_text()
        self.assertNotIn('def compile_helper(',driver)
        self.assertNotIn("['cargo', 'build'",driver)
        self.assertNotIn("['rustc'",driver)
        self.assertIn("['scripts/run-development', '--browserless', '--debug'",driver)
        self.assertLess(driver.index('preparation.load_prepared('),driver.index("spawn([str(launcher)"))


class Stream:
    def __init__(self, harness):
        self.harness,self.closed = harness,False
        self.on_close = lambda:None

    def close(self):
        self.harness.events.append(('close',id(self)))
        self.on_close()
        self.closed = True


class OwnedHandle:
    def __init__(self, harness, pid, options):
        self.harness,self.pid,self.returncode = harness,pid,None
        self.stdin = None
        self.stdout = Stream(harness) if options['stdout'] == -1 else None
        self.stderr = Stream(harness) if options['stderr'] == -1 else None
        self.stamp = 'original-' + str(pid)

    def poll(self):
        return self.returncode

    def wait(self, timeout):
        self.harness.events.append(('wait',self.pid,timeout))
        self.harness.on_wait(self)
        if self.returncode is None: self.returncode = 0
        return self.returncode

    def communicate(self, timeout):
        self.harness.events.append(('communicate',self.pid,timeout))
        self.harness.on_wait(self)
        if self.returncode is None: self.returncode = 0
        return b'algorithm-only metadata',b''


class OwnershipHarness:
    def __init__(self):
        self.events,self.handles,self.handlers = [],[],{}
        self.on_create = lambda process:None
        self.on_identity = lambda process:None
        self.on_wait = lambda process:None
        api = pure_namespace()
        def create(argv, **options):
            assert self.cancel.depth > 0, 'Actual Popen creation must be inside cancellation deferral'
            assert options['start_new_session'] is True and 'preexec_fn' not in options
            process = OwnedHandle(self,100+len(self.handles),options)
            self.handles.append(process)
            self.events.append(('created',process.pid,list(argv)))
            self.on_create(process)
            return process
        def identity(process):
            self.events.append(('identity',process.pid))
            self.on_identity(process)
            return dict(pid=process.pid,group=process.pid,stamp=process.stamp)
        def signal_group(group, signum):
            self.events.append(('signal',group,signum))
            process = next(process for process in self.handles if process.pid == group)
            process.returncode = -signum
        api.update(contextmanager=contextmanager,sys=sys,
            signal=SimpleNamespace(SIGINT=2,SIGTERM=15,SIGKILL=9,
                signal=lambda signum,handler:self.handlers.__setitem__(signum,handler)),
            subprocess=SimpleNamespace(PIPE=-1,DEVNULL=-3,TimeoutExpired=subprocess.TimeoutExpired))
        definitions = {'PreparationCancelled','PreparationCancellation','PreparationChildren','settle_preparation'}
        selected = [copy.deepcopy(node) for node in ast.parse(SOURCE).body
                    if isinstance(node,(ast.ClassDef,ast.FunctionDef)) and node.name in definitions]
        assert {node.name for node in selected} == definitions
        exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-mocked-preparation-ownership>','exec'),api)
        self.api = api
        self.cancel = api['PreparationCancellation']()
        self.cancel.install()  # Injected registry only; no real signal operation.
        self.owner = api['PreparationChildren'](self.cancel,create,identity,signal_group)

    def cleanup(self):
        return self.api['settle_preparation'](self.owner)


class PreparationReviewRegressions(unittest.TestCase):
    def test_exact_pinned_raw_inventory_binds_live_and_offline_nested_receipts(self):
        for case in ('valid','producer-job','target','tool','publication','missing-raw','wrong-raw','wrong-member'):
            with self.subTest(case=case):
                h = Harness(); record = h.build(); h.verify()
                reads = []
                def read(ref):
                    label = next(label for label in ('before','after') if h.refs[label] == ref)
                    reads.append(copy.deepcopy(ref))
                    return h.raw_inventories[label]
                self.assertEqual(h.api['live_inventories'](record,read,h.refs),h.inventories)
                self.assertEqual(reads,[h.refs['before'],h.refs['after']])
                raw = copy.deepcopy(h.raw_inventories)
                nested = record['arms']['after']['inventory']
                if case == 'producer-job': nested['producer']['job'] += 1
                if case == 'target': nested['target'] += '-changed'
                if case == 'tool': nested['tools']['zig']['sha256'] = '4'*64
                if case == 'publication': nested['cache']['publication']['path'] += '-changed'
                if case == 'missing-raw': del raw['after']
                if case == 'wrong-raw': raw['after'] += b' '
                if case == 'wrong-member': record['inventory_members']['after']['path'] = 'preparation/before-inventory.json'
                if case == 'valid':
                    self.assertEqual(h.api['verify_inventory_bytes'](record,raw,h.refs),h.inventories)
                else:
                    with self.assertRaises(ValueError): h.api['verify_inventory_bytes'](record,raw,h.refs)
                    if case in ('producer-job','target','tool','publication'):
                        # No artifact/semantic guard can mask this exact raw/nested equality oracle.
                        with self.assertRaisesRegex(ValueError,'prepared-nested-inventory'):
                            h.api['live_inventories'](record,read,h.refs)
        h = Harness(); record = h.build()
        changed = json.loads(h.raw_inventories['after']); changed['producer']['job'] += 1
        def changed_live(ref):
            return h.api['encoded'](changed) if ref == h.refs['after'] else h.raw_inventories['before']
        with self.assertRaisesRegex(ValueError,'producer-inventory-bytes'):
            h.api['live_inventories'](record,changed_live,h.refs)

    def test_cancel_after_popen_return_before_registry_or_role_publication_retains_original(self):
        for stage in ('after-popen','identity','publication'):
            with self.subTest(stage=stage):
                h = OwnershipHarness()
                if stage == 'after-popen':
                    class Retained(list):
                        def append(self, process):
                            h.cancel.request(15,None)  # Popen has returned; actual append/publication has not happened.
                            self_before = len(self)
                            super().append(process)
                            h.events.append(('retained-after-cancel',process.pid,self_before))
                    h.owner.children = Retained()
                if stage == 'identity': h.on_identity = lambda process:h.cancel.request(15,None)
                if stage == 'publication':
                    original = h.owner.publish
                    def publish(record):
                        h.cancel.request(15,None)
                        original(record)
                    h.owner.publish = publish
                with self.assertRaises(h.api['PreparationCancelled']):
                    h.owner.capture(['git','algorithm-only-metadata'])
                self.assertEqual(h.owner.children,h.handles)
                self.assertEqual(len(h.handles),1)
                record = h.owner.records[h.handles[0]]
                self.assertTrue(record['published']); self.assertIsNotNone(record['identity'])
                summary = h.cleanup()
                self.assertEqual(summary['errors'],[])
                self.assertTrue(summary['streams_closed'])
                self.assertIn(('signal',100,15),h.events)
                self.assertIn(('wait',100,10),h.events)
                self.assertFalse(any(event[0] == 'communicate' for event in h.events))
                with self.assertRaises(h.api['PreparationCancelled']): h.cancel.finish(summary['errors'])
                self.assertTrue(all(callable(handler) for handler in h.handlers.values()))

    def test_first_and_repeated_control_during_wait_or_cleanup_never_runs_later_commands(self):
        for cleanup_first in (False,True):
            with self.subTest(cleanup_first=cleanup_first):
                h = OwnershipHarness()
                h.on_wait = lambda process:(h.cancel.request(15,None),h.cancel.request(2,None))
                if cleanup_first:
                    process = h.owner.start(['cargo','algorithm-only-build'],None,None,-1,-1)
                    summary = h.cleanup()  # FIRST control is inside the protected original wait.
                else:
                    with self.assertRaises(h.api['PreparationCancelled']):
                        h.owner.capture(['git','algorithm-only-metadata'])
                    summary = h.cleanup()
                self.assertEqual(h.cancel.signum,15)
                self.assertEqual(len(h.handles),1)
                self.assertTrue(summary['children'][0]['waited'])
                self.assertTrue(summary['streams_closed'])
                with self.assertRaises(h.api['PreparationCancelled']): h.cancel.finish(summary['errors'])
                # A caught first control still cannot authorize another child.
                with self.assertRaises(h.api['PreparationCancelled']):
                    h.owner.capture(['rustc','must-not-run-after-control'])
                self.assertEqual(len(h.handles),1)

    def test_metadata_cargo_and_rustc_share_the_actual_owned_path_and_stream_contract(self):
        h = OwnershipHarness()
        self.assertEqual(h.owner.capture(['git','algorithm-only-metadata']),b'algorithm-only metadata')
        for executable in ('cargo','rustc'):
            streams = [Stream(h),Stream(h)]
            for stream in streams: h.owner.streams.append(stream)
            self.assertEqual(h.owner.run([executable,'algorithm-only'],None,{},30,*streams),b'')
            self.assertTrue(all(stream.closed for stream in streams))
        summary = h.cleanup(); h.cancel.finish(summary['errors'])
        self.assertEqual(len(h.owner.children),3)
        self.assertTrue(all(child['waited'] and child['exit_code'] == 0 for child in summary['children']))
        self.assertFalse(any(event[0] == 'signal' for event in h.events))
        self.assertEqual([event[1] for event in h.events if event[0] == 'wait'],[101,102,102,101,100])
        self.assertTrue(summary['streams_closed'])

    def test_registration_generation_wait_stream_or_drain_failure_preserves_first_error_and_unknown(self):
        cases = ('identity','publication','changed-generation','wait','stream','drain')
        for case in cases:
            with self.subTest(case=case):
                h = OwnershipHarness(); original = RuntimeError('Original terminal work diagnostic')
                if case == 'identity':
                    def identity_error(process): raise original
                    h.on_identity = identity_error
                    with self.assertRaisesRegex(RuntimeError,'Original terminal work diagnostic'):
                        h.owner.capture(['git','algorithm-only-metadata'])
                elif case == 'publication':
                    def publication_error(record): raise original
                    h.owner.publish = publication_error
                    with self.assertRaisesRegex(RuntimeError,'Original terminal work diagnostic'):
                        h.owner.capture(['git','algorithm-only-metadata'])
                else:
                    process = h.owner.start(['cargo','algorithm-only'],None,None,-1,-1)
                    # Explicit work-owner unwind, rather than sys.exc_info inference.
                    try:
                        with h.cancel.work(): raise original
                    except RuntimeError: pass
                    if case == 'changed-generation': process.stamp = 'replacement'
                    if case == 'wait':
                        def wait_error(process): raise RuntimeError('Secondary wait failure')
                        h.on_wait = wait_error
                    if case == 'stream':
                        def close_error(): raise RuntimeError('Secondary stream failure')
                        process.stdout.on_close = close_error
                    if case == 'drain':
                        def drain_error(): raise RuntimeError('Secondary drain failure')
                        h.owner.cleanup = drain_error
                h.cancel.request(15,None); h.cancel.request(2,None)
                summary = h.cleanup()
                self.assertIs(h.cancel.failure,original)
                self.assertTrue(summary['errors'])
                self.assertEqual(len(h.owner.children),1)  # Never clear ownership on failure.
                with self.assertRaisesRegex(RuntimeError,'Original terminal work diagnostic'):
                    h.cancel.finish(summary['errors'])
                if case in ('identity','changed-generation'):
                    self.assertFalse(any(event[0] == 'signal' for event in h.events))
                    self.assertIn(('wait',100,10),h.events)



if __name__ == '__main__':
    unittest.main()
