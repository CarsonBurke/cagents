"""Finite, standalone owner of this qualifier's Xvfb, private bus and driver.

Not wired into a workflow. NULL preparation pins still refuse before resources.
Readiness is a launch notification, not authenticated client or native success.
All limits below are new proposals, not measured sufficiency or execution grants.
"""
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import time

READY_BYTES = 1024
READY_SECONDS = 30
STREAM_BYTES = 64 * 1024 * 1024
TOTAL_STREAM_BYTES = 256 * 1024 * 1024
DRIVER_SECONDS = 3600
METADATA_SECONDS = 60
DRIVER_TERM_SECONDS = 90
ROOT_TERM_SECONDS = 15
KILL_SECONDS = 10
EOF_SECONDS = 15
KERNEL_SECONDS = 10
CLEANUP_SECONDS = 180
WHOLE_SECONDS = 4200
MAX_ROOTS = 12  # Nine fixed source-metadata commands plus Xvfb/bus/driver.
MAX_FDS = 64
MAX_ADOPTED = 4096
LEDGER_BYTES = 16 * 1024 * 1024
JSON_SECONDS = 5
JSON_NODES = 500000
JSON_DEPTH = 32
JSON_STRING = 65536
DRIVER_SHA = '56e69dc8aed8f7343e3f664e015176c3a97652ba066534d6fdb458942fab12b6'
PREPARATION_SHA = '289d578f7465b00b182a0f18f89b6861b6aaffce34d41d78ad1ff9c73b1236a6'
HEX = re.compile(r'[0-9a-f]{64}')
STAMP = re.compile(r'[1-9][0-9]{0,31}')
ORDINARY = {'accessibility-launcher','accessibility-registry','window-manager','permissions',
            'backend','frontend','fixture','ui','clipboard'}
GLOBALS = {'accessibility-launcher','accessibility-registry','window-manager','command'}
ARMS = {'permissions','documents','backend','frontend','fixture','ui','clipboard','command'}
BUS_NAMES = {'org.freedesktop.portal.Desktop','org.freedesktop.impl.portal.desktop.gtk',
             'org.freedesktop.portal.Documents','org.freedesktop.impl.portal.PermissionStore'}


def need(value, reason):
    if not value: raise ValueError(reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def keys(value, expected, reason):
    need(type(value) is dict and set(value)==set(expected.split()),reason)


def integer(value, minimum=0):
    return type(value) is int and value>=minimum


def identity(value, pid=None):
    keys(value,'pid group stamp','identity-shape')
    need(integer(value['pid'],1) and value['group']==value['pid']
         and isinstance(value['stamp'],str) and STAMP.fullmatch(value['stamp']),'identity-generation')
    if pid is not None: need(value['pid']==pid,'identity-original-PID')


class SupervisorCancelled(RuntimeError):
    pass


class Control:
    def __init__(self):
        self.depth=0;self.signum=None;self.cancel=None;self.failure=None
        self.delivered=False;self.finalizing=False;self.interruptible=False

    def latch(self, error):
        if not isinstance(error,SupervisorCancelled) and self.failure is None:self.failure=error

    def request(self, signum, frame):
        if self.signum is None:
            self.signum=signum;self.cancel=SupervisorCancelled('Supervisor signal '+str(signum))
        if sys.exc_info()[1] is None and self.interruptible:self.checkpoint()

    def checkpoint(self):
        if self.signum is not None and not self.delivered and not self.depth and not self.finalizing:
            self.delivered=True;raise self.cancel

    def admit(self):
        if self.failure is not None:raise self.failure
        self.checkpoint()

    @contextmanager
    def defer(self):
        self.depth+=1;failed=False
        try:yield
        except BaseException as error:
            failed=True;self.latch(error);raise
        finally:
            self.depth-=1
            if not failed and sys.exc_info()[1] is None:self.checkpoint()

    @contextmanager
    def work(self):
        self.admit();previous=self.interruptible;self.interruptible=True
        try:yield
        except BaseException as error:self.latch(error);raise
        finally:self.interruptible=previous


def require_user(env):
    need(env.get('RUNNER_ENVIRONMENT')=='github-hosted' and 'CAGENTS_AGENT_AUTHORITY' not in env,
         'Genuine hosted User required; inherited authority is never removed')
    need(not any(env.get(k) for k in ('DISPLAY','XAUTHORITY','DBUS_SESSION_BUS_ADDRESS',
         'AT_SPI_BUS_ADDRESS','WAYLAND_DISPLAY','WAYLAND_SOCKET')),'Refusing inherited desktop')


def admit_preparation(plan, module, verify):
    module.admit_references(plan['native_preparation']['roles'],module.PINNED_ROLES)
    return verify()  # No cache/path/resource factory is reachable with NULL pins.


def authority_bytes(cookie, number=b''):
    need(type(cookie) is bytes and len(cookie)==16 and type(number) is bytes,'authority-input')
    fields=(b'',number,b'MIT-MAGIC-COOKIE-1',cookie)
    return struct.pack('!H',65535)+b''.join(struct.pack('!H',len(v))+v for v in fields)


def ready_value(role, raw, private):
    need(type(raw) is bytes and 0<len(raw)<=READY_BYTES and raw.count(b'\n')==1 and raw.endswith(b'\n'),'ready-line')
    if role=='Xvfb':
        need(re.fullmatch(rb'(0|[1-9][0-9]{0,4})\n',raw) and int(raw[:-1])<=65535,'ready-display')
        return ':'+raw[:-1].decode('ascii')
    need(role=='bus','ready-role')
    prefix=('unix:path='+str(private)+'/bus,guid=').encode()
    need(raw.startswith(prefix) and re.fullmatch(rb'[0-9a-f]{32}\n',raw[len(prefix):]),'ready-private-address')
    return raw[:-1].decode('ascii')


class System:
    """The only live OS boundary; mock tests replace this entire object."""
    def __init__(self):
        self.pid=os.getpid();self.selector=None
        fields=(Path('/proc')/str(self.pid)/'stat').read_text().rsplit(')',1)[1].split()
        self.instance=dict(pid=self.pid,group=int(fields[2]),stamp=fields[19])
        need(integer(self.instance['group'],1) and STAMP.fullmatch(self.instance['stamp']),'supervisor-instance-UNKNOWN')

    def now(self):return time.monotonic()
    def stamp(self):return time.time_ns()
    def install(self, handler):
        for value in (signal.SIGINT,signal.SIGTERM):signal.signal(value,handler)
    def subreaper(self):
        import ctypes
        libc=ctypes.CDLL(None,use_errno=True);flag=ctypes.c_int()
        need(libc.prctl(36,1,0,0,0)==0 and libc.prctl(37,ctypes.byref(flag),0,0,0)==0 and flag.value==1,'supervisor-subreaper')
    def original_identity(self, process):
        fields=(Path('/proc')/str(process.pid)/'stat').read_text().rsplit(')',1)[1].split()
        value=dict(pid=process.pid,group=int(fields[2]),stamp=fields[19]);identity(value,process.pid)
        return value
    def popen(self, argv, **options):return subprocess.Popen(argv,**options)
    def pipe(self):return os.pipe2(os.O_CLOEXEC)
    def nonblocking(self, fd):os.set_blocking(fd,False)
    def read(self, fd, count):return os.read(fd,count)
    def close_fd(self, fd):os.close(fd)
    def register(self, fd):
        if self.selector is None:self.selector=selectors.DefaultSelector()
        self.selector.register(fd,selectors.EVENT_READ)
    def unregister(self, fd):self.selector.unregister(fd)
    def events(self, timeout):
        if self.selector is None:time.sleep(timeout);return []
        return [key.fd for key,unused in self.selector.select(timeout)]
    def close_selector(self):
        if self.selector is not None:self.selector.close()
    def open_capture(self, path):return Path(path).open('xb')
    def signal_group(self, group, signum):os.killpg(group,signum)
    def waitpid(self):return os.waitpid(-1,os.WNOHANG)
    def private(self, evidence):
        path=Path(tempfile.mkdtemp(prefix='h2018.x.',dir='/tmp')).resolve(strict=True)
        info=path.stat()
        need(info.st_uid==os.geteuid() and stat.S_IMODE(info.st_mode)==0o700
             and len(str(path/'bus'))<100 and not path.is_relative_to(evidence) and not evidence.is_relative_to(path),'private-desktop-root')
        return str(path)
    def secret(self, path):return os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC|os.O_NOFOLLOW,0o600)
    def write_fd(self, fd, data):return os.write(fd,data)
    def fsync(self, fd):os.fsync(fd)
    def random(self):return os.urandom(16)
    def remove_private(self, private):
        # Never unlink display sockets/locks or delete an arbitrary tree.
        for name in ('server.Xauthority','client.Xauthority'):(Path(private)/name).unlink(missing_ok=True)
        Path(private).rmdir()  # A surviving bus socket/other member remains an explicit failure.


