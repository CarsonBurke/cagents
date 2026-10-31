"""Algorithm-only source mocks; never a desktop/process/native evidence packet.

Only fixed source reads and stdlib data are real. System, Popen, signals,
descriptors, clocks, files, selectors and waits are inert. No live module import.
"""
import ast
import copy
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess
import sys
from types import SimpleNamespace
import unittest

SOURCE=Path(__file__).with_name('final-main-supervisor.py').read_text()


class InertPath:
    def __init__(self, value):self.value=str(value)
    def __str__(self):return self.value
    def __truediv__(self, value):return InertPath(self.value+'/'+str(value))
    @property
    def parent(self):return InertPath(self.value.rsplit('/',1)[0])
    def resolve(self, **kwargs):return self
    def __getattr__(self, name):raise AssertionError('Ambient path operation: '+name)


def definitions():
    namespace=dict(__builtins__=__builtins__,contextmanager=contextmanager,hashlib=hashlib,json=json,
        re=re,struct=struct,sys=sys,Path=InertPath,signal=SimpleNamespace(SIGINT=2,SIGTERM=15,SIGKILL=9),
        subprocess=SimpleNamespace(DEVNULL=-3,PIPE=-1,TimeoutExpired=subprocess.TimeoutExpired))
    parsed=ast.parse(SOURCE)
    nodes=[copy.deepcopy(n) for n in parsed.body if isinstance(n,(ast.FunctionDef,ast.ClassDef,ast.Assign))]
    exec(compile(ast.Module(body=nodes,type_ignores=[]),'<actual-supervisor-definitions>','exec'),namespace)
    return namespace


class Memory:
    def __init__(self, system, path):self.system=system;self.path=path;self.data=bytearray();self.closed=False
    def write(self, data):
        self.system.on_write(self);self.data.extend(data)
    def close(self):
        self.system.events_seen.append(('capture-close',self.path));self.system.on_close(self);self.closed=True


class Pipe:
    def __init__(self, system, fd):self.system=system;self.fd=fd;self.closed=False
    def fileno(self):return self.fd
    def close(self):self.system.close_fd(self.fd);self.closed=True


class Handle:
    def __init__(self, system, pid, stdout, stderr):
        self.system=system;self.pid=pid;self.stdout=stdout;self.stderr=stderr;self.code=None;self.wait_error=None
    def poll(self):return self.code
    def wait(self, timeout):
        self.system.events_seen.append(('wait',self.pid,timeout))
        assert timeout==0,'Never block a wait on undrained PIPEs'
        if self.wait_error is not None:raise self.wait_error
        if self.code is None:raise subprocess.TimeoutExpired('algorithm-only-original',0)
        return self.code


