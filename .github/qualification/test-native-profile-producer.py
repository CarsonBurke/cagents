"""Hypothetical algorithm fixtures only; no actual producer/native evidence.

AST-extract pure definitions from the actual producer and reviewed preparation
source. No authored module import, main, subprocess, signal, procfs, filesystem
inventory, cache, GI, API, helper or provider boundary is executed. Fixed source
reads below are the only real Path operations. Run only after independent review.
"""
import ast
from contextlib import contextmanager
import copy
from datetime import datetime, timedelta
from email.utils import parsedate_to_datetime
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import sys
from types import SimpleNamespace
import unittest
from urllib.parse import urlencode

DIRECTORY = Path(__file__).parent
SOURCE = (DIRECTORY/'native-profile-producer.py').read_text()
PREPARATION = (DIRECTORY/'final-main-prepare.py').read_text()
PLAN = json.loads((DIRECTORY/'native-profile-producer-plan.json').read_text())


def extracted():
    names = {'need','digest','encoded','reference','admit_references','admitted_contract','validate_admission',
        'proposed_layout','run_matrix','validate_leaves','validate_coverage','validate_manifest','verify_payload',
        'compiler_json','verify_compiler_evidence','cache_key','publication_proof','positive_roots','settle_writers',
        'drain_streams','user_context','owner_type','readback_request','selected_headers','fetch_readback',
        'http_observation','save_declaration','complete_body_length'}
    constants = {'PINNED_ADMISSION_INPUTS','INPUT_NAMES','FEATURES','PHASE','API_HOST','API_VERSION','SAVE_SHA','SAFE_HEADERS'}
    selected = [copy.deepcopy(node) for node in ast.parse(SOURCE).body
        if isinstance(node,ast.FunctionDef) and node.name in names
        or isinstance(node,ast.Assign) and any(isinstance(t,ast.Name) and t.id in constants for t in node.targets)]
    assert {node.name for node in selected if isinstance(node,ast.FunctionDef)} == names
    api = dict(__builtins__=__builtins__,PurePosixPath=PurePosixPath,posixpath=posixpath,json=json,hashlib=hashlib,
        datetime=datetime,timedelta=timedelta,parsedate_to_datetime=parsedate_to_datetime,urlencode=urlencode,re=re)
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


def cancellation_types():
    names = {'PreparationCancelled','PreparationCancellation'}
    selected = [copy.deepcopy(node) for node in ast.parse(PREPARATION).body
                if isinstance(node,ast.ClassDef) and node.name in names]
    assert {node.name for node in selected} == names
    def unavailable(*args, **kwargs):
        raise AssertionError('No signal installation or external cancellation boundary in algorithm data')
    api = dict(__builtins__=__builtins__,contextmanager=contextmanager,
               sys=SimpleNamespace(exc_info=sys.exc_info),signal=SimpleNamespace(signal=unavailable),
               need=lambda value,reason: value or unavailable(reason))
    exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-reviewed-cancellation-classes>','exec'),api)
    return api['PreparationCancellation'],api['PreparationCancelled']


class Fixture:
    def __init__(self):
        self.api,self.prep = extracted(); self.plan = copy.deepcopy(PLAN)
        self.layout = self.api['proposed_layout']('/algorithm-only/workspace','/algorithm-only/temp')
        self.layout['fixture'] = '/algorithm-only/workspace/control/.github/qualification/final-main-fixture.rs'
        self.records,self.logs,self.graphs = [],{},{}
        self.context = dict(repository=self.plan['repository'],event='pull_request',branch=self.plan['branch'],
            ref=self.plan['cache_ref'],pull_request=1933,run=123,attempt=1,
            job_name=self.plan['publication_contract']['logical_job'],controller_sha='a'*40)
        self.sources={name:dict(path='/algorithm-only/control/'+name,sha256='c'*64,bytes=17)
                      for name in ('module','workflow','plan')}
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
            controller_sha=self.context['controller_sha'],publication_sources=self.sources,
            plan_sha256=self.sources['plan']['sha256'],
            compiler_settlement=dict(original_roots=roots(),kernel_echild=True,no_more_spawns=True,errors=[],external_entry_wait=None))
        self.api['validate_manifest'](self.manifest,self.plan)
        self.api['verify_compiler_evidence'](self.manifest,self.graphs,self.prep)
        return self.manifest


