"""Algorithm-only DTO fixtures; never native evidence or a passing capsule.

Fixed owning source reads are the only real Path operations. Actual pure reader
functions/constants are AST-extracted; the frozen driver validator is an inert,
explicitly hypothetical callback. No authored module import, filesystem reader,
process, signal, resource, GI, API, probe or old test suite is executed here.
"""
import ast
import copy
import hashlib
from pathlib import Path
import re
import unittest

DIRECTORY = Path(__file__).parent
SOURCE = (DIRECTORY / 'final-main-local-settlement.py').read_bytes()
SUPERVISOR = (DIRECTORY / 'final-main-supervisor.py').read_bytes()
DRIVER = (DIRECTORY / 'final-main-native.py').read_bytes()
PREPARATION = (DIRECTORY / 'final-main-prepare.py').read_bytes()


def extracted():
    tree=ast.parse(SOURCE)
    selected=[copy.deepcopy(node) for node in tree.body if isinstance(node,(ast.FunctionDef,ast.Assign))]
    namespace=dict(__builtins__=__builtins__,hashlib=hashlib,re=re)
    exec(compile(ast.Module(body=selected,type_ignores=[]),'<actual-data-only-local-settlement>','exec'),namespace)
    return namespace


class Fixture:
    """Source-shaped hypothetical records; no process/FD/inventory exists."""
    def __init__(self):
        self.api=extracted();self.calls=[]
        self.paths=dict(control='/algorithm-only/work/control',before='/algorithm-only/work/before',
                        after='/algorithm-only/work/product',evidence='/algorithm-only/evidence')
        self.uid=1000
        self.instance=dict(pid=700,group=777,stamp='101')  # Supervisor need not be a group leader.
        revision=lambda digit:dict(sha=digit*40,tree=str(int(digit)+1)*40)
        self.source=dict(control=revision('1'),before=revision('3'),product=revision('5'),run='101',attempt='1',
            logical_job='hypothetical_integration',numeric_job_id=None,supervisor_sha256=self.api['SUPERVISOR_SHA'],
            driver_sha256=self.api['DRIVER_SHA'],preparation_module_sha256=self.api['PREPARATION_SHA'],prepared_sha256='a'*64)
        self.private='/tmp/h2018.x.algorithm'
        self.captures={};capture_rows=[]
        for index in range(1,13):
            if index<=9:
                operation=(index-1)%3
                text=self.source[('control','before','product')[(index-1)//3]][('sha','tree')[operation]] if operation<2 else ''
                stdout=(text+'\n').encode() if text else b''
            else:stdout=b'hypothetical owned output\n'
            for stream,raw in [('stdout',stdout),('stderr',b'')]:
                name='supervisor/supervisor-root-'+str(index)+'.'+stream+'.log'
                self.captures[name]=raw
                capture_rows.append(dict(path=self.paths['evidence']+'/'+name,closed=True,error=None,bytes=len(raw)))
        kinds=['capture-'+stream for unused in range(9) for stream in ('stdout','stderr')]
        kinds+=['directory-native','directory-private','private-auth','ready-read','ready-parent-write','capture-stdout',
                'capture-stderr','private-auth','ready-read','ready-parent-write','capture-stdout','capture-stderr','capture-stdout','capture-stderr']
        fds=[]
        for index,kind in enumerate(kinds,1):
            # Deliberate numeric FD reuse across terminal metadata originals.
            number=10+(index-1)%2 if index<=18 else 40+index
            fds.append(dict(id='supervisor-fd-'+str(index),fd=number,kind=kind,closed=True,registered=False,
                            error=None,eof=kind.startswith('capture-') or kind=='ready-read',bytes=0))
        self.ready={'Xvfb':b'1\n','bus':('unix:path='+self.private+'/bus,guid='+'0'*32+'\n').encode()}
        fds[21]['bytes']=len(self.ready['Xvfb']);fds[26]['bytes']=len(self.ready['bus'])
        roots=[]
        for index in range(1,13):
            role='metadata' if index<=9 else ('Xvfb','bus','driver')[index-10]
            # Distinct generation stamps permit a reused metadata numeric PID.
            pid=1001 if index<=2 else 1000+index
            identity=dict(pid=pid,group=pid,stamp=str(200+index))
            code=-15 if role in ('Xvfb','bus') else 0
            work_wait=dict(start_ns=index*100+10,finish_ns=index*100+20,exit_code=code,error=None)
            finish_order=0 if role=='driver' else (1 if role=='bus' else (11 if role=='Xvfb' else index+1))
            finish_wait=dict(start_ns=3000+finish_order*100,finish_ns=3020+finish_order*100,exit_code=code,error=None)
            waits=[copy.deepcopy(work_wait),copy.deepcopy(finish_wait)] if role in ('metadata','driver') else [copy.deepcopy(finish_wait)]
            signals=[dict(signal=15,returned=True,outcome='returned',error=None)] if code==-15 else []
            row=dict(id='supervisor-root-'+str(index),role=role,argv=[],cwd=None,attempted_ns=index*100,
                pid=pid,identity=identity,returned=True,published=True,error=None,waits=waits,
                terminal=copy.deepcopy(finish_wait),signals=signals,forced=False,streams=[])
            if role=='metadata':
                checkout=self.paths[('control','before','after')[(index-1)//3]]
                row['argv']=['git','-C',checkout,*([['rev-parse','HEAD'],['rev-parse','HEAD^{tree}'],['status','--porcelain=v1']][(index-1)%3])]
            elif role=='Xvfb':
                row['argv']=['/usr/bin/Xvfb','-displayfd',str(fds[22]['fd']),'-screen','0','1600x1000x24',
                    '-nolisten','tcp','-dpi','96','-auth',self.private+'/server.Xauthority']
            elif role=='bus':
                row['argv']=['/usr/bin/dbus-daemon','--session','--nofork','--nopidfile','--nosyslog',
                    '--address=unix:path='+self.private+'/bus','--print-address='+str(fds[27]['fd'])]
            else:
                row['argv']=['/usr/bin/python3',self.paths['control']+'/.github/qualification/final-main-native.py',
                             self.paths['control'],self.paths['before'],self.paths['after']]
                row['cwd']=self.paths['control']
            indices=(index*2-1,index*2) if index<=9 else ((24,25),(29,30),(31,32))[index-10]
            for stream,fd_index in zip(('stdout','stderr'),indices):
                fds[fd_index-1]['bytes']=len(self.captures['supervisor/'+row['id']+'.'+stream+'.log'])
                row['streams'].append(copy.deepcopy(fds[fd_index-1]))
            if role in ('Xvfb','bus'):
                raw=self.ready[role]
                row['ready']=dict(bytes=len(raw),sha256=self.api['digest'](raw),eof=True,
                    meaning='Original launch notification only; no authenticated client readiness claim.')
            roots.append(row)
        self.fact=dict(kind='independent-frozen-driver-consumption-v1',local_settled=True,work_outcome_ok=True,errors=[],
            entered_arms=['before','after'],first_driver_diagnostic=None,requested_driver_cancel=None,qualified=False,limit=self.api['DRIVER_LIMIT'])
        self.record=dict(kind='held-scope-supervisor-settlement-v1',qualified=False,retained_evidence_verified_here=False,
            supervisor_instance=copy.deepcopy(self.instance),roots=roots,source=copy.deepcopy(self.source),descriptors=fds,
            captures=capture_rows,directories=[self.directory(1,'native',19,self.paths['evidence']+'/native',False),
                self.directory(2,'private',20,self.private,True)],spawn_fenced=True,subreaper=True,
            kernel=dict(waits=[dict(pid=50000,raw_status=0,exit_code=0)],echild=True,errors=[]),driver_ledger=copy.deepcopy(self.fact),
            first_diagnostic=None,cancel_signal=None,errors=[],outside_scope=copy.deepcopy(self.api['OUTSIDE']))
        self.expected_driver=dict(product=self.source['product'],before=self.source['before'],control_sha=self.source['control']['sha'],
            control_tree=self.source['control']['tree'],driver_sha256=self.source['driver_sha256'],fixture_sha256='b'*64,
            preparation_sha256=self.source['prepared_sha256'],preparation_module_sha256=self.source['preparation_module_sha256'],
            late_environment_sha256={},workflow_sha256='c'*64,run=self.source['run'],attempt=self.source['attempt'],
            logical_job=self.source['logical_job'],numeric_job_id=None,numeric_job_note='No actual supported numeric job supplier is implemented.',plan_sha256='d'*64)
        # These stub inputs do not claim to pass the actual driver validator.
        self.inputs=dict(ledger={'hypothetical':'only callback routing is tested'},source={'hypothetical':'native source'},arm_cleanups={},mounts={})
        self.callback_sha=self.api['SUPERVISOR_SHA']
        self.callback_result=copy.deepcopy(self.fact);self.callback_error=None

    def directory(self,index,kind,fd_index,path,removed):
        return dict(id='directory-'+str(index),kind=kind,attempted=True,created=True,path=path,
            identity=dict(device=4,inode=100+index,uid=self.uid,mode=0o700,directory=True),fd_id='supervisor-fd-'+str(fd_index),
            validated=True,removed=removed,error=None)

    def check_driver(self,*args):
        self.calls.append(args)
        if self.callback_error is not None:raise self.callback_error
        return copy.deepcopy(self.callback_result)

    def check(self):
        return self.api['check_local'](self.record,self.source,self.paths,self.uid,self.instance,self.captures,
            self.inputs,self.expected_driver,self.check_driver,self.callback_sha)

    def code(self,index,value):
        row=self.record['roots'][index-1]
        for waited in row['waits']:waited['exit_code']=value
        row['terminal']['exit_code']=value

    def stream(self,index,stream,field,value):
        root=self.record['roots'][index-1];which=0 if stream=='stdout' else 1
        row=root['streams'][which];row[field]=value
        descriptor=next(item for item in self.record['descriptors'] if item['id']==row['id'])
        descriptor[field]=value


    def fd(self,index,value):
        row=self.record['descriptors'][index-1];row['fd']=value
        for root in self.record['roots']:
            for stream in root['streams']:
                if stream['id']==row['id']:stream['fd']=value
        # Keep source-owned inherited-ready argv consistent, so a co-live alias
        # is refused for ownership rather than a stale copied argument.
        if index==23:self.record['roots'][9]['argv'][2]=str(value)
        if index==28:self.record['roots'][10]['argv'][-1]='--print-address='+str(value)


class LocalSettlementTests(unittest.TestCase):
    def assert_hold(self,result):
        self.assertIs(result['qualified'],False);self.assertIs(result['can_run_broad'],False)
        self.assertIs(result['retained_evidence_verified_here'],False);self.assertEqual(result['admission'],'HOLD')
        self.assertTrue(result['unknown'])

    def assert_negative(self,fixture,reason):
        result=fixture.check();self.assert_hold(result)
        self.assertIs(result['internal_consistency'],False)
        self.assertTrue(any(reason in value for value in result['diagnostics']),result)
        return result

    def test_normal_data_joins_compute_capture_hashes_but_never_admit_work(self):
        fixture=Fixture();result=fixture.check()
        self.assertIs(result['internal_consistency'],True,result);self.assert_hold(result)
        self.assertEqual(len(result['capture_digests']),24)
        empty='supervisor/supervisor-root-1.stderr.log'
        self.assertEqual(result['capture_digests'][empty],dict(sha256=hashlib.sha256(b'').hexdigest(),bytes=0))
        self.assertNotEqual(fixture.instance['pid'],fixture.instance['group'])
        self.assertEqual(fixture.record['roots'][0]['pid'],fixture.record['roots'][1]['pid'])
        self.assertEqual(fixture.record['descriptors'][0]['fd'],fixture.record['descriptors'][2]['fd'])
        self.assertEqual(len(fixture.calls),1)
        args=fixture.calls[0]
        self.assertIs(args[0],fixture.inputs['ledger']);self.assertIs(args[1],fixture.inputs['source'])
        self.assertIs(args[2],fixture.expected_driver)
        self.assertEqual(args[3],fixture.record['roots'][-1]['identity'])
        self.assertEqual(args[4],fixture.record['roots'][-1]['terminal'])
        self.assertEqual(args[8],fixture.instance['pid'])
        self.assertTrue(all(value==dict(eof=True,closed=True,error=None,capture_closed=True) for value in args[5]))

    def test_source_and_original_instance_require_independent_context(self):
        cases=[('source-run',lambda f:f.record['source'].__setitem__('run','102'),'independent-source-join'),
            ('source-tree',lambda f:f.record['source']['control'].__setitem__('tree','f'*40),'independent-source-join'),
            ('driver-hash',lambda f:f.record['source'].__setitem__('driver_sha256','f'*64),'frozen-source-hashes'),
            ('prepared',lambda f:f.record['source'].__setitem__('prepared_sha256','e'*64),'independent-source-join'),
            ('numeric-job',lambda f:f.record['source'].__setitem__('numeric_job_id',9),'source-job'),
            ('instance',lambda f:f.record['supervisor_instance'].__setitem__('stamp','102'),'supervisor-original-generation'),
            ('callback-source',lambda f:setattr(f,'callback_sha','0'*64),'driver-check-source-bound-callback'),
            ('driver-context',lambda f:f.expected_driver.__setitem__('attempt','2'),'driver-supervisor-source-join')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency']);fixture.calls.clear()
                mutate(fixture);self.assert_negative(fixture,reason);self.assertEqual(fixture.calls,[])

    def test_metadata_membership_argv_order_cleanliness_and_terminal_types(self):
        cases=[('argv',lambda f:f.record['roots'][3]['argv'].__setitem__(2,f.paths['after']),'root-exact-argv-cwd'),
            ('id',lambda f:f.record['roots'][1].__setitem__('id','supervisor-root-1'),'root-creation'),
            ('role',lambda f:f.record['roots'][9].__setitem__('role','driver'),'root-creation'),
            ('partial',lambda f:f.record['roots'].pop(),'normal-root-membership'),
            ('unreturned',lambda f:f.record['roots'][0].__setitem__('returned',False),'root-creation'),
            ('bool-wait',lambda f:f.code(1,False),'wait-types'),
            ('float-pid',lambda f:f.record['roots'][0].__setitem__('pid',1001.0),'root-original-PID'),
            ('terminal-copy',lambda f:f.record['roots'][0]['terminal'].__setitem__('exit_code',1),'root-terminal-last-wait')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                mutate(fixture);self.assert_negative(fixture,reason)
        fixture=Fixture();name='supervisor/supervisor-root-3.stdout.log';raw=b' M dirty-owned-source\n'
        fixture.captures[name]=raw;fixture.record['captures'][4]['bytes']=len(raw);fixture.stream(3,'stdout','bytes',len(raw))
        self.assert_negative(fixture,'metadata-output-cleanliness')

    def test_actual_exit_and_returned_signal_policy_is_not_self_reported_forced(self):
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])  # X/bus returned TERM with actual -15.
        cases=[('actual-kill',lambda f:f.code(10,-9),'root-terminal-policy'),
            ('metadata-nonzero',lambda f:f.code(1,1),'root-terminal-policy'),
            ('driver-term',lambda f:f.code(12,-15),'root-terminal-policy'),
            ('attempt-only',lambda f:f.record['roots'][9]['signals'][0].update(returned=False,outcome='attempted'),'signal-send-unresolved'),
            ('ESRCH',lambda f:f.record['roots'][9]['signals'][0].update(returned=False,outcome='ESRCH',error='algorithm-only ESRCH'),'signal-send-unresolved'),
            ('forced',lambda f:f.record['roots'][9].__setitem__('forced',True),'forced-root')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                mutate(fixture);self.assert_negative(fixture,reason)

    def test_captures_are_fixed_bytes_and_reciprocal_closed_streams(self):
        def wrong_name(f):
            value=f.captures.pop('supervisor/supervisor-root-12.stderr.log')
            f.captures['supervisor/arbitrary-row-name.log']=value
        cases=[('name',wrong_name,'fixed-capture-membership'),
            ('content',lambda f:f.captures.__setitem__('supervisor/supervisor-root-12.stdout.log',b'changed'),'capture-path-writer-bytes'),
            ('row-path',lambda f:f.record['captures'][0].__setitem__('path','/outside/log'),'capture-path-writer-bytes'),
            ('close',lambda f:f.record['captures'][23].__setitem__('closed',False),'capture-path-writer-bytes'),
            ('EOF',lambda f:f.stream(12,'stdout','eof',False),'capture-EOF'),
            ('reciprocal',lambda f:f.record['roots'][11]['streams'][0].__setitem__('fd',999),'root-descriptor-reciprocal'),
            ('same-count-extra',lambda f:f.record['roots'][11]['streams'][0].__setitem__('id','supervisor-fd-29'),'root-descriptor-reciprocal')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                mutate(fixture);self.assert_negative(fixture,reason)
        fixture=Fixture();name='supervisor/supervisor-root-12.stdout.log';new=b'new actual supplied RAM bytes'
        fixture.captures[name]=new;fixture.record['captures'][22]['bytes']=len(new);fixture.stream(12,'stdout','bytes',len(new))
        result=fixture.check();self.assertTrue(result['internal_consistency'],result)
        self.assertEqual(result['capture_digests'][name],dict(sha256=hashlib.sha256(new).hexdigest(),bytes=len(new)))
        self.assert_hold(result)  # No external original-content claim is invented.

    def test_directory_custody_types_and_native_retained_private_removed_distinction(self):
        cases=[('private-live',lambda f:f.record['directories'][1].__setitem__('removed',False),'directory-retention-removal'),
            ('native-removed',lambda f:f.record['directories'][0].__setitem__('removed',True),'directory-retention-removal'),
            ('attempt-lost',lambda f:f.record['directories'].pop(),'directory-membership'),
            ('acquisition',lambda f:f.record['directories'][0].__setitem__('validated',False),'directory-acquisition-FD'),
            ('fd-cross',lambda f:f.record['directories'][0].__setitem__('fd_id','supervisor-fd-20'),'directory-acquisition-FD'),
            ('inode-bool',lambda f:f.record['directories'][0]['identity'].__setitem__('inode',True),'directory-identity-types'),
            ('mode-float',lambda f:f.record['directories'][0]['identity'].__setitem__('mode',448.0),'directory-identity-types'),
            ('wrong-native',lambda f:f.record['directories'][0].__setitem__('path','/algorithm-only/other/native'),'native-fixed-directory'),
            ('fd-type',lambda f:f.record['descriptors'][18].__setitem__('fd',True),'descriptor-types')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                mutate(fixture);self.assert_negative(fixture,reason)

    def test_kernel_requires_scoped_actual_status_shapes_and_final_ECHILD(self):
        cases=[('no-echild',lambda f:f.record['kernel'].__setitem__('echild',False),'kernel-ECHILD-UNKNOWN'),
            ('echild-int',lambda f:f.record['kernel'].__setitem__('echild',1),'kernel-types'),
            ('zero-poll',lambda f:f.record['kernel']['waits'][0].__setitem__('pid',0),'kernel-wait-types'),
            ('decode',lambda f:f.record['kernel']['waits'][0].__setitem__('exit_code',1),'kernel-original-status'),
            ('bool-code',lambda f:f.record['kernel']['waits'][0].__setitem__('exit_code',False),'kernel-wait-types'),
            ('kill',lambda f:f.record['kernel']['waits'][0].update(raw_status=9,exit_code=-9),'kernel-original-status'),
            ('root-overlap',lambda f:f.record['kernel']['waits'][0].__setitem__('pid',f.record['roots'][0]['pid']),'kernel-original-status'),
            ('error',lambda f:f.record['kernel']['errors'].append('retained unknown writer'),'kernel-ECHILD-UNKNOWN')]
        for name,mutate,reason in cases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                mutate(fixture);self.assert_negative(fixture,reason)

    def test_copied_driver_verdict_never_replaces_source_bound_data_callback(self):
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency']);fixture.calls.clear()
        fixture.callback_result['local_settled']=False;fixture.callback_result['errors']=['actual callback negative']
        self.assert_negative(fixture,'copied-driver-fact-mismatch');self.assertEqual(len(fixture.calls),1)
        fixture.record['driver_ledger']=copy.deepcopy(fixture.callback_result)
        self.assert_negative(fixture,'driver-work-unsettled')
        fixture=Fixture();fixture.record['driver_ledger']['local_settled']=1
        self.assert_negative(fixture,'driver-fact-types')
        fixture=Fixture();fixture.callback_error=ValueError('exact hypothetical driver refusal')
        self.assert_negative(fixture,'exact hypothetical driver refusal')

    def test_cancel_first_diagnostics_partial_and_malformed_records_stay_negative(self):
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
        fixture.record.update(first_diagnostic="RuntimeError('original work')",cancel_signal=15,errors=['owned role UNKNOWN'])
        result=self.assert_negative(fixture,'supervisor-work-negative')
        self.assertEqual(result['recorded_first_diagnostic'],"RuntimeError('original work')")
        self.assertEqual(result['recorded_cancel_signal'],15);self.assertEqual(result['recorded_errors'],['owned role UNKNOWN'])
        for field,value,reason in [('spawn_fenced',1,'supervisor-fence'),('cancel_signal',True,'supervisor-cancel-type'),
                                  ('first_diagnostic',{},'supervisor-diagnostic-type'),('source',None,'source-shape')]:
            with self.subTest(field=field):
                fixture=Fixture();fixture.record[field]=value;self.assert_negative(fixture,reason)
        fixture=Fixture();fixture.record['extra_witness']='cannot release HOLD'
        self.assert_negative(fixture,'supervisor-shape')

    def test_normal_original_wait_histories_require_both_work_and_finish(self):
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
        self.assertEqual([len(row['waits']) for row in fixture.record['roots']],[2]*9+[1,1,2])
        for index in [*range(1,10),12]:
            for removed in (0,1):
                with self.subTest(root=index,missing=removed):
                    fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                    row=fixture.record['roots'][index-1];row['waits'].pop(removed)
                    row['terminal']=copy.deepcopy(row['waits'][-1])
                    self.assert_negative(fixture,'normal-root-wait-count')
        for index in (1,10,11,12):
            with self.subTest(root=index,extra=True):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                row=fixture.record['roots'][index-1];row['waits'].insert(0,copy.deepcopy(row['waits'][0]))
                self.assert_negative(fixture,'normal-root-wait-count')
        for index in (1,12):
            with self.subTest(root=index,prior_error=True):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                row=fixture.record['roots'][index-1]
                row['waits'][0].update(exit_code=None,error='retained original wait failure')
                result=self.assert_negative(fixture,'normal-root-wait-error')
                self.assertEqual(result['recorded_errors'],[])
                self.assertTrue(any('retained original wait failure' in value for value in result['diagnostics']))
                self.assertIsNone(result['recorded_first_diagnostic'])  # The history contradicts this copied clean verdict.
            with self.subTest(root=index,prior_outcome=True):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                fixture.record['roots'][index-1]['waits'][0]['exit_code']=1
                self.assert_negative(fixture,'original-wait-outcome-contradiction')

    def test_co_live_original_FDs_are_distinct_without_global_lifetime_uniqueness(self):
        # Guaranteed source acquisition phases, not an inference that prior
        # service output remained open after ready()/pump() observed its EOF.
        aliases=[('directories',20,19),('metadata-pair',2,1),('server-secret',21,19),
            ('X-ready-pair',23,22),('X-ready-output',24,22),('X-directory-output',24,19),
            ('client-secret',26,20),('bus-ready-pair',28,27),('bus-ready-output',29,27),
            ('bus-directory-output',29,20),('driver-pair',32,31),('driver-directory-output',31,19)]
        for name,target,original in aliases:
            with self.subTest(name=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                fixture.fd(target,fixture.record['descriptors'][original-1]['fd'])
                self.assert_negative(fixture,'co-live-descriptor-alias')
        # Every reuse has otherwise consistent stream copies/ready argv. This
        # is a possible history, not evidence of the absent EOF-close timing.
        for name,target,closed in [('closed-server-auth',22,21),
                ('closed-X-ready',26,22),('prior-X-output',29,24),('prior-bus-output',31,29)]:
            with self.subTest(reuse=name):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                fixture.fd(target,fixture.record['descriptors'][closed-1]['fd'])
                result=fixture.check();self.assertTrue(result['internal_consistency'],result);self.assert_hold(result)
                self.assertTrue(any('Inter-service capture close timing' in value for value in result['unknown']))

    def test_regressing_wall_stamps_are_scalar_observations_not_clock_order(self):
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
        for index,row in enumerate(fixture.record['roots'],1):
            row['attempted_ns']=20000-index*100
            for position,waited in enumerate(row['waits']):
                waited['start_ns']=10000-index-position*1000
                waited['finish_ns']=100-index-position*50
            row['terminal']=copy.deepcopy(row['waits'][-1])
        # Covers within-wait reversal, later wait before earlier finish,
        # waits before creation stamp and regressing cross-root wall stamps.
        fixture.record['roots'][0]['waits'][1]['start_ns']=1
        fixture.record['roots'][0]['terminal']=copy.deepcopy(fixture.record['roots'][0]['waits'][-1])
        result=fixture.check();self.assertTrue(result['internal_consistency'],result);self.assert_hold(result)
        for field,value,reason in [('attempted_ns',False,'root-creation'),('start_ns',1.5,'wait-types'),('finish_ns',0,'wait-types')]:
            with self.subTest(field=field):
                fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
                if field=='attempted_ns':fixture.record['roots'][0][field]=value
                else:fixture.record['roots'][0]['waits'][0][field]=value
                self.assert_negative(fixture,reason)
        fixture=Fixture();self.assertTrue(fixture.check()['internal_consistency'])
        fixture.record['roots'][0],fixture.record['roots'][1]=fixture.record['roots'][1],fixture.record['roots'][0]
        self.assert_negative(fixture,'root-creation')  # Actual list/role order remains source-owned.

    def test_source_only_reader_has_no_resource_boundary_or_HOLD_override(self):
        namespace=extracted()
        self.assertEqual(namespace['SUPERVISOR_SHA'],hashlib.sha256(SUPERVISOR).hexdigest())
        self.assertEqual(namespace['DRIVER_SHA'],hashlib.sha256(DRIVER).hexdigest())
        self.assertEqual(namespace['PREPARATION_SHA'],hashlib.sha256(PREPARATION).hexdigest())
        tree=ast.parse(SOURCE)
        imports=[name.name for node in tree.body if isinstance(node,ast.Import) for name in node.names]
        self.assertEqual(imports,['hashlib','re'])
        self.assertFalse(any(isinstance(node,(ast.ImportFrom,ast.ClassDef)) for node in tree.body))
        calls={ast.unparse(node.func) for node in ast.walk(tree) if isinstance(node,ast.Call)}
        for forbidden in ('open','exec','eval','compile','__import__','subprocess.Popen','os.waitpid','Path'):
            self.assertNotIn(forbidden,calls)
        check=next(node for node in tree.body if isinstance(node,ast.FunctionDef) and node.name=='check_local')
        self.assertEqual([arg.arg for arg in check.args.args],['record','expected_source','paths','owner_uid','supervisor_instance',
            'captures','driver_inputs','expected_driver','driver_check','driver_check_source_sha256'])
        self.assertIsNone(check.args.kwarg);self.assertIsNone(check.args.vararg)
        fixture=Fixture();self.assert_hold(fixture.check())
        fixture.callback_result['qualified']=True;self.assert_negative(fixture,'driver-fact-types')


if __name__ == '__main__':
    unittest.main()
