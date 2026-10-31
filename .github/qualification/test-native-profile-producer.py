"""Hypothetical algorithm fixtures only; no actual producer/native evidence.

AST-extract pure definitions from the actual producer and reviewed preparation
source. No authored module import, main, subprocess, signal, procfs, filesystem
inventory, cache, GI, API, helper or provider boundary is executed. Fixed source
reads below are the only real Path operations. Run only after independent review.
"""
import ast
import copy
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
from types import SimpleNamespace
import unittest

DIRECTORY = Path(__file__).parent
SOURCE = (DIRECTORY/'native-profile-producer.py').read_text()
PREPARATION = (DIRECTORY/'final-main-prepare.py').read_text()
PLAN = json.loads((DIRECTORY/'native-profile-producer-plan.json').read_text())


def extracted():
    names = {'need','digest','encoded','reference','admit_references','admitted_contract','validate_admission',
        'proposed_layout','run_matrix','validate_leaves','validate_coverage','validate_manifest','verify_payload',
        'compiler_json','verify_compiler_evidence','cache_key','publication_proof','positive_roots','settle_writers',
        'drain_streams','user_context','owner_type'}
    constants = {'PINNED_ADMISSION_INPUTS','INPUT_NAMES','FEATURES','PHASE'}
    selected = [copy.deepcopy(node) for node in ast.parse(SOURCE).body
        if isinstance(node,ast.FunctionDef) and node.name in names
        or isinstance(node,ast.Assign) and any(isinstance(t,ast.Name) and t.id in constants for t in node.targets)]
    assert {node.name for node in selected if isinstance(node,ast.FunctionDef)} == names
    api = dict(__builtins__=__builtins__,PurePosixPath=PurePosixPath,posixpath=posixpath,json=json,hashlib=hashlib)
    exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-pure-producer-definitions>','exec'),api)
    prep_names = {'need','path_text','cargo_argv','select_binary','select_libraries','fixture_argv'}
    prep_selected = [copy.deepcopy(node) for node in ast.parse(PREPARATION).body
        if isinstance(node,ast.FunctionDef) and node.name in prep_names]
    assert {node.name for node in prep_selected} == prep_names
    prep = dict(__builtins__=__builtins__,Path=PurePosixPath,FEATURES=copy.deepcopy(api['FEATURES']))
    exec(compile(ast.Module(body=prep_selected,type_ignores=[]),'<actual-pure-preparation-selection>','exec'),prep)
    return api,SimpleNamespace(**{name:prep[name] for name in prep_names})


def roots():
    return dict(errors=[],streams_closed=True,children=[dict(identity={'pid':123,'stamp':'algorithm-only','group':123},
        published=True,error=None,waited=True,exit_code=0)],scope='Algorithm-only root ledger; not a process witness.')