class InertSystem:
    pid=900
    def __init__(self):
        self.instance=dict(pid=900,group=900,stamp='9001')
        self.t=0;self.ns=100;self.nextfd=20;self.queues={};self.registered=set();self.closed=set()
        self.handles=[];self.files={};self.events_seen=[];self.owner=None;self.pipe_count=0
        self.kernel=[];self.on_create=lambda p:None;self.on_identity=lambda p:None
        self.on_events=lambda:None;self.on_write=lambda stream:None;self.on_close=lambda stream:None
        self.on_register=lambda fd:None;self.on_signal=lambda p,s:None
    def now(self):self.t+=.1;return self.t
    def stamp(self):self.ns+=1;return self.ns
    def descriptor(self, chunks=()):
        fd=self.nextfd;self.nextfd+=1;self.queues[fd]=list(chunks);return fd
    def pipe(self):
        self.pipe_count+=1
        raw=b'77\n' if self.pipe_count==1 else b'unix:path=/tmp/h2018.x.algorithm/bus,guid='+b'a'*32+b'\n'
        return self.descriptor([raw,b'']),self.descriptor()
    def nonblocking(self, fd):self.events_seen.append(('nonblocking',fd))
    def register(self, fd):self.on_register(fd);self.registered.add(fd)
    def unregister(self, fd):self.registered.remove(fd)
    def events(self, timeout):
        assert 0<=timeout<=.1;self.t+=.1;self.on_events()
        return [fd for fd in sorted(self.registered) if self.queues[fd]]
    def read(self, fd, count):
        self.events_seen.append(('read',fd));value=self.queues[fd].pop(0)
        if isinstance(value,BaseException):raise value
        if callable(value):value=value()
        if len(value)>count:self.queues[fd].insert(0,value[count:]);return value[:count]
        return value
    def close_fd(self, fd):self.events_seen.append(('fd-close',fd));self.closed.add(fd)
    def open_capture(self, path):
        value=Memory(self,path);self.files[path]=value;return value
    def popen(self, argv, **options):
        assert self.owner.control.depth>0 and options['start_new_session'] and options['close_fds']
        assert 'preexec_fn' not in options and options['stdout']==options['stderr']==-1
        stdout=Pipe(self,self.descriptor([b'out',b'']));stderr=Pipe(self,self.descriptor([b'err',b'']))
        value=Handle(self,100+len(self.handles),stdout,stderr);self.handles.append(value)
        self.events_seen.append(('create',value.pid,list(argv)));self.on_create(value)
        if 'final-main-native.py' in str(argv):value.code=0
        return value
    def original_identity(self, process):
        assert any(p is process for p,row in self.owner.handles),'Original retained before identity'
        self.on_identity(process);return dict(pid=process.pid,group=process.pid,stamp=str(process.pid)+'1')
    def signal_group(self, group, signum):
        p=next(p for p in self.handles if p.pid==group)
        self.events_seen.append(('signal',p.pid,signum));self.on_signal(p,signum)
        if p.code is None:
            role=next(row['role'] for actual,row in self.owner.handles if actual is p)
            p.code=0 if role=='driver' else -signum
    def waitpid(self):
        self.events_seen.append(('kernel-wait',))
        if not self.kernel:raise ChildProcessError()
        value=self.kernel.pop(0)
        if isinstance(value,BaseException):raise value
        return value
    def close_selector(self):self.events_seen.append(('selector-close',))
    def private(self, evidence):self.events_seen.append(('private',str(evidence)));return '/tmp/h2018.x.algorithm'
    def random(self):return b'algorithm-cookie'  # Exactly16 inert test bytes, not a generated secret.
    def secret(self, path):
        fd=self.descriptor();self.events_seen.append(('secret-open',path,fd));return fd
    def write_fd(self, fd, raw):self.events_seen.append(('secret-write',fd,len(raw)));return len(raw)
    def fsync(self, fd):self.events_seen.append(('fsync',fd))
    def remove_private(self, path):self.events_seen.append(('private-remove',path))


def harness():
    n=definitions();io=InertSystem();control=n['Control']();owner=n['Supervisor'](io,control,'/algorithm/evidence/supervisor')
    io.owner=owner;owner.subreaper=True;owner.source={'algorithm_only':'not a source/runtime evidence binding'}
    return n,io,control,owner


def driver_data(n, role='command'):
    """A deliberately work-negative, thin ledger DTO; no native8 packet."""
    original=dict(pid=50,group=50,stamp='111')
    expected=dict(product={'sha':'a'*40,'tree':'b'*40},before={'sha':'c'*40,'tree':'d'*40},
        control_sha='e'*40,control_tree='f'*40,driver_sha256=n['DRIVER_SHA'],fixture_sha256='1'*64,
        preparation_sha256='2'*64,preparation_module_sha256=n['PREPARATION_SHA'],late_environment_sha256={},
        workflow_sha256='3'*64,run='algorithm-run',attempt='1',logical_job='algorithm-job',numeric_job_id=None,
        numeric_job_note='No actual supported numeric job supplier is implemented.',plan_sha256='4'*64)
    source=dict(expected,driver_instance=dict(pid=50,observation=dict(state='R',parent=900,group=50,stamp='111')))
    wait=dict(method='wait',start_ns=3,finish_ns=2,exit_code=0,error=None)  # Wall-clock reversal remains observed data.
    history=dict(method='wait',start_ns=1,finish_ns=2,exit_code=None,error='algorithm-only prior timeout')
    scope='global-driver' if role=='command' else 'before'
    root=dict(id='creation-1',scope=scope,role=role,argv=['algorithm-only'],cwd=None,attempted_ns=1,pid=77,
        identity=dict(pid=77,group=77,stamp='222'),pidfd=None,pidfd_note='Not acquired; authority is the original retained Popen.',
        expected_mount=None,returned=True,published=True,error=None,writers=[],waits=[history,wait],terminal=copy.deepcopy(wait),signals=[],forced=False,
        exit_policy=dict(kind='declared-command-exits' if role=='command' else 'ordinary-native-zero-or-issued-TERM',allowed_exit_codes=[0]))
    value=dict(kind='driver-role-writer-ledger-v1',scope='This original driver only; pre-exit statement.',binding=source,
        entered_arms=[] if scope=='global-driver' else ['before'],creations=[root],writers=[],callbacks=[],
        kernel=dict(scope='Original driver subreaper kernel children only',waits=[],echild=True,errors=[]),
        no_future_spawns=True,errors=[],first_terminal_diagnostic='Algorithm-only negative work; not native evidence.',requested_cancel=None,
        outside_scope=n['OUTSIDE'],final_publication=n['FINAL'])
    for index,name in enumerate(('stdout','stderr') if role=='command' else ('log',),1):
        writer=dict(id='writer-'+str(index),scope=scope,path='<creation-1.'+name+'>' if role=='command' else '/algorithm/root.log',
            kind='child-PIPE' if role=='command' else 'parent-log',acquired=True,closed=True,error=None,inherited_by=['creation-1'],callback=None)
        root['writers'].append(writer['id']);value['writers'].append(writer)
    terminal=dict(exit_code=1,error=None);captured=[dict(eof=True,closed=True,error=None,capture_closed=True) for unused in range(2)]
    return value,source,expected,original,terminal,captured,{},dict(before='/algorithm/before/runtime/doc',after='/algorithm/after/runtime/doc'),900