def http_data(f, request, phase, rows, date):
    body=f.api['encoded'](dict(total_count=len(rows),actions_caches=rows))
    headers=f.api['encoded'](f.api['selected_headers']([('Date',date),('X-GitHub-Request-Id','algorithm-only'),
        ('Content-Type','application/json; charset=utf-8')]))
    data=dict(body=body,headers=headers,observation=dict(kind='original-cache-http-observation-v1',phase=phase,
        request=copy.deepcopy(request),status=200,complete=True,error=None,closure_errors=[],
        observed_start='algorithm-only',observed_finish='algorithm-only'))
    rebind_http(f,data)
    return data


def rebind_http(f, data):
    for name in ('body','headers'):
        raw=data[name]
        data['observation'][name]=dict(path='/algorithm-only/'+data['observation']['phase']+'.'+name,
            sha256=f.api['digest'](raw),bytes=len(raw))


def publication_data(f):
    raw=f.api['encoded'](f.manifest)
    request=f.api['readback_request'](raw,f.context,f.plan,f.sources,f.layout)
    before=http_data(f,request,'before',[],'Tue, 01 Jan 2030 00:00:00 GMT')
    row=dict(id=999,ref=request['ref'],key=request['key'],version='algorithm-version',size_in_bytes=123,
             created_at='2030-01-01T00:00:30Z')
    after=http_data(f,request,'after',[row],'Tue, 01 Jan 2030 00:01:00 GMT')
    save=f.api['save_declaration'](request,f.layout,'success','success')
    return raw,request,before,after,save


class HttpHarness:
    """Inert socket/HTTP objects, retained bytes in RAM; no external factory."""
    def __init__(self, f, request):
        self.f,self.request=f,request;self.events=[];self.retained={};self.status=200
        self.headers=[('Date','Tue, 01 Jan 2030 00:00:00 GMT'),('X-GitHub-Request-Id','algorithm-only'),
                      ('Content-Type','application/json'),('Set-Cookie','must-not-retain'),('Authorization','must-not-retain')]
        self.chunks=[f.api['encoded'](dict(total_count=0,actions_caches=[])),b'']
        self.token='algorithm-only-readback-token';self.clock=lambda:0
        self.after_read=lambda item:None
        self.stamp=lambda:'algorithm-only'
    def factory(self, timeout):
        self.events.append(('factory',timeout));outer=self
        class Socket:
            def settimeout(self, value):outer.events.append(('timeout',value))
        class Response:
            status=outer.status
            def getheaders(self):return outer.headers
            def read1(self, maximum):
                outer.events.append(('read',maximum));item=outer.chunks.pop(0)
                if isinstance(item,BaseException):raise item
                outer.after_read(item)
                return item
            def close(self):outer.events.append(('response-close',))
        class Connection:
            sock=Socket()
            def connect(self):outer.events.append(('connect',))
            def request(self, method, path, headers):
                assert method=='GET' and path==outer.request['path']
                assert headers['Authorization']=='Bearer '+outer.token
                outer.events.append(('GET',path))  # Deliberately never retain token/header map.
            def getresponse(self):return Response()
            def close(self):outer.events.append(('connection-close',))
        return Connection()
    def retain(self, name, raw):
        assert name not in self.retained
        self.retained[name]=raw
        return dict(path='/algorithm-only/'+name,sha256=self.f.api['digest'](raw),bytes=len(raw))
    def fetch(self):
        return self.f.api['fetch_readback'](self.request,self.token,'before',self.factory,self.clock,
            self.stamp,self.f.plan['readback_caps'],self.retain)


