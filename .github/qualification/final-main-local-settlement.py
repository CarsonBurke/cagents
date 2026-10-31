"""Data-only consistency checks for the frozen supervisor's local statement.

Inputs are already retained bytes/data supplied by a future source-bound caller.
This module opens nothing and imports no authored module. The driver_check
callback must be the independently source-verified frozen consume_driver; its
provenance is a caller responsibility, not certified by a supplied hash string.
No result here admits broad work, seals native evidence or supplies a later
supervisor/entry wait, outer streams, hosted admission or native-flow review.
"""
import hashlib
import re

SUPERVISOR_SHA = '99bc991b39911bed660ef7f0b24796d16f2f42e82ef422f92ba9346ed434965c'
DRIVER_SHA = '12fc9a98b7eee40fa89a6463bf2ba463ef53f47248ded7f14ceee8cd6aa4033b'
PREPARATION_SHA = '289d578f7465b00b182a0f18f89b6861b6aaffce34d41d78ad1ff9c73b1236a6'
MAX_ROOTS = 12
MAX_FDS = 64
MAX_ADOPTED = 4096
STREAM_BYTES = 64 * 1024 * 1024
TOTAL_STREAM_BYTES = 256 * 1024 * 1024
READY_BYTES = 1024
OUTSIDE = ['Supervisor later exit and parent-captured stdout/stderr','Immutable staging/seals',
           'Original external entry wait2343 UNSUPPLIED','Approved hosted positive producer entryf478 UNSUPPLIED',
           'Whole-job/collector/independent acceptance and resource/merge authority']
DRIVER_LIMIT = 'Neither local settlement nor entry proves source cleanliness/native8 frames, supervisor later exit/staging/whole-job or acceptance.'
UNKNOWN = ['No independent original-supervisor terminal or parent stdout/stderr supplier.',
           'Original external entry wait2343 and hosted positive entryf478 remain UNSUPPLIED.',
           'Full native8 flow, preparation inventories, immutable staging, official artifacts and acceptance are outside this reader.',
           'Ready-line raw bytes are absent from the supervisor DTO; only its count/hash/EOF associations can be checked.',
           'Directory identities and kernel waits are recorded observations, not independently re-observed here.',
           'Inter-service capture close timing is absent; prior X/bus numeric FDs may be reused after EOF, not proven live across later acquisitions.',
           'The source-verified data-only driver callback is supplied by the caller; a matching label does not authenticate callable code.']
TOP = 'kind qualified retained_evidence_verified_here supervisor_instance roots source descriptors captures directories spawn_fenced subreaper kernel driver_ledger first_diagnostic cancel_signal errors outside_scope'
SOURCE = 'control product before run attempt logical_job numeric_job_id supervisor_sha256 driver_sha256 preparation_module_sha256 prepared_sha256'
ROOT = 'id role argv cwd attempted_ns pid identity returned published error waits terminal signals forced streams'
FD = 'id fd kind closed registered error eof bytes'
CAPTURE = 'path closed error bytes'
DIRECTORY = 'id kind attempted created path identity fd_id validated removed error'
FACT = 'kind local_settled work_outcome_ok errors entered_arms first_driver_diagnostic requested_driver_cancel qualified limit'


def need(value, reason):
    if not value:
        raise ValueError(reason)


def keys(value, fields, reason):
    expected=fields.split()
    need(type(value) is dict and len(value) == len(expected) and set(value) == set(expected),reason)


def integer(value, minimum=0):
    return type(value) is int and value >= minimum


def text(value, limit=65536):
    return type(value) is str and len(value) <= limit and '\x00' not in value