class SupervisorTests(unittest.TestCase):
    def test_null_admission_and_inherited_authority_refuse_before_any_factory(self):
        n=definitions();calls=[]
        def admit(roles,pinned):
            calls.append('admission')
            if any(v is None for v in roles.values()):raise ValueError('NULL role')
        module=SimpleNamespace(PINNED_ROLES={'before':None,'after':None},admit_references=admit)
        plan={'native_preparation':{'roles':module.PINNED_ROLES}}
        with self.assertRaisesRegex(ValueError,'NULL'):n['admit_preparation'](plan,module,lambda:calls.append('verify/resource'))
        self.assertEqual(calls,['admission'])
        n['require_user']({'RUNNER_ENVIRONMENT':'github-hosted'})
        for value in ('','inherited'):
            with self.assertRaisesRegex(ValueError,'inherited authority'):n['require_user']({'RUNNER_ENVIRONMENT':'github-hosted','CAGENTS_AGENT_AUTHORITY':value})
        with self.assertRaisesRegex(ValueError,'inherited desktop'):n['require_user']({'RUNNER_ENVIRONMENT':'github-hosted','DISPLAY':':1'})
        n,io,c,owner=harness()
        with self.assertRaisesRegex(ValueError,'prepared-before-desktop'):owner.launch({},InertPath('/control'),'/before','/after',None)
        self.assertEqual(io.events_seen,[])

    def test_binary_authority_and_private_ready_grammar(self):
        n=definitions();cookie=b'algorithm-cookie'
        raw=n['authority_bytes'](cookie,b'77');position=0
        family=struct.unpack('!H',raw[:2])[0];position=2;fields=[]
        for unused in range(4):
            count=struct.unpack('!H',raw[position:position+2])[0];position+=2
            fields.append(raw[position:position+count]);position+=count
        self.assertEqual((family,fields,position),(65535,[b'',b'77',b'MIT-MAGIC-COOKIE-1',cookie],len(raw)))
        self.assertEqual(n['ready_value']('Xvfb',b'77\n','/private'),':77')
        address=b'unix:path=/private/bus,guid='+b'a'*32+b'\n'
        self.assertEqual(n['ready_value']('bus',address,'/private'),address[:-1].decode())
        for raw in (b'',b'77',b'77\nextra',b'77\n\n',b'-1\n',b'65536\n'):
            with self.assertRaises(ValueError):n['ready_value']('Xvfb',raw,'/private')
        with self.assertRaisesRegex(ValueError,'private-address'):n['ready_value']('bus',address,'/other')

    def test_original_creation_cancel_or_registration_failure_retains_actual_handles_and_streams(self):
        for stage in ('creation','identity','registration'):
            with self.subTest(stage=stage):
                n,io,c,owner=harness();original=RuntimeError('algorithm registration error')
                if stage=='creation':io.on_create=lambda p:c.request(2,None)
                elif stage=='identity':
                    def fail(p):raise original
                    io.on_identity=fail
                else:
                    def fail(fd):raise original
                    io.on_register=fail
                with self.assertRaises(n['SupervisorCancelled'] if stage=='creation' else RuntimeError):owner.start('Xvfb',['algorithm-only'],{})
                p=io.handles[0];self.assertIs(owner.handles[0][0],p)
                self.assertIn(p.stdout,owner.original_streams);self.assertIn(p.stderr,owner.original_streams)
                p.code=0;io.on_register=lambda fd:None
                result=owner.finish()
                self.assertTrue(any(event[:2]==('wait',p.pid) for event in io.events_seen))
                self.assertTrue(p.stdout.closed and p.stderr.closed)
                if stage!='creation':self.assertTrue(result['errors']);self.assertIs(c.failure,original)

    def test_ready_requires_complete_EOF_alive_original_and_unexpired_bound(self):
        for stage in ('valid','empty','partial','extra','oversize','stall','death','late-EOF'):
            with self.subTest(stage=stage):
                n,io,c,owner=harness();read,write=owner.ready_pipe();p,row=owner.start('Xvfb',['algorithm'],{})
                owner.close_fd(write)
                data={'valid':[b'77\n',b''],'empty':[b''],'partial':[b'77',b''],'extra':[b'77\nmore',b''],
                      'oversize':[b'1'*1025,b''],'stall':[],'death':[b'77\n',b'']}.get(stage)
                if stage=='late-EOF':
                    def late():io.t+=31;return b''
                    data=[b'77\n',late]
                io.queues[read['fd']]=data
                if stage=='death':p.code=1
                if stage=='valid':
                    with c.work():self.assertEqual(owner.ready(p,row,read,'/private'),':77')
                else:
                    with self.assertRaises(ValueError) as caught:
                        with c.work():owner.ready(p,row,read,'/private')
                    self.assertIs(c.failure,caught.exception);self.assertNotIn('ready',row)
                p.code=0;result=owner.finish()
                self.assertTrue(all(item['closed'] for item in owner.fds));self.assertEqual(len(io.handles),1)
                if stage=='valid':self.assertTrue(row['ready']['eof'])

    def test_simultaneous_capture_and_driver_first_wait_validation_before_bus_X(self):
        n,io,c,owner=harness();checks=[]
        def consume(p,row):
            self.assertIsNotNone(row['terminal']);self.assertTrue(all(s['eof'] and s['closed'] and s['capture']['closed'] for s in row['streams']))
            self.assertTrue(all(actual.poll() is None for actual,r in owner.handles if r['role'] in ('bus','Xvfb')))
            checks.append('driver-ledger-after-independent-wait-and-EOF')
            return dict(local_settled=True,work_outcome_ok=False,algorithm_only=True)
        owner.check_driver=consume
        p,row,code,env=owner.launch({},InertPath('/control'),'/before','/after',{'algorithm':'prepared'})
        self.assertEqual(code,0);self.assertEqual(env['DISPLAY'],':77')
        self.assertTrue(all(item['bytes']==3 for item in row['streams']))
        result=owner.finish();self.assertEqual(result['errors'],[]);self.assertEqual(len(checks),1)
        waits=[e[1] for e in io.events_seen if e[0]=='wait']
        self.assertLess(waits.index(p.pid),next(i for i,pid in enumerate(waits) if pid!=p.pid))
        self.assertTrue(result['kernel']['echild']);self.assertFalse(result['qualified'])
        self.assertFalse(result['driver_ledger']['work_outcome_ok'])  # Owner mock is never native evidence.

    def test_pipe_caps_first_error_and_repeated_cleanup_cancel_do_not_skip_original_settlement(self):
        n,io,c,owner=harness();p,row=owner.start('Xvfb',['algorithm'],{});original=RuntimeError('capture write error')
        def fail(stream):raise original
        io.on_write=fail
        with self.assertRaises(RuntimeError) as caught:owner.pump(.1)
        self.assertIs(caught.exception,original);self.assertIs(c.failure,original)
        io.on_write=lambda stream:None
        io.on_signal=lambda p,s:(c.request(2,None),c.request(15,None))
        result=owner.finish()
        self.assertIs(c.failure,original);self.assertTrue(result['errors'])
        self.assertTrue(any(e[0]=='wait' for e in io.events_seen));self.assertTrue(all(v.closed for v in io.files.values()))
        n,io,c,owner=harness();p,row=owner.start('Xvfb',['algorithm'],{})
        n['STREAM_BYTES']=2
        with self.assertRaisesRegex(ValueError,'byte-cap'):owner.pump(.1)
        self.assertTrue(owner.finish()['errors'])

    def test_supervisor_kernel_accounts_private_bus_children_and_never_uses_zero_as_closure(self):
        n,io,c,owner=harness();p,row=owner.start('bus',['algorithm'],{});p.code=0
        io.kernel=[(888,0),(889,15),(0,0),ChildProcessError()]
        result=owner.finish();self.assertEqual(result['errors'],[])
        self.assertEqual(result['kernel']['waits'],[dict(pid=888,raw_status=0,exit_code=0),dict(pid=889,raw_status=15,exit_code=-15)])
        events=io.events_seen;self.assertLess(next(i for i,e in enumerate(events) if e[0]=='wait'),next(i for i,e in enumerate(events) if e[0]=='kernel-wait'))
        for stage in ('zero','root-wait','overlap','adopted-KILL'):
            n,io,c,owner=harness();p,row=owner.start('bus',['algorithm'],{});p.code=0
            if stage=='zero':io.waitpid=lambda:(0,0)
            if stage=='root-wait':p.wait_error=RuntimeError('actual original unresolved')
            if stage=='overlap':io.kernel=[(p.pid,0),ChildProcessError()]
            if stage=='adopted-KILL':io.kernel=[(888,9),ChildProcessError()]
            self.assertTrue(owner.finish()['errors'])

    def test_strict_driver_source_generation_independent_wait_and_streams(self):
        n=definitions();args=driver_data(n)
        fact=n['consume_driver'](*args);self.assertTrue(fact['local_settled']);self.assertFalse(fact['work_outcome_ok']);self.assertFalse(fact['qualified'])
        for case in ('source','generation','wait','EOF'):
            args=list(driver_data(n));self.assertTrue(n['consume_driver'](*copy.deepcopy(args))['local_settled'])
            if case=='source':args[1]['control_tree']='0'*40
            if case=='generation':args[3]['stamp']='333'
            if case=='wait':args[4]=None
            if case=='EOF':args[5][0]['eof']=False
            reason={'source':'trusted-source','generation':'original-generation','wait':'original-driver-wait','EOF':'original-driver-streams'}[case]
            with self.assertRaisesRegex(ValueError,reason):n['consume_driver'](*args)
        args=list(driver_data(n));args[4]['exit_code']=-9
        self.assertFalse(n['consume_driver'](*args)['local_settled'])

    def test_strict_driver_signal_policies_reciprocal_members_and_negative_kernel_shape(self):
        n=definitions();args=list(driver_data(n,'ui'));root=args[0]['creations'][0]
        root['waits'][-1]['exit_code']=root['terminal']['exit_code']=-15
        send=dict(signal=15,unix_ns=5,scope='original group',target=77,returned=True,outcome='returned',error=None)
        root['signals']=[send]
        self.assertTrue(n['consume_driver'](*copy.deepcopy(args))['local_settled'])
        send.update(returned=False,outcome='ESRCH',error='algorithm ESRCH')
        self.assertFalse(n['consume_driver'](*args)['local_settled'])
        args=list(driver_data(n));args[0]['kernel']=dict(echild=False,waits=[],errors=['algorithm original kernel exception'])
        self.assertFalse(n['consume_driver'](*args)['local_settled'])
        args=list(driver_data(n));root=args[0]['creations'][0];root['writers']=['writer-missing']
        with self.assertRaisesRegex(ValueError,'reciprocal'):n['consume_driver'](*args)
        args=list(driver_data(n));args[0]['creations'][0]['exit_policy']['allowed_exit_codes']=[0,1]
        with self.assertRaisesRegex(ValueError,'source-policy'):n['consume_driver'](*args)

    def test_source_wiring_fixed_caps_and_no_cookie_or_authority_bypass(self):
        tree=ast.parse(SOURCE);main=next(n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name=='main')
        calls=[n for n in ast.walk(main) if isinstance(n,ast.Call)]
        admission=next(n for n in calls if isinstance(n.func,ast.Attribute) and n.func.attr=='admit_references')
        system=next(n for n in calls if isinstance(n.func,ast.Name) and n.func.id=='System')
        mkdir=next(n for n in calls if isinstance(n.func,ast.Attribute) and n.func.attr=='mkdir')
        self.assertLess(admission.lineno,system.lineno);self.assertLess(admission.lineno,mkdir.lineno)
        verify=next(n for n in calls if isinstance(n.func,ast.Attribute) and n.func.attr=='load_prepared')
        launch=next(n for n in calls if isinstance(n.func,ast.Attribute) and n.func.attr=='launch')
        self.assertLess(verify.lineno,launch.lineno)
        self.assertNotIn('xvfb-run',SOURCE);self.assertNotIn('dbus-run-session',SOURCE);self.assertNotIn("'-ac'",SOURCE)
        self.assertNotIn('SIG_IGN',SOURCE);self.assertNotIn('pthread_sigmask',SOURCE)
        self.assertNotIn('os.environ.pop',SOURCE)
        n=definitions();self.assertEqual(n['READY_BYTES'],1024);self.assertEqual(n['READY_SECONDS'],30)
        self.assertLessEqual(n['DRIVER_SECONDS']+n['CLEANUP_SECONDS'],n['WHOLE_SECONDS'])


if __name__=='__main__':unittest.main()