class Fixture:
    def __init__(self):
        self.api,self.prep = extracted(); self.plan = copy.deepcopy(PLAN)
        self.layout = self.api['proposed_layout']('/algorithm-only/workspace','/algorithm-only/temp')
        self.layout['fixture'] = '/algorithm-only/workspace/control/.github/qualification/final-main-fixture.rs'
        self.records,self.logs,self.graphs = [],{},{}
        self.context = dict(repository=self.plan['repository'],event='pull_request',branch=self.plan['branch'],
            ref=self.plan['cache_ref'],pull_request=1933,run=123,attempt=1,job_name='algorithm-only',controller_sha='a'*40)
        self.environments = {label:{'algorithm-only':label} for label in ('before','after')}

    def rows(self, label, graph):
        checkout,target = self.layout['checkouts'][label],self.layout['targets'][label]
        name = {'cli':'cagents','host':'cagentsd','ui':'cagents-ui'}[graph]
        artifact = dict(reason='compiler-artifact',package_id='path+file://'+checkout+'#cagents@0.1.0',
            manifest_path=checkout+'/Cargo.toml',features=copy.deepcopy(self.api['FEATURES'][graph]),fresh=False,
            target=dict(name=name,kind=['bin'],src_path=checkout+('/src/main.rs' if graph == 'cli' else '/src/bin/'+name+'.rs')),
            executable=target+'/dev-runtime/'+name,filenames=[target+'/dev-runtime/'+name],profile=dict(test=False))
        result = [artifact]
        if graph == 'ui':
            library = copy.deepcopy(artifact)
            library.update(target=dict(name='cagents',kind=['lib'],src_path=checkout+'/src/lib.rs'),executable=None,
                filenames=[target+'/dev-runtime/deps/libcagents-test.rlib'])
            result += [library,dict(reason='compiler-artifact',target=dict(name='serde_json',kind=['lib']),
                filenames=[target+'/dev-runtime/deps/libserde_json-test.rlib']),
                dict(reason='build-script-executed',linked_paths=['native='+target+'/dev-runtime/build/test-out'])]
        return result

    def run(self, label, graph, argv, cwd, env, timeout):
        assert cwd == self.layout['checkouts'][label] and env == self.environments[label]
        rows = self.rows(label,graph) if graph != 'fixture' else []
        raw = b''.join(self.api['encoded'](row)+b'\n' for row in rows)
        self.graphs[label,graph] = rows
        row = dict(arm=label,graph=graph,argv=list(argv),cwd=cwd,exit_code=0)
        for name,data in [('stdout',raw),('stderr',b'')]:
            member='logs/'+label+'-'+graph+'.'+name
            self.logs[member] = data
            row[name+'_member'] = member
            row[name] = dict(path='/algorithm-only/proof/'+member,sha256=self.api['digest'](data),bytes=len(data))
        self.records.append(row)
        return rows

    def build(self):
        arms = self.api['run_matrix'](self.plan,self.layout,self.environments,self.prep,self.run)
        for arm in arms.values():
            paths = [(value['executable'],0o755) for value in arm['triplet'].values()]
            paths += [(path,0o644) for path in arm['libraries'].values()]+[(arm['helper'],0o755)]
            arm['leaves'] = sorted([dict(path=str(PurePosixPath(path).relative_to(arm['target'])),type='file',
                mode=mode,bytes=11,sha256='b'*64) for path,mode in paths],key=lambda value:value['path'])
        self.manifest = dict(kind='native-payload-manifest-v2',products=self.plan['products'],context=self.context,
            layout=self.layout,arms=arms,commands=self.records,fixture_sha256=self.plan['fixture_sha256'],
            compiler_settlement=dict(original_roots=roots(),kernel_echild=True,no_more_spawns=True,errors=[],external_entry_wait=None))
        self.api['validate_manifest'](self.manifest,self.plan)
        self.api['verify_compiler_evidence'](self.manifest,self.graphs,self.prep)
        return self.manifest