class Supervisor:
    def __init__(self, system, control, output):
        self.io,self.control,self.output=system,control,str(output)
        self.roots=[];self.handles=[];self.fds=[];self.captures=[];self.errors=[]
        self.original_streams=[];self.original_pipes=[]
        self.readers={};self.fenced=False;self.subreaper=False;self.total=0
        self.started=system.now();self.kernel=None;self.private=None;self.driver_fact=None
        self.check_driver=None;self.driver_environment=None
        self.source=None

    def negative(self, error, context):
        self.control.latch(error);self.errors.append(context+': '+repr(error))

    def close_fd(self, item):
        if item['closed']:return
        try:
            if item['registered']:
                self.io.unregister(item['fd']);item['registered']=False
        except BaseException as error:self.negative(error,'FD unregister');item['error']=repr(error)
        try:
            if item['object'] is None:self.io.close_fd(item['fd'])
            else:item['object'].close()
            item['closed']=True
        except BaseException as error:self.negative(error,'Original FD close');item['error']=repr(error)

    def retain_fd(self, fd, kind, stream=None):
        need(len(self.fds)<MAX_FDS,'supervisor-FD-cap')
        row=dict(id='supervisor-fd-'+str(len(self.fds)+1),fd=fd,kind=kind,object=stream,closed=False,registered=False,error=None,eof=False,bytes=0,data=bytearray(),capture=None)
        self.fds.append(row)  # Original descriptor/object before registration.
        return row

    def listen(self, item):
        self.io.nonblocking(item['fd']);self.io.register(item['fd']);item['registered']=True
        self.readers[item['fd']]=item

    def capture(self, name):
        row=dict(path=self.output+'/'+name,object=None,closed=False,error=None,bytes=0)
        self.captures.append(row)
        try:
            value=self.io.open_capture(row['path']);row['object']=value
        except BaseException as error:row['error']=repr(error);raise
        return row

    def ready_pipe(self):
        with self.control.defer():
            need(len(self.fds)+2<=MAX_FDS,'supervisor-FD-cap')
            pair=self.io.pipe()
            original=dict(fds=pair,members=[]);self.original_pipes.append(original)
            # Retain both integers before either nonblocking/selector operation.
            read=self.retain_fd(pair[0],'ready-read');original['members'].append(read)
            write=self.retain_fd(pair[1],'ready-parent-write');original['members'].append(write)
            self.listen(read)
            return read,write

    def start(self, role, argv, env, cwd=None, inherited=()):
        self.control.admit();need(not self.fenced and len(self.roots)<MAX_ROOTS,'supervisor-spawn-fence')
        need(self.io.now()<self.started+WHOLE_SECONDS-CLEANUP_SECONDS,'supervisor-work-budget')
        with self.control.defer():
            need(len(self.fds)+2<=MAX_FDS,'supervisor-FD-cap')
            row=dict(id='supervisor-root-'+str(len(self.roots)+1),role=role,argv=list(map(str,argv)),cwd=None if cwd is None else str(cwd),
                attempted_ns=self.io.stamp(),pid=None,identity=None,returned=False,published=False,error=None,
                waits=[],terminal=None,signals=[],forced=False,streams=[])
            self.roots.append(row)
            try:
                captures=[self.capture(row['id']+'.'+name+'.log') for name in ('stdout','stderr')]
                self.control.admit()
                process=self.io.popen(argv,cwd=cwd,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                    start_new_session=True,close_fds=True,pass_fds=tuple(item['fd'] for item in inherited))
                self.handles.append((process,row))  # FIRST action after Popen returns.
                row['returned']=True;row['pid']=process.pid
                self.original_streams.extend((process.stdout,process.stderr))
                for name,capture in zip(('stdout','stderr'),captures):
                    stream=getattr(process,name)
                    item=self.retain_fd(stream.fileno(),'capture-'+name,stream);row['streams'].append(item)
                    item['capture']=capture;self.listen(item)
                row['identity']=self.io.original_identity(process)
                identity(row['identity'],process.pid)
                row['published']=True
                return process,row
            except BaseException as error:
                row['error']=repr(error);self.control.latch(error);raise

    def pump(self, timeout, cleanup=False):
        if not cleanup:self.control.admit()
        for fd in self.io.events(max(0,min(.1,timeout))):
            item=self.readers[fd]
            if item['closed']:continue
            try:
                raw=self.io.read(fd,65536)
                if not raw:
                    item['eof']=True;self.close_fd(item);continue
                item['bytes']+=len(raw)
                if item['kind']=='ready-read':
                    need(item['bytes']<=READY_BYTES,'ready-byte-cap');item['data'].extend(raw)
                else:
                    self.total+=len(raw);need(item['bytes']<=STREAM_BYTES and self.total<=TOTAL_STREAM_BYTES,'capture-byte-cap')
                    item['capture']['object'].write(raw);item['capture']['bytes']+=len(raw)
            except BlockingIOError:continue
            except BaseException as error:
                item['error']=repr(error);self.negative(error,'Original pipe read/capture');self.close_fd(item)
                if not cleanup:raise
        if not cleanup:self.control.admit()

    def ready(self, process, row, pipe, private):
        deadline=min(self.started+WHOLE_SECONDS-CLEANUP_SECONDS,self.io.now()+READY_SECONDS)
        while not pipe['eof']:
            self.control.admit();need(process.poll() is None,'ready-original-root-died')
            need(self.io.now()<deadline,'ready-timeout')
            self.pump(deadline-self.io.now())
        self.control.admit();need(process.poll() is None and pipe['error'] is None,'ready-original-root-died-or-pipe-error')
        need(self.io.now()<deadline,'ready-timeout')
        value=ready_value(row['role'],bytes(pipe['data']),private)
        row['ready']=dict(bytes=pipe['bytes'],sha256=digest(bytes(pipe['data'])),eof=True,
                          meaning='Original launch notification only; no authenticated client readiness claim.')
        return value

    def wait_original(self, process, row):
        began=self.io.stamp()
        try:code=process.wait(timeout=0)
        except BaseException as error:
            self.control.latch(error)
            try:row['waits'].append(dict(start_ns=began,finish_ns=self.io.stamp(),exit_code=None,error=repr(error)))
            except BaseException as secondary:self.negative(secondary,'Original wait metadata UNKNOWN')
            raise
        try:
            need(type(code) is int,'Original wait result UNKNOWN')
            result=dict(start_ns=began,finish_ns=self.io.stamp(),exit_code=code,error=None)
            row['waits'].append(result);row['terminal']=result
        except BaseException as error:row['error']='Wait metadata UNKNOWN';self.negative(error,'Original wait metadata');raise
        return code

    def await_original(self, process, row, deadline, cleanup=False):
        while process.poll() is None:
            if not cleanup:self.control.admit()
            need(self.io.now()<deadline,'original-root-deadline')
            self.pump(deadline-self.io.now(),cleanup)
        code=self.wait_original(process,row)
        eof_deadline=min(deadline,self.io.now()+EOF_SECONDS)
        while not all(item['eof'] for item in row['streams']):
            need(self.io.now()<eof_deadline,'original-stream-EOF-unknown')
            self.pump(eof_deadline-self.io.now(),cleanup)
        need(self.io.now()<=deadline,'original-root-or-EOF-deadline')
        for item in row['streams']:
            capture=item['capture']
            if not capture['closed']:
                try:capture['object'].close();capture['closed']=True
                except BaseException as error:capture['error']=repr(error);self.negative(error,'Original capture close');raise
        return code

    def send(self, process, row, signum):
        need(row['identity'] is not None and self.io.original_identity(process)==row['identity'],'original-send-generation-UNKNOWN')
        attempt=dict(signal=signum,returned=False,outcome='attempted',error=None)
        row['signals'].append(attempt)
        try:self.io.signal_group(row['identity']['group'],signum)
        except BaseException as error:
            attempt['outcome']='ESRCH' if isinstance(error,ProcessLookupError) else 'error';attempt['error']=repr(error);raise
        attempt.update(returned=True,outcome='returned')
        if signum==signal.SIGKILL:row['forced']=True

    def stop(self, process, row, deadline):
        # No new-work gate: retained originals must still settle after failure.
        if process.poll() is None:
            try:self.send(process,row,signal.SIGTERM)
            except BaseException as error:self.negative(error,'Original TERM')
            grace=DRIVER_TERM_SECONDS if row['role']=='driver' else ROOT_TERM_SECONDS
            try:return self.await_original(process,row,min(deadline,self.io.now()+grace),True)
            except BaseException as error:self.negative(error,'Original TERM/wait/EOF')
            if process.poll() is None:
                try:self.send(process,row,signal.SIGKILL)
                except BaseException as error:self.negative(error,'Original KILL')
        return self.await_original(process,row,min(deadline,self.io.now()+KILL_SECONDS),True)

    def metadata(self, argv):
        process,row=self.start('metadata',argv,None)
        code=self.await_original(process,row,min(self.started+WHOLE_SECONDS-CLEANUP_SECONDS,self.io.now()+METADATA_SECONDS))
        need(code==0,'metadata-command-failed')
        # Capture files contain real bytes; read only this closed original's bounded output.
        stream=row['streams'][0];capture=stream['capture']
        return capture['path']

    def secret(self, path, raw):
        with self.control.defer():
            need(len(self.fds)<MAX_FDS,'supervisor-FD-cap')
            fd=self.io.secret(path);item=self.retain_fd(fd,'private-auth')
            first=None
            try:
                offset=0
                while offset<len(raw):
                    count=self.io.write_fd(fd,raw[offset:]);need(integer(count,1),'secret-short-write');offset+=count
                self.io.fsync(fd)
            except BaseException as error:first=error;self.control.latch(error)
            finally:self.close_fd(item)
            if first is not None:raise first
            need(item['closed'] and item['error'] is None,'secret-close-UNKNOWN')

    def launch(self, env, control, before, after, verified):
        need(verified is not None,'prepared-before-desktop')
        self.control.admit();need(self.io.now()<self.started+WHOLE_SECONDS-CLEANUP_SECONDS,'supervisor-work-budget')
        self.private=self.io.private(Path(self.output).parent.resolve(strict=True))
        cookie=self.io.random();self.secret(self.private+'/server.Xauthority',authority_bytes(cookie))
        read,write=self.ready_pipe()
        with self.control.defer():
            try:
                x,row=self.start('Xvfb',['/usr/bin/Xvfb','-displayfd',str(write['fd']),'-screen','0','1600x1000x24',
                    '-nolisten','tcp','-dpi','96','-auth',self.private+'/server.Xauthority'],dict(env),inherited=(write,))
            finally:self.close_fd(write)
        display=self.ready(x,row,read,self.private)
        self.secret(self.private+'/client.Xauthority',authority_bytes(cookie,display[1:].encode()))
        # Cookie remains only in memory/private auth files, never argv/receipt/capture.
        cookie=None
        desktop=dict(env,DISPLAY=display,XAUTHORITY=self.private+'/client.Xauthority')
        read,write=self.ready_pipe()
        with self.control.defer():
            try:
                bus,row=self.start('bus',['/usr/bin/dbus-daemon','--session','--nofork','--nopidfile','--nosyslog',
                    '--address=unix:path='+self.private+'/bus','--print-address='+str(write['fd'])],desktop,inherited=(write,))
            finally:self.close_fd(write)
        address=self.ready(bus,row,read,self.private);desktop['DBUS_SESSION_BUS_ADDRESS']=address
        self.driver_environment=desktop
        process,row=self.start('driver',['/usr/bin/python3',str(control/'.github/qualification/final-main-native.py'),
            str(control),str(before),str(after)],desktop,cwd=str(control))
        code=self.await_original(process,row,min(self.started+WHOLE_SECONDS-CLEANUP_SECONDS,self.io.now()+DRIVER_SECONDS))
        return process,row,code,desktop

    def kernel_drain(self, deadline):
        need(self.fenced and self.subreaper and all(row['returned'] and row['terminal'] is not None for row in self.roots),'kernel-originals-or-fence-UNKNOWN')
        result=dict(waits=[],echild=False,errors=[])
        while self.io.now()<deadline:
            try:pid,status=self.io.waitpid()
            except ChildProcessError:result['echild']=True;return result
            need(integer(pid) and integer(status) and (pid!=0 or status==0),'kernel-wait-types')
            if pid:
                need(len(result['waits'])<MAX_ADOPTED,'kernel-wait-cap')
                code=-(status&0x7f) if status&0x7f else (status>>8)&255
                result['waits'].append(dict(pid=pid,raw_status=status,exit_code=code))
                if any(row.get('pid')==pid for row in self.roots):result['errors'].append('Kernel status overlaps retained original root')
                if code==-signal.SIGKILL:result['errors'].append('Adopted original writer SIGKILL')
            else:self.pump(min(.1,deadline-self.io.now()),True)
        result['errors'].append('Final scoped ECHILD UNKNOWN');return result

    def finish(self):
        self.control.finalizing=True;self.fenced=True
        deadline=min(self.started+WHOLE_SECONDS,self.io.now()+CLEANUP_SECONDS)
        # Driver first, while original bus/X remain available for Documents/callback cleanup.
        ordered=sorted(self.handles,key=lambda pair:0 if pair[1]['role']=='driver' else (1 if pair[1]['role']=='bus' else 2))
        for process,row in ordered:
            try:self.stop(process,row,deadline)
            except BaseException as error:
                self.negative(error,'Original root settlement')
                try:self.wait_original(process,row)  # Actual nonblocking wait even after identity/timeout failure.
                except BaseException as secondary:self.negative(secondary,'Original final wait attempt')
            if row['role']=='driver':
                try:
                    need(self.check_driver is not None,'Independent driver ledger reader UNSUPPLIED')
                    self.driver_fact=self.check_driver(process,row)
                    need(self.driver_fact['local_settled'],'Driver local writer/role settlement UNKNOWN')
                except BaseException as error:self.negative(error,'Independent driver ledger/wait/streams')
        try:self.kernel=self.kernel_drain(min(deadline,self.io.now()+KERNEL_SECONDS))
        except BaseException as error:self.negative(error,'Scoped kernel drain');self.kernel=dict(waits=[],echild=False,errors=[repr(error)])
        self.errors.extend(self.kernel['errors'])
        for item in self.fds:self.close_fd(item)
        for stream in self.original_streams:
            if not any(item['object'] is stream for item in self.fds):
                try:stream.close()
                except BaseException as error:self.negative(error,'Unpublished original PIPE close')
                self.errors.append('Original PIPE metadata UNKNOWN')
        for original in self.original_pipes:
            for index,fd in enumerate(original['fds']):
                if index>=len(original['members']):
                    try:self.io.close_fd(fd)
                    except BaseException as error:self.negative(error,'Unpublished original ready FD close')
                    self.errors.append('Original ready FD metadata UNKNOWN')
        for row in self.captures:
            try:
                if row['object'] is not None and not row['closed']:row['object'].close();row['closed']=True
            except BaseException as error:row['error']=repr(error);self.negative(error,'Original capture close')
        try:self.io.close_selector()
        except BaseException as error:self.negative(error,'Selector close')
        for row in self.roots:
            if not row['returned'] or not row['published'] or row['identity'] is None or row['error'] or row['terminal'] is None:
                self.errors.append(row['id']+' original UNKNOWN')
            if row['forced'] or (row['terminal'] is not None and row['terminal']['exit_code']==-signal.SIGKILL):self.errors.append(row['id']+' forced/actual SIGKILL')
            if row['terminal'] is not None:
                code=row['terminal']['exit_code']
                returned_term=any(s['signal']==signal.SIGTERM and s['returned'] for s in row['signals'])
                if code!=0 and not (row['role'] in ('Xvfb','bus') and code==-signal.SIGTERM and returned_term):self.errors.append(row['id']+' negative original outcome '+str(code))
        if any(not item['closed'] or item['error'] or (item['kind'].startswith('capture-') and not item['eof']) for item in self.fds):self.errors.append('Original descriptor/EOF UNKNOWN')
        if any(not row['closed'] or row['error'] for row in self.captures):self.errors.append('Original capture writer UNKNOWN')
        if self.source is None:self.errors.append('Supervisor source binding UNKNOWN')
        if self.private is not None and self.kernel['echild'] and not any(row['terminal'] is None for row in self.roots):
            try:self.io.remove_private(self.private)
            except BaseException as error:self.negative(error,'Private auth/socket directory cleanup')
        return self.record()

    def record(self):
        def clean(item):return {k:v for k,v in item.items() if k not in ('object','data','capture','streams')}
        return dict(kind='held-scope-supervisor-settlement-v1',qualified=False,retained_evidence_verified_here=False,
            supervisor_instance=self.io.instance,roots=[dict(**clean(row),streams=[clean(item) for item in row['streams']]) for row in self.roots],
            source=self.source,
            descriptors=[clean(item) for item in self.fds],captures=[clean(item) for item in self.captures],
            spawn_fenced=self.fenced,subreaper=self.subreaper,kernel=self.kernel,driver_ledger=self.driver_fact,
            first_diagnostic=None if self.control.failure is None else repr(self.control.failure),cancel_signal=self.control.signum,errors=list(self.errors),
            outside_scope=['Supervisor later exit and parent-captured stdout/stderr','Immutable staging/seals',
                'Original external entry wait2343 UNSUPPLIED','Approved hosted positive producer entryf478 UNSUPPLIED',
                'Whole-job/collector/independent acceptance and resource/merge authority'])