class ProducerAlgorithms(unittest.TestCase):
    def test_null_admission_and_inherited_authority_refuse_before_any_external_callback(self):
        f=Fixture(); reads=[]
        self.assertIsNone(PLAN['admission_inputs']); self.assertIsNone(f.api['PINNED_ADMISSION_INPUTS'])
        with self.assertRaisesRegex(ValueError,'SOURCE_ONLY'):
            f.api['admitted_contract'](PLAN,f.context,lambda ref:reads.append(ref))
        self.assertEqual(reads,[])
        env=dict(RUNNER_ENVIRONMENT='github-hosted',GITHUB_REPOSITORY=PLAN['repository'],GITHUB_EVENT_NAME='pull_request',
            GITHUB_HEAD_REF=PLAN['branch'],GITHUB_REF=PLAN['cache_ref'],PRODUCER_PULL_REQUEST='1933',GITHUB_RUN_ID='123',
            GITHUB_RUN_ATTEMPT='1',GITHUB_JOB=f.context['job_name'],PRODUCER_CONTROL_SHA='a'*40)
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
            save_action=f.plan['publication_contract']['cache_save_action'],
            official_readback=f.plan['publication_contract']['official_readback'],service_archive_sha256=None)
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

    def test_canonical_JSON_roundtrip_keeps_role_objects_unordered_and_command_array_ordered(self):
        f=Fixture();f.build()
        f.manifest=json.loads(f.api['encoded'](f.manifest))
        self.assertEqual(list(f.manifest['arms']),['after','before'])  # Real canonical encoding sorts object keys.
        f.api['validate_manifest'](f.manifest,f.plan)
        f.api['verify_compiler_evidence'](f.manifest,f.graphs,f.prep)
        f.api['verify_payload'](f.manifest,{label:arm['leaves'] for label,arm in f.manifest['arms'].items()},f.plan)
        raw,request,before,after,save=publication_data(f)
        self.assertFalse(f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)['publication_qualified'])
        for case in ('missing','extra','wrong-role','list','null'):
            with self.subTest(case=case):
                value=json.loads(raw)
                f.api['validate_manifest'](value,f.plan)
                if case=='missing':del value['arms']['before']
                if case=='extra':value['arms']['extra']=copy.deepcopy(value['arms']['before'])
                if case=='wrong-role':value['arms']['typo']=value['arms'].pop('before')
                if case=='list':value['arms']=['before','after']
                if case=='null':value['arms']=None
                with self.assertRaisesRegex(ValueError,'^manifest-roles$'):f.api['validate_manifest'](value,f.plan)
        value=json.loads(raw)
        value['commands'][0],value['commands'][1]=value['commands'][1],value['commands'][0]
        f.api['validate_manifest'](value,f.plan)  # Object membership remains valid; ARRAY order must still fail.
        with self.assertRaisesRegex(ValueError,'^compiler-matrix$'):
            f.api['verify_compiler_evidence'](value,f.graphs,f.prep)

    def test_complete_payload_match_includes_extra_missing_content_modes_links_and_source(self):
        for case in ('missing','extra','hash','mode','type','source','profile-writer'):
            with self.subTest(case=case):
                f=Fixture();m=f.build();observed={label:copy.deepcopy(arm['leaves']) for label,arm in m['arms'].items()}
                f.api['verify_payload'](m,observed,f.plan)
                if case=='missing':observed['after'].pop()
                if case=='extra':observed['after'].append(dict(path='z-extra',type='file',mode=0o644,bytes=0,sha256='d'*64))
                if case=='hash':observed['after'][0]['sha256']='d'*64
                if case=='mode':observed['after'][0]['mode']=0o600
                if case=='type':
                    path=observed['after'][0]['path']
                    target=next(leaf['path'] for leaf in observed['after'] if leaf['path']!=path and leaf['type']=='file')
                    observed['after'][0]=dict(path=path,type='symlink',mode=0o777,
                        target=posixpath.relpath(target,str(PurePosixPath(path).parent)))
                    f.api['validate_leaves'](observed['after'],f.plan['caps'])
                if case=='source':m['arms']['after']['source']={'sha':'d'*40,'tree':'e'*40}
                if case=='profile-writer':m['compiler_settlement']['original_roots']['children'][0]['exit_code']=-9
                with self.assertRaisesRegex(ValueError,'complete-payload-mismatch' if case=='type' else ''):
                    f.api['verify_payload'](m,observed,f.plan)
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
        raw,request,before,after,save=publication_data(f)
        result=f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)
        self.assertIsNone(result['service_archive_sha256']);self.assertIsNone(result['external_acceptance']);self.assertFalse(result['qualified'])
        self.assertFalse(result['publication_qualified']);self.assertIsNone(result['official_save_log'])
        self.assertIsNone(result['proof_artifact_identity']);self.assertNotIn('save_log',result)
        for case in ('preexisting','ambiguous','ref','key','version','size','before-save','save-failed'):
            raw,request,before,after,save=publication_data(f);rows=json.loads(after['body'])
            if case=='preexisting':before['body']=after['body'];rebind_http(f,before)
            if case=='ambiguous':rows['actions_caches'].append(copy.deepcopy(rows['actions_caches'][0]));rows['total_count']=2
            if case=='ref':rows['actions_caches'][0]['ref']='refs/pull/9999/merge'
            if case=='key':rows['actions_caches'][0]['key']='wrong'
            if case=='version':rows['actions_caches'][0]['version']=None
            if case=='size':rows['actions_caches'][0].pop('size_in_bytes');rows['actions_caches'][0]['size']=123
            if case=='before-save':rows['actions_caches'][0]['created_at']='2029-12-31T23:59:58Z'
            if case=='save-failed':save['outcome']='failure'
            after['body']=f.api['encoded'](rows);rebind_http(f,after)
            with self.subTest(case=case),self.assertRaises(ValueError):
                f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)

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

    def test_recoverable_observation_control_precedes_positive_drain_and_original_wait(self):
        Cancellation,Cancelled=cancellation_types()
        class RecoverableObservation(Exception):pass
        for stage in ('poll','before-original-wait'):
            with self.subTest(stage=stage):
                f=Fixture();cancel=Cancellation();events=[]
                def handled_observation():
                    try:raise RecoverableObservation('algorithm-only stale observation')
                    except RecoverableObservation:
                        events.append('caught-observation')
                        cancel.request(15,None)  # Actual reviewed logic; no handler install/OS signal.
                        self.assertFalse(cancel.delivered)
                    return [],True
                if stage=='poll':
                    with self.assertRaises(Cancelled),cancel.work():
                        f.api['drain_streams'](handled_observation,lambda token:self.fail('unexpected read'),
                            lambda *args:self.fail('unexpected write'),lambda:0,1,10,10,cancel.checkpoint)
                        events.append('positive-return')
                        self.fail('Original wait must not follow a latched observation control')
                else:
                    class Output:
                        def write(self, raw):raise AssertionError('unexpected write')
                        def close(self):events.append('inert-output-close')
                    class Process:
                        stdout=object();stderr=object()
                        def wait(self, **kwargs):
                            events.append('original-wait')
                            raise AssertionError('Original wait started with pending cancellation')
                    process=Process()
                    class Base:
                        def __init__(self, control):self.cancellation=control;self.records={process:{}}
                        def open_output(self, path):return Output()
                        def start(self, *args, **kwargs):events.append('inert-owned-child');return process
                    class Selector:
                        def register(self, *args):pass
                        def close(self):pass
                    def completed_drain(*args):
                        handled_observation()
                        return dict(stdout=0,stderr=0)
                    f.api.update(Path=PurePosixPath,subprocess=SimpleNamespace(PIPE=object()),
                        selectors=SimpleNamespace(DefaultSelector=Selector,EVENT_READ=1),
                        time=SimpleNamespace(monotonic=lambda:0),drain_streams=completed_drain)
                    Owned=f.api['owner_type'](SimpleNamespace(PreparationChildren=Base))
                    with self.assertRaises(Cancelled):
                        Owned(cancel).logged(['algorithm-only'],'/algorithm-only',{},1,
                            '/algorithm-only/log',f.plan['caps'],1)
                self.assertTrue(cancel.delivered)
                self.assertEqual(events.count('caught-observation'),1)
                self.assertNotIn('positive-return',events)
                self.assertNotIn('original-wait',events)
                self.assertIsNone(cancel.failure)
        cancel=Cancellation();original=ValueError('original terminal failure')
        with self.assertRaises(ValueError) as caught:
            with cancel.work():
                try:raise original
                finally:cancel.request(15,None)
        self.assertIs(caught.exception,original)
        self.assertIs(cancel.failure,original)
        with self.assertRaises(ValueError) as caught:cancel.finish([])
        self.assertIs(caught.exception,original)

    def test_fixed_get_retains_original_body_and_only_labeled_parsed_safe_headers(self):
        f=Fixture();f.build();raw,request,before,after,save=publication_data(f)
        harness=HttpHarness(f,request)
        harness.headers.append(('Content-Length',str(len(harness.chunks[0]))))
        observation=harness.fetch()
        self.assertEqual([event for event in harness.events if event[0]=='GET'],[('GET',request['path'])])
        self.assertEqual(len([event for event in harness.events if event[0]=='factory']),1)
        self.assertEqual(harness.retained['body'],f.api['encoded'](dict(total_count=0,actions_caches=[])))
        self.assertEqual(observation,json.loads(harness.retained['json']))
        headers=json.loads(harness.retained['headers.json'])
        self.assertEqual(headers['representation'],'Selected parsed fields; not raw wire header bytes.')
        self.assertEqual(set(headers['values']),{'date','x-github-request-id','content-type','content-length'})
        retained=b''.join(harness.retained.values())
        self.assertNotIn(harness.token.encode(),retained);self.assertNotIn(b'must-not-retain',retained)
        self.assertEqual(harness.events[-2:],[('response-close',),('connection-close',)])
        rows,date=f.api['http_observation'](dict(observation=observation,body=harness.retained['body'],
            headers=harness.retained['headers.json']),request,'before',f.plan['readback_caps'])
        self.assertEqual(rows,[]);self.assertEqual(date.isoformat(),'2030-01-01T00:00:00+00:00')
        self.assertIn('r123-a1-',request['key'])
        self.assertEqual(request['url'],'https://api.github.com'+request['path'])
        self.assertIn('ref=refs%2Fpull%2F1933%2Fmerge',request['path'])
        self.assertNotIn('version=',request['path'])  # Absence must cover ALL versions.

    def test_http_failure_partial_caps_redirects_and_endpoint_changes_never_retry(self):
        for case in ('redirect','forbidden','server-error','partial','body-cap','duplicate-date','deadline','declared-longer-EOF'):
            with self.subTest(case=case):
                f=Fixture();f.build();raw,request,*rest=publication_data(f)
                baseline=HttpHarness(f,request);baseline.fetch()
                harness=HttpHarness(f,request)
                if case=='redirect':harness.status=302;harness.headers.append(('Location','https://example.invalid'))
                if case=='forbidden':harness.status=403
                if case=='server-error':harness.status=500
                if case=='partial':harness.chunks=[b'{partial',OSError('algorithm-only network failure')]
                if case=='body-cap':harness.chunks=[b'x'*(f.plan['readback_caps']['body_bytes']+1)]
                if case=='duplicate-date':harness.headers.append(('Date','Tue, 01 Jan 2030 00:00:00 GMT'))
                if case=='declared-longer-EOF':harness.headers.append(('Content-Length',str(len(harness.chunks[0])+7)))
                if case=='deadline':
                    times=iter([0,100]);harness.clock=lambda:next(times)
                if case in ('redirect','forbidden','server-error'):
                    observation=harness.fetch()
                    with self.assertRaisesRegex(ValueError,'readback-not-complete-success'):
                        f.api['http_observation'](dict(observation=observation,body=harness.retained['body'],
                            headers=harness.retained['headers.json']),request,'before',f.plan['readback_caps'])
                else:
                    with self.assertRaises((ValueError,OSError)):harness.fetch()
                    observation=json.loads(harness.retained['json'])
                    self.assertFalse(observation['complete']);self.assertIsNotNone(observation['error'])
                self.assertEqual(len([event for event in harness.events if event[0]=='factory']),1)
                self.assertLessEqual(len([event for event in harness.events if event[0]=='GET']),1)
                self.assertIn(('connection-close',),harness.events)
                if case=='partial':self.assertEqual(harness.retained['body'],b'{partial')
                if case=='declared-longer-EOF':
                    self.assertEqual(json.loads(harness.retained['body']),dict(total_count=0,actions_caches=[]))
                self.assertNotIn(harness.token.encode(),b''.join(harness.retained.values()))
        for field,value in [('method','POST'),('host','example.invalid'),('url','http://api.github.com/'),('path','/alternate')]:
            f=Fixture();f.build();raw,request,*rest=publication_data(f);harness=HttpHarness(f,request)
            harness.request=copy.deepcopy(request);harness.request[field]=value
            with self.assertRaisesRegex(ValueError,'fixed-readback-request'):harness.fetch()
            self.assertEqual(harness.events,[]);self.assertEqual(harness.retained,{})
        f=Fixture();f.build();raw,request,*rest=publication_data(f);harness=HttpHarness(f,request);harness.token=None
        with self.assertRaisesRegex(ValueError,'step-scoped-readback-token-required'):harness.fetch()
        self.assertEqual(harness.events,[])

    def test_returned_EOF_or_bytes_after_deadline_never_complete_or_admit_save(self):
        for stage in ('in-cap-EOF','late-EOF','late-bytes'):
            with self.subTest(stage=stage):
                f=Fixture();f.build();raw,request,*rest=publication_data(f);harness=HttpHarness(f,request)
                expected=harness.chunks[0];clock=[0];harness.clock=lambda:clock[0]
                def returned(item):
                    if stage=='late-EOF' and not item or stage=='late-bytes' and item:
                        clock[0]=f.plan['readback_caps']['request_seconds']+1
                harness.after_read=returned
                if stage=='in-cap-EOF':
                    observation=harness.fetch()
                    rows,date=f.api['http_observation'](dict(observation=observation,
                        body=harness.retained['body'],headers=harness.retained['headers.json']),
                        request,'before',f.plan['readback_caps'])
                    self.assertEqual(rows,[]);self.assertTrue(observation['complete'])
                else:
                    with self.assertRaisesRegex(ValueError,'readback-deadline'):harness.fetch()
                    observation=json.loads(harness.retained['json'])
                    self.assertFalse(observation['complete']);self.assertEqual(observation['error'],'ValueError')
                    self.assertEqual(harness.retained['body'],expected)
                    with self.assertRaisesRegex(ValueError,'readback-not-complete-success'):
                        f.api['http_observation'](dict(observation=observation,
                            body=harness.retained['body'],headers=harness.retained['headers.json']),
                            request,'before',f.plan['readback_caps'])  # No validated absence -> no save gate.
                self.assertEqual(len([event for event in harness.events if event[0]=='factory']),1)
                self.assertEqual(len([event for event in harness.events if event[0]=='GET']),1)

    def test_finish_stamp_failure_preserves_original_error_and_partial_response(self):
        for original in (OSError('algorithm-only original read failure'),None):
            with self.subTest(prior_failure=original is not None):
                f=Fixture();f.build();raw,request,*rest=publication_data(f);harness=HttpHarness(f,request)
                secondary=RuntimeError('algorithm-only finish stamp failure');stamps=[0]
                def stamp():
                    stamps[0]+=1
                    if stamps[0]==1:return 'algorithm-only-start'
                    raise secondary
                harness.stamp=stamp
                if original is not None:harness.chunks=[b'{retained-prefix',original]
                expected=harness.chunks[0]
                with self.assertRaises(type(original or secondary)) as caught:harness.fetch()
                self.assertIs(caught.exception,original or secondary)
                observation=json.loads(harness.retained['json'])
                self.assertEqual(harness.retained['body'],expected)
                self.assertFalse(observation['complete']);self.assertIsNone(observation['observed_finish'])
                self.assertEqual(observation['finish_error'],'RuntimeError')
                self.assertEqual(observation['error'],type(original or secondary).__name__)
                self.assertIn(('response-close',),harness.events);self.assertIn(('connection-close',),harness.events)
                with self.assertRaisesRegex(ValueError,'readback-not-complete-success'):
                    f.api['http_observation'](dict(observation=observation,
                        body=harness.retained['body'],headers=harness.retained['headers.json']),
                        request,'before',f.plan['readback_caps'])

    def test_complete_pages_UTC_clock_row_and_save_binding_have_independent_negative_controls(self):
        cases=('incomplete-count','next-page','missing-row','zero-id','bool-id','empty-version','zero-size',
               'non-UTC-created','bad-date','naive-date','reversed-date','too-early','too-late',
               'changed-body-bytes','wrong-save-action','wrong-save-path','wrong-save-conclusion','wrong-scope','wrong-source',
               'declared-longer','declared-shorter','length-malformed','length-overcap')
        for case in cases:
            with self.subTest(case=case):
                f=Fixture();f.build();raw,request,before,after,save=publication_data(f)
                self.assertFalse(f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)['publication_qualified'])
                rows=json.loads(after['body']);headers=json.loads(after['headers'])
                if case=='incomplete-count':rows['total_count']=2
                if case=='next-page':headers['values']['link']='<https://api.github.com/next>; rel="next"'
                if case=='missing-row':rows=dict(total_count=0,actions_caches=[])
                if case=='zero-id':rows['actions_caches'][0]['id']=0
                if case=='bool-id':rows['actions_caches'][0]['id']=True
                if case=='empty-version':rows['actions_caches'][0]['version']=''
                if case=='zero-size':rows['actions_caches'][0]['size_in_bytes']=0
                if case=='non-UTC-created':rows['actions_caches'][0]['created_at']='2030-01-01T01:00:30+01:00'
                if case=='bad-date':headers['values']['date']='not-a-date'
                if case=='naive-date':headers['values']['date']='Tue, 01 Jan 2030 00:01:00'
                if case=='reversed-date':headers['values']['date']='Mon, 31 Dec 2029 23:59:59 GMT'
                if case=='too-early':rows['actions_caches'][0]['created_at']='2029-12-31T23:59:58Z'
                if case=='too-late':rows['actions_caches'][0]['created_at']='2030-01-01T00:01:02Z'
                if case=='wrong-save-action':save['action_sha']='f'*40
                if case=='wrong-save-path':save['declared_paths'][-1]='/algorithm-only/foreign'
                if case=='wrong-save-conclusion':save['conclusion']='failure'
                if case=='wrong-scope':request['ref']='refs/pull/9999/merge'
                if case=='wrong-source':request['sources']['module']['sha256']='d'*64
                if case=='declared-longer':headers['values']['content-length']=str(len(after['body'])+1)
                if case=='declared-shorter':headers['values']['content-length']=str(len(after['body'])-1)
                if case=='length-malformed':headers['values']['content-length']='-1'
                if case=='length-overcap':headers['values']['content-length']=str(f.plan['readback_caps']['body_bytes']+1)
                after['body']=f.api['encoded'](rows);after['headers']=f.api['encoded'](headers);rebind_http(f,after)
                if case=='changed-body-bytes':after['body']+=b' '
                with self.assertRaises(ValueError):f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)
        for created in ('2029-12-31T23:59:59Z','2030-01-01T00:01:01Z'):
            f=Fixture();f.build();raw,request,before,after,save=publication_data(f)
            rows=json.loads(after['body']);rows['actions_caches'][0]['created_at']=created
            after['body']=f.api['encoded'](rows);rebind_http(f,after)
            self.assertFalse(f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)['publication_qualified'])
        f=Fixture();f.build();raw,request,before,after,save=publication_data(f)
        headers=json.loads(after['headers']);headers['values']['content-length']=str(len(after['body']))
        after['headers']=f.api['encoded'](headers);rebind_http(f,after)
        self.assertFalse(f.api['publication_proof'](raw,f.context,request,before,after,save,f.plan)['publication_qualified'])

    def test_publication_source_wiring_scopes_token_and_requires_pre_save_gate(self):
        workflow=(DIRECTORY.parent/'workflows/produce-native-profile-cache.yml').read_text()
        self.assertEqual(workflow.count('PRODUCER_READBACK_TOKEN: ${{ github.token }}'),2)
        self.assertEqual(workflow.count('uses: actions/cache/save@'),1)
        f=Fixture()
        self.assertIn('uses: actions/cache/save@'+f.api['SAVE_SHA'],workflow)
        self.assertIn('    permissions:\n      contents: read\n      actions: read',workflow)
        self.assertNotIn('actions: write',workflow);self.assertNotIn('permissions: write-all',workflow)
        before=workflow.index('--pre-save-readback');save=workflow.index('uses: actions/cache/save@');after=workflow.index('--post-save-readback')
        self.assertLess(before,save);self.assertLess(save,after)
        self.assertIn("if: ${{ success() && steps.pre_save.outcome == 'success' }}",workflow)
        compiler=workflow[workflow.index('      - name: ORIGINAL serial'):workflow.index('      # M is immutable')]
        self.assertNotIn('TOKEN',compiler);self.assertNotIn('github.token',compiler)
        self.assertIn('PRODUCER_SAVE_CONCLUSION: ${{ steps.save_native.conclusion }}',workflow)
        self.assertEqual(PLAN['readback_caps'],dict(request_seconds=60,socket_seconds=15,body_bytes=1048576,step_seconds=120))
        main=next(node for node in ast.parse(SOURCE).body if isinstance(node,ast.FunctionDef) and node.name=='main')
        segment=ast.get_source_segment(SOURCE,main)
        self.assertLess(segment.index("'PRODUCER_READBACK_TOKEN' not in os.environ"),segment.index('prep_path ='))
        self.assertLess(segment.index('admit_references(plan)'),segment.index('perform_readback('))
        self.assertIsNone(PLAN['admission_inputs']);self.assertIn('UNSUPPLIED',PLAN['remaining_seams'][0])
        self.assertNotIn('--post-save\'',SOURCE)  # Unsupported old save-log prototype removed.

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