class ProducerAlgorithms(unittest.TestCase):
    def test_null_admission_and_inherited_authority_refuse_before_any_external_callback(self):
        f=Fixture(); reads=[]
        self.assertIsNone(PLAN['admission_inputs']); self.assertIsNone(f.api['PINNED_ADMISSION_INPUTS'])
        with self.assertRaisesRegex(ValueError,'SOURCE_ONLY'):
            f.api['admitted_contract'](PLAN,f.context,lambda ref:reads.append(ref))
        self.assertEqual(reads,[])
        env=dict(RUNNER_ENVIRONMENT='github-hosted',GITHUB_REPOSITORY=PLAN['repository'],GITHUB_EVENT_NAME='pull_request',
            GITHUB_HEAD_REF=PLAN['branch'],GITHUB_REF=PLAN['cache_ref'],PRODUCER_PULL_REQUEST='1933',GITHUB_RUN_ID='123',
            GITHUB_RUN_ATTEMPT='1',GITHUB_JOB='algorithm-only',PRODUCER_CONTROL_SHA='a'*40)
        self.assertEqual(f.api['user_context'](env),f.context)
        for value in ('','synthetic-authority'):
            changed=dict(env,CAGENTS_AGENT_AUTHORITY=value)
            with self.assertRaises(ValueError): f.api['user_context'](changed)
            self.assertEqual(changed['CAGENTS_AGENT_AUTHORITY'],value)
        reference=lambda name:dict(path='/algorithm-only/'+name,sha256='c'*64,bytes=17)
        refs={name:reference(name) for name in f.api['INPUT_NAMES']}
        policy=dict(kind='native-producer-policy-decision-v1',decision='allow-one-original-compile-only-producer',caps=f.plan['caps'],allocation='algorithm-only')
        grant=dict(kind='native-producer-exact-grant-v1',context=f.context,purpose=f.plan['purpose'],allocation='algorithm-only',policy_sha256=refs['policy']['sha256'],job_id=456)
        facts=dict(kind='native-producer-preflight-contract-v1',context=f.context,disk_free_bytes=100*1024**3,
            quota_headroom_bytes=100*1024**3,cache_entry_limit_bytes=100*1024**3,
            tools={name:reference(name) for name in ('cargo','rustc','zig','clang','libclang')})
        contract=dict(kind='native-producer-publication-contract-v1',cache_ref=f.context['ref'],
            save_action=f.plan['publication_contract']['cache_save_action'],official_readback='separate-original-collector',service_archive_sha256=None)
        docs=dict(policy=policy,grant=grant,preflight=facts,publication_contract=contract)
        f.api['validate_admission'](f.plan,f.context,docs,refs)  # Hypothetical valid contract, not an actual grant.
        for name,value in [('disk_free_bytes',None),('quota_headroom_bytes',0),('cache_entry_limit_bytes',None)]:
            changed=copy.deepcopy(docs);changed['preflight'][name]=value
            with self.assertRaises(ValueError): f.api['validate_admission'](f.plan,f.context,changed,refs)
        wrong=dict(f.context,ref='refs/pull/9999/merge')
        with self.assertRaises(ValueError): f.api['validate_admission'](f.plan,wrong,docs,refs)

    def test_serial_six_cargo_two_fixture_matrix_preserves_real_shaped_selection(self):
        f=Fixture();m=f.build()
        self.assertEqual([(row['arm'],row['graph']) for row in f.records],
            [(label,graph) for label in ('before','after') for graph in ('cli','host','ui','fixture')])
        for row in f.records:
            if row['graph'] in f.api['FEATURES']:
                self.assertEqual(row['argv'],f.prep.cargo_argv(row['graph']))
                self.assertNotIn('run',row['argv'])
                self.assertNotIn('test',row['argv'])
        self.assertNotEqual(m['arms']['before']['target'],m['arms']['after']['target'])
        for arm in m['arms'].values():
            self.assertTrue(arm['helper'].startswith(arm['target']+'/.qualification/'))
            self.assertEqual(arm['cargo_json_storage'],'proof-only')
        for case in ('features','profile','source','library','matrix','fixture'):
            with self.subTest(case=case):
                f=Fixture();f.build()
                if case=='features':f.graphs['after','ui'][0]['features'].append('host')
                if case=='profile':f.graphs['after','ui'][0]['profile']['test']=True
                if case=='source':f.graphs['after','ui'][0]['target']['src_path']='/algorithm-only/foreign.rs'
                if case=='library':f.manifest['arms']['after']['libraries']['serde_json']='/algorithm-only/foreign.rlib'
                if case=='matrix':f.records[0],f.records[1]=f.records[1],f.records[0]
                if case=='fixture':f.records[-1]['argv'][-1]='native=/algorithm-only/foreign'
                with self.assertRaises(ValueError):f.api['verify_compiler_evidence'](f.manifest,f.graphs,f.prep)

    def test_complete_payload_match_includes_extra_missing_content_modes_links_and_source(self):
        for case in ('missing','extra','hash','mode','type','source','profile-writer'):
            with self.subTest(case=case):
                f=Fixture();m=f.build();observed={label:copy.deepcopy(arm['leaves']) for label,arm in m['arms'].items()}
                f.api['verify_payload'](m,observed,f.plan)
                if case=='missing':observed['after'].pop()
                if case=='extra':observed['after'].append(dict(path='z-extra',type='file',mode=0o644,bytes=0,sha256='d'*64))
                if case=='hash':observed['after'][0]['sha256']='d'*64
                if case=='mode':observed['after'][0]['mode']=0o600
                if case=='type':observed['after'][0]=dict(path=observed['after'][0]['path'],type='symlink',mode=0o777,target='missing')
                if case=='source':m['arms']['after']['source']={'sha':'d'*40,'tree':'e'*40}
                if case=='profile-writer':m['compiler_settlement']['original_roots']['children'][0]['exit_code']=-9
                with self.assertRaises(ValueError):f.api['verify_payload'](m,observed,f.plan)
        f=Fixture();valid=[dict(path='file',type='file',mode=0o755,bytes=0,sha256='b'*64),dict(path='link',type='symlink',mode=0o777,target='file')]
        f.api['validate_leaves'](valid,f.plan['caps'])
        for target in ('../../outside','/outside','link'):
            changed=copy.deepcopy(valid);changed[-1]['target']=target
            with self.assertRaises(ValueError):f.api['validate_leaves'](changed,f.plan['caps'])

    def test_bound_cargo_bytes_and_each_cached_artifact_cannot_be_substituted(self):
        f=Fixture();m=f.build();read=lambda path:f.logs[path]
        self.assertEqual(f.api['compiler_json'](m,read),{key:value for key,value in f.graphs.items() if key[1] in f.api['FEATURES']})
        member=f.records[0]['stdout_member'];f.logs[member]+=b' '
        with self.assertRaisesRegex(ValueError,'compiler-log-bytes'):f.api['compiler_json'](m,read)
        f=Fixture();m=f.build();arm=m['arms']['after'];rows={graph:f.graphs['after',graph] for graph in ('cli','host','ui')}
        f.api['validate_coverage'](arm,arm['leaves'],rows)
        rows['ui'].append(dict(reason='compiler-artifact',filenames=['/algorithm-only/foreign-output']))
        with self.assertRaisesRegex(ValueError,'compiler-output-outside-role'):f.api['validate_coverage'](arm,arm['leaves'],rows)
        f=Fixture();m=f.build();arm=m['arms']['after'];arm['leaves']=[leaf for leaf in arm['leaves'] if leaf['path']!='.qualification/fixture']
        with self.assertRaisesRegex(ValueError,'missing-cargo-output'):
            f.api['validate_coverage'](arm,arm['leaves'],{graph:f.graphs['after',graph] for graph in ('cli','host','ui')})

    def test_M_key_P_and_acceptance_are_separate_and_preexisting_or_ambiguous_rows_refuse(self):
        f=Fixture();m=f.build();raw=f.api['encoded'](m);key=f.api['cache_key'](raw)
        self.assertNotIn('key',m)
        for field in ('key','cache_key','sha256','P','A','proof_artifact'):
            changed=copy.deepcopy(m);changed[field]='must-not-enter-M'
            with self.assertRaisesRegex(ValueError,'M-cycle'):f.api['validate_manifest'](changed,f.plan)
        ref=lambda name:dict(path='/algorithm-only/'+name,sha256='e'*64,bytes=17)
        log=dict(run=f.context['run'],attempt=1,controller_sha=f.context['controller_sha'],key=key,ref=f.context['ref'],
            started_at='2030-01-01T00:00:00Z',completed_at='2030-01-01T00:01:00Z',stdout=ref('save-out'),stderr=ref('save-err'))
        row=dict(id=999,ref=f.context['ref'],key=key,version='algorithm-version',size_in_bytes=123,created_at='2030-01-01T00:00:30Z')
        result=f.api['publication_proof'](raw,f.context,'success',[],[row],log,f.context['ref'])
        self.assertIsNone(result['service_archive_sha256']);self.assertIsNone(result['external_acceptance']);self.assertFalse(result['qualified'])
        for case in ('preexisting','ambiguous','ref','key','version','size','before-save','save-failed'):
            before=[];after=[copy.deepcopy(row)];outcome='success'
            if case=='preexisting':before=[copy.deepcopy(row)]
            if case=='ambiguous':after.append(copy.deepcopy(row))
            if case=='ref':after[0]['ref']='refs/pull/9999/merge'
            if case=='key':after[0]['key']='wrong'
            if case=='version':after[0]['version']=None
            if case=='size':after[0].pop('size_in_bytes');after[0]['size']=123
            if case=='before-save':after[0]['created_at']='2029-12-31T23:59:59Z'
            if case=='save-failed':outcome='failure'
            with self.subTest(case=case),self.assertRaises(ValueError):f.api['publication_proof'](raw,f.context,outcome,before,after,log,f.context['ref'])

    def test_scoped_kernel_wait_not_procfs_absence_controls_compiler_writer_settlement(self):
        f=Fixture()
        for case in ('positive','live','signalled','root-unwaited','root-signalled','stream-open','missing-identity'):
            with self.subTest(case=case):
                summary=roots();events=[];ticks=[0]
                if case=='root-unwaited':summary['children'][0]['waited']=False
                if case=='root-signalled':summary['children'][0]['exit_code']=-9
                if case=='stream-open':summary['streams_closed']=False
                if case=='missing-identity':summary['children'][0]['identity']=None
                owner=SimpleNamespace(spawning_closed=False)
                def original_roots(actual):
                    self.assertIs(actual,owner);self.assertTrue(owner.spawning_closed)
                    events.append('original-waits-and-streams');return summary
                owner.preparation=SimpleNamespace(settle_preparation=original_roots)
                answers=[(234,9 if case=='signalled' else 0),'ECHILD']
                def waitpid(pid,flags):
                    self.assertEqual((pid,flags),(-1,1));events.append('kernel-wait')
                    if case=='live':return 0,0
                    answer=answers.pop(0)
                    if answer=='ECHILD':raise ChildProcessError()
                    return answer
                def now():ticks[0]+=.25;return ticks[0]
                result=f.api['settle_writers'](owner,waitpid,now,lambda seconds:None,2,1)
                self.assertEqual(events[0],'original-waits-and-streams')
                self.assertTrue(owner.spawning_closed);self.assertIsNone(result['external_entry_wait'])
                if case=='positive':self.assertTrue(result['kernel_echild']);self.assertEqual(result['errors'],[])
                else:self.assertTrue(result['errors'])
                if case=='root-unwaited':self.assertNotIn('kernel-wait',events)

        events=[]
        class FakeBase:
            def __init__(self): pass
            def start(self, *args, **kwargs): events.append('inert original Popen');return object()
        Owned=f.api['owner_type'](SimpleNamespace(PreparationChildren=FakeBase))
        owner=Owned();owner.start(['algorithm-only'])
        owner.spawning_closed=True
        with self.assertRaisesRegex(ValueError,'Popen-after-writer-boundary'):owner.start(['must-not-spawn'])
        self.assertEqual(events,['inert original Popen'])

    def test_pipe_cap_deadline_and_cancellation_are_bounded_synchronous_callbacks(self):
        f=Fixture();writes=[];polls=[]
        sequence=[([('stdout','A')],False),([],True)]
        def poll():polls.append(True);return sequence.pop(0)
        counts=f.api['drain_streams'](poll,lambda token:b'bytes',lambda name,raw:writes.append((name,raw)),
            lambda:0,1,10,10,lambda:None)
        self.assertEqual(counts,dict(stdout=5,stderr=0));self.assertEqual(writes,[('stdout',b'bytes')])
        with self.assertRaisesRegex(ValueError,'producer-log-cap'):
            f.api['drain_streams'](lambda:([('stdout','A')],False),lambda token:b'12345',lambda *args:self.fail('over-cap write'),lambda:0,1,4,4,lambda:None)
        with self.assertRaisesRegex(ValueError,'deadline'):
            f.api['drain_streams'](lambda:self.fail('poll after deadline'),lambda token:b'',lambda *args:None,lambda:2,1,10,10,lambda:None)
        class Cancelled(Exception):pass
        def cancel():raise Cancelled('algorithm-only first control')
        with self.assertRaises(Cancelled):
            f.api['drain_streams'](lambda:self.fail('poll after control'),lambda token:b'',lambda *args:self.fail('write after control'),lambda:0,1,10,10,cancel)

    def test_source_routing_caps_and_no_desktop_consumer_or_permission_bypass(self):
        producer=(DIRECTORY.parent/'workflows/produce-native-profile-cache.yml').read_text()
        consumer=(DIRECTORY.parent/'workflows/qualify-final-main-integration.yml').read_text()
        legacy=(DIRECTORY.parent/'workflows/qualify-held-scope.yml').read_text()
        self.assertIn('if: false &&',consumer);self.assertIn("github.event.pull_request.number == 1933",producer)
        self.assertIn("github.event.pull_request.head.sha == '3e2c925b",legacy)
        self.assertNotIn('actions/cache/restore@',producer);self.assertNotIn('xvfb-run',producer)
        self.assertNotIn('actions: write',producer);self.assertNotIn('continue-on-error',producer)
        self.assertIn('cancel-in-progress: false',producer)
        self.assertEqual(PLAN['caps']['job_seconds'],150*60);self.assertLessEqual(PLAN['caps']['cargo_seconds'],4200)
        self.assertLess(PLAN['caps']['compile_seconds']+PLAN['caps']['adopted_wait_seconds']+PLAN['caps']['inventory_seconds'],PLAN['caps']['entry_seconds'])
        main=next(node for node in ast.parse(SOURCE).body if isinstance(node,ast.FunctionDef) and node.name=='main')
        calls=[node for node in ast.walk(main) if isinstance(node,ast.Call)]
        admission=[node for node in calls if isinstance(node.func,ast.Name) and node.func.id=='admit_references']
        self.assertEqual(len(admission),1)
        boundaries=[node for node in calls if isinstance(node.func,ast.Attribute) and node.func.attr in ('mkdir','exists','statvfs','mkdtemp','CDLL')]
        self.assertTrue(boundaries)
        for node in boundaries:self.assertLess(admission[0].lineno,node.lineno)
        self.assertLess(producer.index('--admission-only'),producer.index('uses: ./control/.github/actions/setup-linux'))
        self.assertLess(producer.index('Save the exact new payload'),producer.index('--post-save'))


if __name__ == '__main__':
    unittest.main()