FIELDS={
 'top':'kind scope binding entered_arms creations writers callbacks kernel no_future_spawns errors first_terminal_diagnostic requested_cancel outside_scope final_publication',
 'binding':'product before control_sha control_tree driver_sha256 fixture_sha256 preparation_sha256 preparation_module_sha256 late_environment_sha256 workflow_sha256 run attempt logical_job numeric_job_id numeric_job_note plan_sha256 driver_instance',
 'creation':'id scope role argv cwd attempted_ns pid identity pidfd pidfd_note expected_mount returned published error writers waits terminal signals forced exit_policy',
 'wait':'method start_ns finish_ns exit_code error',
 'writer':'id scope path kind acquired closed error inherited_by callback',
 'callback':'id scope path accepting inflight closed filter_id filter_removed monitor query initialized errors',
 'signal':'signal unix_ns scope target returned outcome error'}
OUTSIDE=['Xvfb/private session-bus roots and activated families','Driver later exit and captured parent streams',
         'Native capsule/staging, whole-job/executor return and independent acceptance']
FINAL='This sidecar is the final driver native-payload write; its own bytes are not self-hashed.'
PARTIAL='PARTIAL UNKNOWN; unsettled writers may remain.'


def unique_strings(value, reason):
    need(type(value) is list and all(type(v) is str for v in value) and len(set(value))==len(value),reason)