def strings(value, reason):
    need(type(value) is list and len(value) <= 4096 and all(text(item) for item in value),reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def hex_text(value, size):
    return type(value) is str and re.fullmatch('[0-9a-f]{'+str(size)+'}',value) is not None


def path_text(value):
    need(text(value,4096) and value.startswith('/') and value != '/' and not value.endswith('/')
         and all(part not in ('','.','..') for part in value[1:].split('/')),'canonical-path-data')
    return value


def instance(value, root=False):
    keys(value,'pid group stamp','instance-shape')
    need(integer(value['pid'],1) and integer(value['group'],1) and text(value['stamp'],32)
         and re.fullmatch('[1-9][0-9]{0,31}',value['stamp']) is not None,'instance-types')
    if root:
        need(value['pid'] == value['group'],'original-root-group')


def source_data(value):
    keys(value,SOURCE,'source-shape')
    for name in ('control','before','product'):
        keys(value[name],'sha tree','source-revision')
        need(all(hex_text(value[name][key],40) for key in ('sha','tree')),'source-revision')
    need(all(text(value[key],32) and re.fullmatch('[1-9][0-9]*',value[key]) is not None for key in ('run','attempt'))
         and text(value['logical_job'],256) and value['logical_job'] and value['numeric_job_id'] is None,'source-job')
    need(all(hex_text(value[key],64) for key in ('supervisor_sha256','driver_sha256','preparation_module_sha256','prepared_sha256')),'source-hashes')
    need(value['supervisor_sha256'] == SUPERVISOR_SHA and value['driver_sha256'] == DRIVER_SHA
         and value['preparation_module_sha256'] == PREPARATION_SHA,'frozen-source-hashes')


def wait_data(value):
    keys(value,'start_ns finish_ns exit_code error','wait-shape')
    need(integer(value['start_ns'],1) and integer(value['finish_ns'],1)
         and (value['exit_code'] is None or type(value['exit_code']) is int)
         and (value['error'] is None or text(value['error'])),'wait-types')
    need((value['error'] is None and type(value['exit_code']) is int)
         or (type(value['error']) is str and value['exit_code'] is None),'wait-outcome')


def descriptor(value):
    keys(value,FD,'descriptor-shape')
    need(text(value['id'],64) and integer(value['fd']) and text(value['kind'],64)
         and all(type(value[key]) is bool for key in ('closed','registered','eof'))
         and integer(value['bytes']) and (value['error'] is None or text(value['error'])),'descriptor-types')
    need(value['closed'] and not value['registered'] and value['error'] is None,'descriptor-unsettled')


def driver_fact(value):
    keys(value,FACT,'driver-fact-shape')
    need(value['kind'] == 'independent-frozen-driver-consumption-v1'
         and all(type(value[key]) is bool for key in ('local_settled','work_outcome_ok','qualified'))
         and value['qualified'] is False and value['limit'] == DRIVER_LIMIT,'driver-fact-types')
    strings(value['errors'],'driver-fact-errors')
    need(type(value['entered_arms']) is list and all(type(item) is str for item in value['entered_arms'])
         and value['entered_arms'] in ([],['before'],['before','after'])
         and (value['first_driver_diagnostic'] is None or text(value['first_driver_diagnostic']))
         and (value['requested_driver_cancel'] is None or type(value['requested_driver_cancel']) is int
              and value['requested_driver_cancel'] in (2,15)),'driver-fact-work')


def fixed_captures():
    return ['supervisor/supervisor-root-'+str(index)+'.'+stream+'.log'
            for index in range(1,13) for stream in ('stdout','stderr')]


def validate_local(record, expected_source, paths, owner_uid, supervisor_instance,
                   captures, driver_inputs, expected_driver, driver_check, driver_check_source_sha256):
    """Internal normal-path consistency only. Every byte lookup uses fixed names.

    expected_source/expected_driver/paths/instance/uid must come from an
    independently authenticated caller context, never this record's verdict.
    driver_inputs has the existing ledger/source/cleanup/mount DATA objects;
    driver_check has the exact frozen consume_driver signature, with no I/O.
    """
    keys(record,TOP,'supervisor-shape')
    need(record['kind'] == 'held-scope-supervisor-settlement-v1'
         and record['qualified'] is False and record['retained_evidence_verified_here'] is False
         and record['outside_scope'] == OUTSIDE,'supervisor-kind-scope')
    strings(record['errors'],'supervisor-errors')
    need(record['first_diagnostic'] is None or text(record['first_diagnostic']),'supervisor-diagnostic-type')
    need(record['cancel_signal'] is None or type(record['cancel_signal']) is int and record['cancel_signal'] in (2,15),'supervisor-cancel-type')
    need(record['spawn_fenced'] is True and record['subreaper'] is True,'supervisor-fence')
    source_data(expected_source);source_data(record['source'])
    need(record['source'] == expected_source,'independent-source-join')
    instance(supervisor_instance);instance(record['supervisor_instance'])
    need(record['supervisor_instance'] == supervisor_instance,'supervisor-original-generation')
    keys(paths,'control before after evidence','context-paths')
    for value in paths.values():path_text(value)
    need(len(set(paths.values())) == 4 and integer(owner_uid),'context-paths-uid')
    roots,descriptors,capture_rows = record['roots'],record['descriptors'],record['captures']
    need(type(roots) is list and len(roots) == MAX_ROOTS,'normal-root-membership')
    need(type(descriptors) is list and len(descriptors) == 32 and len(descriptors) <= MAX_FDS,'normal-descriptor-membership')
    need(type(capture_rows) is list and len(capture_rows) == 24,'normal-capture-membership')
    names=fixed_captures()
    need(type(captures) is dict and len(captures) == 24 and set(captures) == set(names),'fixed-capture-membership')
    need(all(type(captures[name]) is bytes and len(captures[name]) <= STREAM_BYTES for name in names)
         and sum(len(captures[name]) for name in names) <= TOTAL_STREAM_BYTES,'capture-byte-cap')
    fds={}
    expected_kinds=['capture-'+stream for unused in range(9) for stream in ('stdout','stderr')]
    expected_kinds += ['directory-native','directory-private','private-auth','ready-read','ready-parent-write',
                       'capture-stdout','capture-stderr','private-auth','ready-read','ready-parent-write',
                       'capture-stdout','capture-stderr','capture-stdout','capture-stderr']
    for index,row in enumerate(descriptors,1):
        descriptor(row)
        need(row['id'] == 'supervisor-fd-'+str(index) and row['kind'] == expected_kinds[index-1],'descriptor-id-kind')
        fds[row['id']]=row
        if row['kind'].startswith('capture-'):
            need(row['eof'] is True,'capture-EOF')
        elif row['kind'] == 'ready-read':
            need(row['eof'] is True and 0 < row['bytes'] <= READY_BYTES,'ready-EOF-count')
        else:
            need(row['eof'] is False and row['bytes'] == 0,'nonreader-descriptor-count')
    # Record IDs stay unique across history; numeric FDs may be reused after
    # close. These acquisition sets coexist without an intervening pump close.
    # Prior-service captures are excluded: ready()/pump() may have closed them
    # on EOF before a later acquisition, and the DTO has no close-time facts.
    co_live=[(index*2-1,index*2) for index in range(1,10)]
    co_live += [(19,20),(19,20,21),(19,20,22,23,24,25),
                (19,20,26),(19,20,27,28,29,30),(19,20,31,32)]
    for phase in co_live:
        numbers=[fds['supervisor-fd-'+str(index)]['fd'] for index in phase]
        need(len(numbers) == len(set(numbers)),'co-live-descriptor-alias')
    captures_by_path={}
    for name,row in zip(names,capture_rows):
        keys(row,CAPTURE,'capture-shape')
        need(row['path'] == paths['evidence']+'/'+name and row['closed'] is True and row['error'] is None
             and integer(row['bytes']) and row['bytes'] == len(captures[name]),'capture-path-writer-bytes')
        captures_by_path[name]=row
    directory_rows=record['directories']
    need(type(directory_rows) is list and len(directory_rows) == 2,'directory-membership')
    for index,(kind,fd_index) in enumerate((('native',19),('private',20)),1):
        row=directory_rows[index-1];keys(row,DIRECTORY,'directory-shape')
        need(row['id'] == 'directory-'+str(index) and row['kind'] == kind
             and all(row[key] is True for key in ('attempted','created','validated'))
             and row['error'] is None and row['fd_id'] == 'supervisor-fd-'+str(fd_index),'directory-acquisition-FD')
        path_text(row['path']);keys(row['identity'],'device inode uid mode directory','directory-identity-shape')
        observed=row['identity']
        need(integer(observed['device']) and integer(observed['inode'],1) and integer(observed['uid'])
             and observed['uid'] == owner_uid and type(observed['mode']) is int and observed['mode'] == 0o700
             and observed['directory'] is True,'directory-identity-types')
        need(row['removed'] is (kind == 'private'),'directory-retention-removal')
        if kind == 'native':need(row['path'] == paths['evidence']+'/native','native-fixed-directory')
        else:
            need(row['path'].startswith('/tmp/h2018.x.') and row['path'][len('/tmp/h2018.x.'):]
                 and '/' not in row['path'][len('/tmp/h2018.x.'):] and len(row['path']+'/bus') < 100
                 and not row['path'].startswith(paths['evidence']+'/') and not paths['evidence'].startswith(row['path']+'/')
                 and row['path'] != paths['evidence'],'private-directory-data')
    need(directory_rows[0]['identity'] != directory_rows[1]['identity'],'directory-generation-alias')
    private=directory_rows[1]['path']
    stream_fds=[];generations=set();pids=[]
    for index,row in enumerate(roots,1):
        role='metadata' if index <= 9 else ('Xvfb','bus','driver')[index-10]
        fields=ROOT+(' ready' if role in ('Xvfb','bus') else '')
        keys(row,fields,'root-shape')
        need(row['id'] == 'supervisor-root-'+str(index) and row['role'] == role
             and integer(row['attempted_ns'],1) and row['returned'] is True and row['published'] is True
             and row['error'] is None and type(row['forced']) is bool,'root-creation')
        instance(row['identity'],True)
        need(type(row['pid']) is int and row['pid'] == row['identity']['pid'],'root-original-PID')
        generation=(row['pid'],row['identity']['stamp'])
        need(generation not in generations,'root-generation-reuse');generations.add(generation);pids.append(row['pid'])
        # The normal source awaits metadata/driver once for work and again
        # during finish; X/bus are each awaited once during finish. A prior
        # wait error would have latched a diagnostic and is never clean here.
        expected_waits=2 if role in ('metadata','driver') else 1
        need(type(row['waits']) is list and len(row['waits']) == expected_waits,'normal-root-wait-count')
        for waited in row['waits']:
            wait_data(waited)
            need(waited['error'] is None,'normal-root-wait-error: '+str(waited['error']))
        wait_data(row['terminal'])
        need(row['terminal'] == row['waits'][-1] and row['terminal']['error'] is None,'root-terminal-last-wait')
        need(all(waited['exit_code'] == row['terminal']['exit_code'] for waited in row['waits']),'original-wait-outcome-contradiction')
        # System.stamp is time.time_ns: positive scalar observations and list
        # position are retained; no within/across-record clock order is inferred.
        need(type(row['signals']) is list and len(row['signals']) <= 4096,'root-signals')
        returned_term=returned_kill=False
        for sent in row['signals']:
            keys(sent,'signal returned outcome error','signal-shape')
            need(type(sent['signal']) is int and sent['signal'] in (15,9) and type(sent['returned']) is bool,'signal-types')
            if sent['returned']:
                need(sent['outcome'] == 'returned' and sent['error'] is None,'signal-return')
                returned_term |= sent['signal'] == 15;returned_kill |= sent['signal'] == 9
            else:
                need(sent['outcome'] in ('attempted','ESRCH','error')
                     and ((sent['outcome'] == 'attempted' and sent['error'] is None)
                          or (sent['outcome'] != 'attempted' and text(sent['error']))),'signal-negative')
                need(False,'signal-send-unresolved')
        need(row['forced'] is returned_kill and not row['forced'],'forced-root')
        code=row['terminal']['exit_code']
        need(code != -9 and (code == 0 or role in ('Xvfb','bus') and code == -15 and returned_term),'root-terminal-policy')
        if role == 'metadata':
            checkout=paths[('control','before','after')[(index-1)//3]]
            operation=(index-1)%3
            argv=['git','-C',checkout,*([['rev-parse','HEAD'],['rev-parse','HEAD^{tree}'],['status','--porcelain=v1']][operation])]
            expected_text=expected_source[('control','before','product')[(index-1)//3]][('sha','tree')[operation]] if operation < 2 else ''
            raw=captures['supervisor/'+row['id']+'.stdout.log']
            need(len(raw) <= 16384 and raw.decode().strip() == expected_text,'metadata-output-cleanliness')
            cwd=None
        elif role == 'Xvfb':
            argv=['/usr/bin/Xvfb','-displayfd',str(fds['supervisor-fd-23']['fd']),'-screen','0','1600x1000x24',
                  '-nolisten','tcp','-dpi','96','-auth',private+'/server.Xauthority'];cwd=None
        elif role == 'bus':
            argv=['/usr/bin/dbus-daemon','--session','--nofork','--nopidfile','--nosyslog',
                  '--address=unix:path='+private+'/bus','--print-address='+str(fds['supervisor-fd-28']['fd'])];cwd=None
        else:
            argv=['/usr/bin/python3',paths['control']+'/.github/qualification/final-main-native.py',paths['control'],paths['before'],paths['after']]
            cwd=paths['control']
        need(type(row['argv']) is list and len(row['argv']) == len(argv) and all(type(arg) is str for arg in row['argv'])
             and row['argv'] == argv and type(row['cwd']) is type(cwd) and row['cwd'] == cwd,'root-exact-argv-cwd')
        need(type(row['streams']) is list and len(row['streams']) == 2,'root-stream-pair')
        fd_indices=(index*2-1,index*2) if index <= 9 else ((24,25),(29,30),(31,32))[index-10]
        for stream,stream_row,fd_index in zip(('stdout','stderr'),row['streams'],fd_indices):
            descriptor(stream_row)
            need(stream_row['id'] == 'supervisor-fd-'+str(fd_index) and stream_row == fds[stream_row['id']]
                 and stream_row['kind'] == 'capture-'+stream,'root-descriptor-reciprocal')
            name='supervisor/'+row['id']+'.'+stream+'.log'
            need(stream_row['bytes'] == captures_by_path[name]['bytes'],'root-capture-count')
            stream_fds.append(stream_row['id'])
        if role in ('Xvfb','bus'):
            ready=row['ready'];keys(ready,'bytes sha256 eof meaning','ready-shape')
            descriptor_id='supervisor-fd-'+str(22 if role == 'Xvfb' else 27)
            need(integer(ready['bytes'],1) and ready['bytes'] == fds[descriptor_id]['bytes'] and ready['eof'] is True
                 and hex_text(ready['sha256'],64) and ready['meaning'] == 'Original launch notification only; no authenticated client readiness claim.','ready-association')
    need(len(stream_fds) == len(set(stream_fds)) == 24,'root-stream-unique-IDs')
    kernel=record['kernel'];keys(kernel,'waits echild errors','kernel-shape')
    strings(kernel['errors'],'kernel-errors')
    need(type(kernel['echild']) is bool and type(kernel['waits']) is list and len(kernel['waits']) <= MAX_ADOPTED,'kernel-types')
    for waited in kernel['waits']:
        keys(waited,'pid raw_status exit_code','kernel-wait-shape')
        need(integer(waited['pid'],1) and integer(waited['raw_status']) and type(waited['exit_code']) is int,'kernel-wait-types')
        raw=waited['raw_status'];code=-(raw&127) if raw&127 else (raw>>8)&255
        need(waited['exit_code'] == code and code != -9 and waited['pid'] not in pids,'kernel-original-status')
    need(kernel['echild'] and not kernel['errors'],'kernel-ECHILD-UNKNOWN')
    need(not record['errors'] and record['first_diagnostic'] is None and record['cancel_signal'] is None,'supervisor-work-negative')
    keys(driver_inputs,'ledger source arm_cleanups mounts','driver-data-inputs')
    need(type(expected_driver) is dict and driver_check_source_sha256 == expected_source['supervisor_sha256']
         and callable(driver_check),'driver-check-source-bound-callback')
    joins=dict(product=expected_source['product'],before=expected_source['before'],control_sha=expected_source['control']['sha'],
        control_tree=expected_source['control']['tree'],driver_sha256=expected_source['driver_sha256'],
        preparation_module_sha256=expected_source['preparation_module_sha256'],preparation_sha256=expected_source['prepared_sha256'],
        run=expected_source['run'],attempt=expected_source['attempt'],logical_job=expected_source['logical_job'],numeric_job_id=None)
    need(all(key in expected_driver and type(expected_driver[key]) is type(value) and expected_driver[key] == value
             for key,value in joins.items()),'driver-supervisor-source-join')
    driver=roots[-1]
    captured=[dict(eof=row['eof'],closed=row['closed'],error=row['error'],capture_closed=captures_by_path['supervisor/'+driver['id']+'.'+stream+'.log']['closed'])
              for stream,row in zip(('stdout','stderr'),driver['streams'])]
    fact=driver_check(driver_inputs['ledger'],driver_inputs['source'],expected_driver,driver['identity'],driver['terminal'],captured,
                      driver_inputs['arm_cleanups'],driver_inputs['mounts'],supervisor_instance['pid'])
    driver_fact(fact);driver_fact(record['driver_ledger'])
    need(fact == record['driver_ledger'],'copied-driver-fact-mismatch')
    need(fact['local_settled'] and fact['work_outcome_ok'] and fact['entered_arms'] == ['before','after']
         and not fact['errors'] and fact['first_driver_diagnostic'] is None and fact['requested_driver_cancel'] is None,'driver-work-unsettled')
    return {name:dict(sha256=digest(captures[name]),bytes=len(captures[name])) for name in names}


def check_local(record, expected_source, paths, owner_uid, supervisor_instance,
                captures, driver_inputs, expected_driver, driver_check, driver_check_source_sha256):
    """A consistent local record still ALWAYS returns HOLD; no witness override."""
    result=dict(kind='held-scope-local-settlement-consistency-v1',internal_consistency=False,diagnostics=[],
        recorded_first_diagnostic=None,recorded_cancel_signal=None,recorded_errors=[],capture_digests={},
        qualified=False,retained_evidence_verified_here=False,can_run_broad=False,admission='HOLD',unknown=list(UNKNOWN))
    if type(record) is dict:
        if record.get('first_diagnostic') is None or text(record.get('first_diagnostic')):
            result['recorded_first_diagnostic']=record.get('first_diagnostic')
        if record.get('cancel_signal') is None or type(record.get('cancel_signal')) is int:
            result['recorded_cancel_signal']=record.get('cancel_signal')
        if type(record.get('errors')) is list:
            result['recorded_errors']=[item for item in record['errors'][:4096] if text(item)]
    try:
        result['capture_digests']=validate_local(record,expected_source,paths,owner_uid,supervisor_instance,
            captures,driver_inputs,expected_driver,driver_check,driver_check_source_sha256)
        result['internal_consistency']=True
    except Exception as error:
        result['diagnostics'].append(type(error).__name__+': '+str(error)[:65536])
    return result
