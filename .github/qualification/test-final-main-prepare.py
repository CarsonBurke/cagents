"""Algorithm-only preparation tests, not producer inventories or run evidence.

Only pure definitions are AST-extracted from the actual stdlib preparation
module. Path is PurePosixPath; inventory reads, resource inspection, Cargo,
rustc and runtime file reads are injected inert functions. No process, cache,
GI, /proc, bus, mount, signal, helper or provider operation is performed.
Run only after independent source/isolation review and the lead's authorization.
"""
import ast
import copy
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
from types import SimpleNamespace
import unittest


SOURCE = Path(__file__).with_name('final-main-prepare.py').read_text()
FUNCTIONS = {'need','digest','encoded','path_text','reference','admit_references',
    'validate_inventories','admitted','environment','environment_binding','cargo_argv',
    'select_binary','select_libraries','fixture_argv','compile_matrix','verify_data',
    'verify_artifacts','verify_environment','check_arm_files'}
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
        return self.record

    def verify(self):
        self.api['verify_data'](self.record,self.plan,dict(sha='4'*40,tree='5'*40),'6'*64,'7'*64,self.refs)
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
        body = ast.get_source_segment(SOURCE,main)
        self.assertLess(body.index('admit_references('),body.index('.resolve('))
        self.assertLess(body.index('admit_references('),body.index('subprocess.'))
        self.assertLess(body.index('admit_references('),body.index('out.mkdir('))
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


if __name__ == '__main__':
    unittest.main()