def strings(value, reason):
    need(type(value) is list and all(type(v) is str for v in value),reason)


def nullable_string(value):return value is None or type(value) is str


def consume_driver(value, source, expected, original, terminal, captured, arm_cleanups, mounts, parent_pid):
    """Strict frozen producer reader; data only, never opens row-named paths."""
    keys(value,FIELDS['top'],'driver-ledger-shape');keys(source,FIELDS['binding'],'driver-source-shape')
    need(value['kind']=='driver-role-writer-ledger-v1' and value['scope']=='This original driver only; pre-exit statement.','driver-ledger-version')
    need(value['binding']==source and {k:v for k,v in source.items() if k!='driver_instance'}==expected,'driver-trusted-source')
    identity(original)
    keys(source['driver_instance'],'pid observation','driver-instance-shape')
    instance=source['driver_instance'];obs=instance['observation']
    keys(obs,'state parent group stamp','driver-instance-observation')
    need(instance['pid']==original['pid'] and type(instance['pid']) is int and obs['stamp']==original['stamp']
         and obs['group']==original['group'] and type(obs['group']) is int and obs['parent']==parent_pid
         and type(obs['parent']) is int and obs['state'] in ('R','S','D','T','t','I','P','W'),'driver-original-generation')
    need(source['numeric_job_id'] is None and source['numeric_job_note']=='No actual supported numeric job supplier is implemented.', 'driver-logical-job-only')
    need(value['entered_arms'] in ([],['before'],['before','after']),'driver-entered-prefix')
    need(type(value['no_future_spawns']) is bool and type(value['requested_cancel']) in (type(None),int)
         and value['requested_cancel'] in (None,2,15) and nullable_string(value['first_terminal_diagnostic']),'driver-work-types')
    strings(value['errors'],'driver-errors');need(value['outside_scope']==OUTSIDE and value['final_publication'] in (FINAL,PARTIAL),'driver-scope-publication')
    bad=list(value['errors'])
    if value['no_future_spawns'] is not True or value['final_publication']!=FINAL:bad.append('Driver spawning/writer boundary incomplete')
    need(type(terminal) is dict and type(terminal.get('exit_code')) is int and terminal.get('error') is None,'independent-original-driver-wait')
    need(type(captured) is list and len(captured)==2 and all(item.get('eof') is True and item.get('closed') is True
         and item.get('error') is None and item.get('capture_closed') is True for item in captured),'independent-original-driver-streams')
    if terminal['exit_code']==-signal.SIGKILL:bad.append('Actual driver SIGKILL')
    tables={}
    for plural,prefix,cap in [('creations','creation',4096),('writers','writer',16384),('callbacks','callback',4096)]:
        rows=value[plural];need(type(rows) is list and len(rows)<=cap,'driver-'+plural+'-cap')
        table={}
        for index,row in enumerate(rows,1):
            keys(row,FIELDS[prefix],'driver-'+prefix+'-shape')
            need(row['id']==prefix+'-'+str(index) and row['scope'] in ['global-driver',*value['entered_arms']],'driver-member-id-scope')
            table[row['id']]=row
        tables[plural]=table
    roots=tables['creations'];writers=tables['writers'];callbacks=tables['callbacks']
    generations=set();roles=set()
    for row in roots.values():
        need(row['role'] in (GLOBALS if row['scope']=='global-driver' else ARMS),'driver-role-scope')
        if row['role']!='command':
            pair=(row['scope'],row['role']);need(pair not in roles,'driver-role-duplicate');roles.add(pair)
        need(type(row['argv']) is list and row['argv'] and all(type(v) is str for v in row['argv'])
             and nullable_string(row['cwd']) and integer(row['attempted_ns'],1),'driver-command-fields')
        need(all(type(row[k]) is bool for k in ('returned','published','forced')) and nullable_string(row['error'])
             and nullable_string(row['expected_mount']) and row['pidfd'] is None
             and (row['pid'] is None or integer(row['pid'],1))
             and row['pidfd_note']=='Not acquired; authority is the original retained Popen.','driver-creation-types')
        if row['identity'] is not None:
            identity(row['identity'],row['pid']);generation=(row['pid'],row['identity']['stamp'])
            need(generation not in generations,'driver-generation-duplicate');generations.add(generation)
        else:bad.append(row['id']+' identity UNKNOWN')
        if not row['returned'] or not row['published'] or row['error'] is not None:bad.append(row['id']+' acquisition UNKNOWN')
        unique_strings(row['writers'],'driver-creation-writers')
        need(type(row['waits']) is list and len(row['waits'])<=256,'driver-wait-cap')
        successes=[]
        for wait in row['waits']:
            keys(wait,FIELDS['wait'],'driver-wait-shape')
            need(wait['method'] in ('wait','communicate','identity-unresolved-wait','Documents-normal-TERM')
                 and integer(wait['start_ns'],1) and integer(wait['finish_ns'],1)
                 and (wait['exit_code'] is None or type(wait['exit_code']) is int) and nullable_string(wait['error']),'driver-wait-types')
            # time.time_ns can move backwards; timestamps are observations, not ordering authority.
            if wait['error'] is None and type(wait['exit_code']) is int:successes.append(wait)
        if not successes or row['terminal']!=successes[-1]:bad.append(row['id']+' terminal UNKNOWN')
        keys(row['exit_policy'],'kind allowed_exit_codes','driver-policy-shape')
        need(type(row['exit_policy']['allowed_exit_codes']) is list
             and all(type(code) is int for code in row['exit_policy']['allowed_exit_codes']),'driver-policy-types')
        if row['role']=='command':
            allowed=[0,1] if row['argv']==['xdotool','search','--onlyvisible','--class','cagents'] else [0]
            policy=dict(kind='declared-command-exits',allowed_exit_codes=allowed)
        else:policy=dict(kind='documents-normal-TERM-wait0-unmount' if row['role']=='documents' else 'ordinary-native-zero-or-issued-TERM',allowed_exit_codes=[0])
        need(row['exit_policy']==policy,'driver-source-policy')
        need(type(row['signals']) is list and len(row['signals'])<=4096,'driver-signal-cap')
        returned_term=False;returned_kill=False
        for send in row['signals']:
            descendant=send.get('scope')=='original stamped descendant'
            keys(send,FIELDS['signal']+(' pid stamp' if descendant else ''),'driver-signal-shape')
            need(send['signal'] in (15,9) and type(send['signal']) is int and integer(send['unix_ns'],1)
                 and integer(send['target'],1) and type(send['returned']) is bool and nullable_string(send['error']),'driver-signal-types')
            if send['scope']=='original group':need(row['identity'] is not None and send['target']==row['identity']['group'],'driver-group-target')
            elif send['scope']=='original Documents':need(row['role']=='documents' and send['target']==row['pid'],'driver-Documents-target')
            else:
                need(descendant and send['target']==send['pid'] and integer(send['pid'],1)
                     and type(send['stamp']) is str and STAMP.fullmatch(send['stamp']),'driver-descendant-target')
            if send['returned']:
                need(send['outcome']=='returned' and send['error'] is None,'driver-signal-return')
                if send['signal']==15 and send['scope']=='original group':returned_term=True
                if send['signal']==9:returned_kill=True
            else:
                need((send['outcome']=='attempted' and send['error'] is None)
                     or (send['outcome'] in ('ESRCH','error') and type(send['error']) is str),'driver-signal-negative')
                if send['outcome']=='attempted':bad.append('Unfinished signal attempt UNKNOWN')
        need(not returned_kill or row['forced'] is True,'driver-forced-receipt')
        if row['forced']:bad.append('Driver forced KILL send')
        if successes:
            code=successes[-1]['exit_code'];controlled=policy['kind']=='ordinary-native-zero-or-issued-TERM' and returned_term and code==-15
            if code==-9 or not (code in policy['allowed_exit_codes'] or controlled):bad.append('Driver unexpected terminal outcome')
        if row['role']=='documents':
            need(row['expected_mount']==mounts[row['scope']],'driver-Documents-prepared-mount')
            cleanup=arm_cleanups.get(row['scope'])
            if cleanup is None:bad.append('Driver Documents cleanup receipt UNKNOWN')
            else:
                keys(cleanup,'errors documents_graceful private_bus_names owned_descendants_remaining','driver-arm-cleanup-shape')
                graceful=cleanup['documents_graceful']
                if graceful is None:bad.append('Driver Documents graceful UNKNOWN')
                else:
                    keys(graceful,'exit_code mount mounted_after identity','driver-Documents-graceful-shape')
                    need(type(graceful['exit_code']) is int and graceful['mounted_after'] is False,'driver-Documents-graceful-types')
                    need(graceful==dict(exit_code=0,mount=row['expected_mount'],mounted_after=False,identity=row['identity']),'driver-Documents-graceful')
                if cleanup['errors'] or cleanup['owned_descendants_remaining'] or cleanup['private_bus_names']!={name:None for name in BUS_NAMES}:bad.append('Driver arm settlement UNKNOWN')
                if not successes or successes[-1]['exit_code']!=0 or not any(w['method']=='Documents-normal-TERM' and w['exit_code']==0 and w['error'] is None for w in row['waits']):bad.append('Driver Documents normal wait UNKNOWN')
                if not any(s['scope']=='original Documents' and s['signal']==15 and s['returned'] for s in row['signals']):bad.append('Driver Documents normal send UNKNOWN')
    kinds={'child-PIPE','parent-log','synchronous-payload','synchronous-image','synchronous-sqlite','clipboard-input','callback-log'}
    for row in writers.values():
        need(row['kind'] in kinds and type(row['path']) is str and row['path'] and nullable_string(row['error'])
             and type(row['acquired']) is bool and type(row['closed']) is bool,'driver-writer-types')
        unique_strings(row['inherited_by'],'driver-writer-inheritance')
        need(all(child in roots and row['id'] in roots[child]['writers'] for child in row['inherited_by']),'driver-writer-reciprocal')
        if row['kind']=='child-PIPE':
            match=re.fullmatch(r'<(creation-[1-9][0-9]*)\.(stdin|stdout|stderr)>',row['path'])
            need(match is not None and row['inherited_by']==[match[1]],'driver-symbolic-PIPE')
        if row['callback'] is not None:
            need(row['callback'] in callbacks and row['kind']=='callback-log' and row['path']==callbacks[row['callback']]['path']
                 and row['scope']==callbacks[row['callback']]['scope'],'driver-callback-writer')
        elif row['kind']=='callback-log':bad.append('Driver callback writer association UNKNOWN')
        if not row['acquired'] or not row['closed'] or row['error'] is not None:bad.append('Driver writer UNKNOWN')
    for row in roots.values():
        need(all(w in writers and row['id'] in writers[w]['inherited_by'] for w in row['writers']),'driver-root-reciprocal')
        owned=[writers[w] for w in row['writers']]
        if row['role']=='command':
            if any(sum(w['path']=='<'+row['id']+'.'+name+'>' and w['kind']=='child-PIPE' for w in owned)!=1 for name in ('stdout','stderr')):bad.append('Driver command PIPE membership UNKNOWN')
        elif not any(w['kind']=='parent-log' for w in owned):bad.append('Driver root log membership UNKNOWN')
        if row['role']=='clipboard' and not any(w['kind']=='clipboard-input' for w in owned):bad.append('Driver clipboard input membership UNKNOWN')
    for row in callbacks.values():
        need(type(row['path']) is str and row['scope'] in value['entered_arms']
             and all(type(row[k]) is bool for k in ('accepting','closed','filter_removed','initialized'))
             and integer(row['inflight']) and row['monitor'] in ('uncreated','acquisition-attempted','acquired','closed')
             and row['query'] in ('uncreated','acquisition-attempted','acquired','closed'),'driver-callback-types')
        strings(row['errors'],'driver-callback-errors')
        if not (row['initialized'] and not row['accepting'] and row['inflight']==0 and row['closed'] and row['filter_removed']
                and row['monitor']==row['query']=='closed' and integer(row['filter_id'],1) and not row['errors']):bad.append('Driver callback UNKNOWN')
        need(sum(w['callback']==row['id'] for w in writers.values())==1 or not row['initialized'],'driver-callback-single-writer')
    kernel=value['kernel'];need(type(kernel) is dict,'driver-kernel-type')
    if set(kernel)=={'echild','waits','errors'}:
        need(kernel['echild'] is False and kernel['waits']==[] and type(kernel['errors']) is list and kernel['errors'],'driver-negative-kernel')
        strings(kernel['errors'],'driver-negative-kernel-errors');bad.append('Driver kernel exception UNKNOWN')
    else:
        keys(kernel,'scope waits echild errors','driver-kernel-shape')
        need(kernel['scope']=='Original driver subreaper kernel children only' and type(kernel['echild']) is bool
             and type(kernel['waits']) is list and len(kernel['waits'])<=4096,'driver-kernel-types')
        strings(kernel['errors'],'driver-kernel-errors')
        if not kernel['echild'] or kernel['errors']:bad.append('Driver kernel ECHILD UNKNOWN')
        for waited in kernel['waits']:
            keys(waited,'pid raw_status exit_code unix_ns','driver-kernel-wait-shape')
            need(integer(waited['pid'],1) and integer(waited['raw_status']) and integer(waited['unix_ns'],1),'driver-kernel-wait-types')
            raw=waited['raw_status'];code=-(raw&127) if raw&127 else (raw>>8)&255
            need(type(waited['exit_code']) is int and waited['exit_code']==code,'driver-kernel-decode')
            if code==-9 or any(r['pid']==waited['pid'] for r in roots.values()):bad.append('Driver kernel status invalid for settlement')
    complete_roles=all((scope,role) in roles for scope in ('before','after') for role in ARMS-{'command'})
    # Entry alone proves neither a clean source nor any native frame. Those
    # remain with the unchanged preparation/driver and future native checker.
    work_ok=not bad and terminal['exit_code']==0 and value['entered_arms']==['before','after'] and complete_roles
    work_ok=work_ok and value['first_terminal_diagnostic'] is None and value['requested_cancel'] is None
    return dict(kind='independent-frozen-driver-consumption-v1',local_settled=not bad,work_outcome_ok=work_ok,
        errors=bad,entered_arms=value['entered_arms'],first_driver_diagnostic=value['first_terminal_diagnostic'],
        requested_driver_cancel=value['requested_cancel'],qualified=False,
        limit='Neither local settlement nor entry proves source cleanliness/native8 frames, supervisor later exit/staging/whole-job or acceptance.')


