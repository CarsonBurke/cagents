#!/usr/bin/python3
"""Offline SDK fixture derived from retained_actor_tests::install_provider.

Source input only until an owning native runner executes it. One actual UI
prompt, exact explicit account, no network, no subprocess or signals.
This derivative also accepts a real named --resume initialization. Its first
resume exits before work exactly once, so the native Failed-card Retry must
recover through normal daemon lifecycle and admission. The marker is SDK
fixture diagnostics, never an execution proof or application receipt.
"""
import json, os, pathlib, sys, time, uuid, stat
os.umask(0o077)
scriptroot=pathlib.Path(__file__).resolve().parent.parent
tracepath=scriptroot/'provider-trace.jsonl'
def trace(value):
    # Fixed allowlist below; never record prompt bodies, argv, or the environment.
    data=(json.dumps(value,ensure_ascii=True)[:2047]+'\n').encode()
    try:
        with tracepath.open('ab') as log:
            if log.tell()+len(data)<=8192: log.write(data)
    except OSError:
        pass  # Diagnostic I/O must not change the provider's protocol behavior.
trace({'event':'bootstrap','HOME':os.environ.get('HOME'),'CLAUDE_CONFIG_DIR':os.environ.get('CLAUDE_CONFIG_DIR'),'cwd':os.getcwd(),'scriptroot':str(scriptroot)})
root=scriptroot
assert root.name.startswith('updater1513-') and root.stat().st_uid==os.getuid()
assert stat.S_IMODE(root.stat().st_mode)==0o700
if sys.argv[1:2]==['--version']:
    print('2.1.286 (offline updater1513 fixture)'); raise SystemExit
if sys.argv[1:2]==['auth']:
    print(json.dumps(dict(loggedIn=True,email='fixture@example.test',subscriptionType='pro'))); raise SystemExit

account=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']).resolve(strict=True)
assert account in [(root/'fixture-inputs/workload'/name).resolve(strict=True) for name in ('account-alpha','account-beta')]
assert pathlib.Path.cwd().resolve()==(root/'fixture-inputs/workload/project').resolve(strict=True)

trace({'event':'fixture-owner','pid':os.getpid(),'parent':os.getppid(),'account':str(account),'markerroot':str(root)})
def argument(name):
    values = []
    for index, arg in enumerate(sys.argv[1:], 1):
        if arg.startswith(name + '='):
            values.append(arg.split('=', 1)[1])
        elif arg == name:
            assert index + 1 < len(sys.argv), 'missing named SDK argument'
            values.append(sys.argv[index + 1])
    assert len(values) <= 1, 'duplicate SDK conversation argument'
    assert not values or values[0], 'empty named SDK conversation'
    return values[0] if values else None
resume = argument('--resume')
new_session = argument('--session-id')
assert resume is None or new_session is None, 'ambiguous SDK conversation'
session = resume or new_session or str(uuid.uuid4())
assert str(uuid.UUID(session)) == session, 'exact canonical conversation required'

project_key=''.join(c if c.isascii() and c.isalnum() else '-' for c in os.getcwd())
history=account/'projects'/project_key/(session+'.jsonl')
parent=None
written=0
def send(value):
    global parent,written
    if value.get('type') in ('user','assistant','result'):
        history.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
        row=dict(value,sessionId=session,parentUuid=parent)
        raw=json.dumps(row,ensure_ascii=False)+'\n'
        written+=len(raw.encode()); assert written<=4*1024*1024
        with history.open('a') as log: log.write(raw)
        parent=value.get('uuid',parent)
    print(json.dumps(value),flush=True)
if resume:
    # The normal initial UI prompt created this history and settled result.
    # No model rows, accounts, history bytes, authority or receipts are seeded.
    assert (root/'provider-done').read_text() == 'done'
    fd = os.open(history, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        row = os.fstat(fd)
        assert stat.S_ISREG(row.st_mode) and row.st_uid == os.getuid() and row.st_nlink == 1
        assert 0 < row.st_size <= 4*1024*1024
    finally:
        os.close(fd)
    marker = root/'history-retry-start-failed-once.json'
    payload = json.dumps({'thread': session, 'account': str(account)}, sort_keys=True).encode()
    try:
        fd = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except FileExistsError:
        fd = os.open(marker, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            row = os.fstat(fd)
            assert stat.S_ISREG(row.st_mode) and row.st_uid == os.getuid() and row.st_nlink == 1
            assert stat.S_IMODE(row.st_mode) == 0o600 and row.st_size == len(payload)
            assert os.read(fd, len(payload)+1) == payload, 'retry changed exact account/thread'
        finally:
            os.close(fd)
    else:
        try:
            assert os.write(fd, payload) == len(payload)
        finally:
            os.close(fd)
        trace({'event': 'owned-fixture-resume-refused-once', 'thread': session})
        raise SystemExit(75)
    # Same supported no-prompt init pattern as the executed119/d95 fixture.
    # The host reads exact saved history normally; the SDK never rewrites it.
    send({'type':'system','subtype':'init','session_id':session})
for line in sys.stdin:
    frame=json.loads(line)
    trace({'event':'input','type':str(frame.get('type'))[:96],'uuid':str(frame.get('uuid'))[:96]})
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    elif frame.get('type')=='user':
        assert not (root/'prompts').exists(), 'unexpected second prompt or replacement provider'
        assert 'LIVE_WORK_1513' in json.dumps(frame.get('message',{})), 'unexpected fixture prompt'
        with (root/'prompts').open('a') as log: log.write('prompt\n')
        with (root/'prompt-bodies').open('a') as log: log.write(json.dumps(frame.get('message',{}))+'\n')
        received=frame['uuid']
        send({'type':'system','subtype':'init','session_id':session})
        send({'type':'user','uuid':received,'session_id':session,'parent_tool_use_id':None,'message':{'role':'user','content':[{'type':'text','text':'LIVE_WORK_1513'}]}})
        send({'type':'command_lifecycle','command_uuid':received,'state':'started','session_id':session,'uuid':'lifecycle-'+received})
        for index in range(40):
            send({'type':'assistant','uuid':str(uuid.uuid5(uuid.UUID(session), f'frame-{index}')),'message':{'id':str(uuid.uuid5(uuid.UUID(session), f'frame-{index}')),'role':'assistant','content':[{'type':'text','text':f'FRAME_{index:03}|'}]}})
            with (root/'produced').open('a') as log: log.write(str(index)+'\n')
            gate=root/'lease-third-frame-gate'
            if index==2 and gate.exists():
                deadline=time.monotonic()+300
                while gate.exists() and not (root/'lease-third-frame-release').exists():
                    if time.monotonic()>=deadline:
                        trace({'event':'third-frame-gate-timeout'})
                        raise RuntimeError('private fixture third-frame gate timed out')
                    time.sleep(0.02)
            time.sleep(0.04)
        send({'type':'result','uuid':str(uuid.uuid5(uuid.UUID(session), 'final')),'subtype':'success','is_error':False,'session_id':session,'usage':{'input_tokens':7,'output_tokens':40}})
        (root/'provider-done').write_text('done')