def regular_bytes(path, cap=LEDGER_BYTES, allow_empty=False):
    """Expected caller-selected file only; no ledger-supplied path is opened."""
    began=time.monotonic();fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
    try:
        before=os.fstat(fd)
        need(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_uid==os.geteuid()
             and (before.st_size>0 or allow_empty) and before.st_size<=cap,'original-regular-file')
        chunks=[];size=0
        while True:
            need(time.monotonic()-began<JSON_SECONDS,'original-file-read-deadline')
            raw=os.read(fd,min(65536,cap-size+1))
            need(time.monotonic()-began<JSON_SECONDS,'original-file-read-deadline')
            if not raw:break
            size+=len(raw);need(size<=cap,'original-file-byte-cap');chunks.append(raw)
        after=os.fstat(fd)
        need((before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)==
             (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns)
             and size==before.st_size,'original-file-changed')
        return b''.join(chunks)
    finally:os.close(fd)


def parse_json(raw):
    def unique(pairs):
        value={}
        for key,item in pairs:
            need(key not in value,'duplicate-JSON-key');value[key]=item
        return value
    def constant(value):raise ValueError('Nonfinite JSON '+value)
    value=json.loads(raw,object_pairs_hook=unique,parse_constant=constant)
    pending=[(value,0)];count=0
    while pending:
        item,depth=pending.pop();count+=1
        need(depth<=JSON_DEPTH and count<=JSON_NODES,'JSON-complexity-cap')
        if isinstance(item,str):need(len(item)<=JSON_STRING,'JSON-string-cap')
        elif type(item) is dict:
            pending.extend((v,depth+1) for pair in item.items() for v in pair)
        elif type(item) is list:pending.extend((v,depth+1) for v in item)
    return value


def load_preparation(control):
    path=control/'.github/qualification/final-main-prepare.py'
    need(digest(path.read_bytes())==PREPARATION_SHA,'Frozen preparation module changed')
    spec=importlib.util.spec_from_file_location('held_scope_preparation_for_supervisor',path)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module  # Exact stdlib definitions only; never the --verify CLI.


def inspect_driver(owner, control, prepared, preparation_sha, identity_control, plan, env):
    native=Path(env['QUALIFICATION_EVIDENCE'])/'native'
    value=parse_json(regular_bytes(native/'driver-role-writer-ledger.json'))
    source=parse_json(regular_bytes(native/'source.json'))
    expected=dict(product=plan['product'],before=plan['before'],control_sha=identity_control['sha'],control_tree=identity_control['tree'],
        driver_sha256=DRIVER_SHA,fixture_sha256=digest((control/'.github/qualification/final-main-fixture.rs').read_bytes()),
        preparation_sha256=preparation_sha,preparation_module_sha256=PREPARATION_SHA,
        late_environment_sha256={key:digest(owner.driver_environment[key].encode()) for key in ('DISPLAY','XAUTHORITY','DBUS_SESSION_BUS_ADDRESS','AT_SPI_BUS_ADDRESS') if key in owner.driver_environment},
        workflow_sha256=digest((control/'.github/workflows/qualify-final-main-integration.yml').read_bytes()),
        run=env['GITHUB_RUN_ID'],attempt=env['GITHUB_RUN_ATTEMPT'],logical_job=env['GITHUB_JOB'],numeric_job_id=None,
        numeric_job_note='No actual supported numeric job supplier is implemented.',plan_sha256=digest((control/'.github/qualification/final-main-plan.json').read_bytes()))
    cleanups={}
    for label in value['entered_arms']:
        path=native/(label+'-cleanup.json')
        if path.exists():cleanups[label]=parse_json(regular_bytes(path))
    mounts={label:str(Path(prepared['base'])/label/'runtime/doc') for label in ('before','after')}
    def consume(process,row):
        captured=[dict(eof=item['eof'],closed=item['closed'],error=item['error'],capture_closed=item['capture']['closed']) for item in row['streams']]
        return consume_driver(value,source,expected,row['identity'],row['terminal'],captured,cleanups,mounts,owner.io.pid)
    return consume


def publish_record(path, value):
    temporary=path.with_name('.'+path.name+'.tmp')
    need(not path.exists(),'Prior supervisor record exists')
    raw=(json.dumps(value,indent=2,sort_keys=True)+'\n').encode()
    need(len(raw)<=LEDGER_BYTES,'Supervisor record byte cap')
    stream=temporary.open('xb');first=None
    try:
        stream.write(raw);stream.flush();os.fsync(stream.fileno())
    except BaseException as error:first=error
    finally:
        try:stream.close()
        except BaseException as error:
            if first is None:first=error
    if first is not None:raise first
    os.replace(temporary,path)
    # Final supervisor payload write; its later exit/captured streams remain outside.


def main():
    env=dict(os.environ);require_user(env)
    need(len(sys.argv)==4,'Pass CONTROL BEFORE AFTER')
    control_arg=Path(sys.argv[1])
    plan=json.loads((control_arg/'.github/qualification/final-main-plan.json').read_bytes())
    preparation=load_preparation(control_arg)
    # Source-pinned NULL admission precedes resolve/stat/cache/targets/mkdir,
    # System/factory/capture/metadata child and every private desktop operation.
    preparation.admit_references(plan['native_preparation']['roles'],preparation.PINNED_ROLES)
    control,before,after=[Path(v).resolve(strict=True) for v in sys.argv[1:]]
    evidence=Path(env['QUALIFICATION_EVIDENCE']);info=evidence.stat()
    need(evidence.is_absolute() and evidence.resolve(strict=True)==evidence and stat.S_ISDIR(info.st_mode)
         and info.st_uid==os.geteuid(),'canonical-owned-evidence-root')
    output=evidence/'supervisor'
    output.mkdir(mode=0o700)
    system=System();cancel=Control();system.install(cancel.request)
    owner=Supervisor(system,cancel,output)
    try:
        with cancel.work():
            system.subreaper();owner.subreaper=True
            def git(checkout, *args):
                capture=owner.metadata(['git','-C',str(checkout),*args])
                return regular_bytes(capture,16384,True).decode().strip()
            current=dict(sha=git(control,'rev-parse','HEAD'),tree=git(control,'rev-parse','HEAD^{tree}'))
            need(not git(control,'status','--porcelain=v1'),'Dirty control source')
            need(digest((control/'.github/qualification/final-main-native.py').read_bytes())==DRIVER_SHA,'Frozen driver bytes changed')
            for label,checkout in [('before',before),('after',after)]:
                pin=plan['before' if label=='before' else 'product']
                need(git(checkout,'rev-parse','HEAD')==pin['sha'] and git(checkout,'rev-parse','HEAD^{tree}')==pin['tree']
                     and not git(checkout,'status','--porcelain=v1'),'Matched checkout source changed')
            prepared,prepared_sha=admit_preparation(plan,preparation,
                lambda:preparation.load_prepared(control,plan,{'before':before,'after':after},env,current))
            owner.source=dict(control=current,product=plan['product'],before=plan['before'],run=env['GITHUB_RUN_ID'],
                attempt=env['GITHUB_RUN_ATTEMPT'],logical_job=env['GITHUB_JOB'],numeric_job_id=None,
                supervisor_sha256=digest(Path(__file__).read_bytes()),driver_sha256=DRIVER_SHA,
                preparation_module_sha256=PREPARATION_SHA,prepared_sha256=prepared_sha)
            need(not (Path(env['QUALIFICATION_EVIDENCE'])/'native/driver-role-writer-ledger.json').exists(),'Prior driver ledger exists')
            def reader(process,row):
                return inspect_driver(owner,control,prepared,prepared_sha,current,plan,env)(process,row)
            owner.check_driver=reader
            unused,row,code,desktop=owner.launch(env,control,before,after,prepared)
            need(code==0,'Driver work failed')
    except BaseException as error:cancel.latch(error)
    finally:
        try:record=owner.finish()
        except BaseException as error:
            cancel.latch(error);owner.errors.append('Supervisor settlement interrupted: '+repr(error));record=owner.record()
        try:publish_record(output/'supervisor-settlement.json',record)
        except BaseException as error:cancel.latch(error)
    if cancel.failure is not None:raise cancel.failure
    if cancel.cancel is not None:raise cancel.cancel
    need(not record['errors'],'Supervisor settlement UNKNOWN')


if __name__=='__main__':main()
