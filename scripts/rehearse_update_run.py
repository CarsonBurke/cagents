#!/usr/bin/python3
"""Scenario orchestration for rehearse-update; all application commands use its private env."""
import argparse
from collections import Counter
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tarfile
import time
import tomllib
import uuid


def wait_native_lock(root, descriptor, parent_pid):
    """Keep one blocking waiter; cancellation terminates and joins only it."""
    root=Path(root).resolve()
    if os.getppid()!=parent_pid:
        return 75
    marker=root/'.rehearse-update'
    if marker.is_symlink() or marker.read_text().strip()!=str(root) or descriptor<3:
        raise RuntimeError('native lock waiting requires an owned private root and inherited descriptor')
    stop=root/'desktop/stop'
    if stop.exists():
        return 75
    waiter=None
    pidfd=None
    previous={}
    def interrupted(signum, frame):
        raise SystemExit(128+signum)
    try:
        for signum in (signal.SIGTERM,signal.SIGINT,signal.SIGHUP):
            previous[signum]=signal.signal(signum,interrupted)
        # Do not let the child queue for the lock until its pidfd is secured.
        # If pidfd setup fails, closing stdin makes the child exit without
        # ever running flock; parent death also terminates it in the kernel.
        gate=('import os,sys\nif os.getppid()!=int(sys.argv[2]): sys.exit(75)\n'
              'if os.read(0,1)!=b"1": sys.exit(0)\nos.execlp("flock","flock",sys.argv[1])\n')
        waiter=subprocess.Popen(['setpriv','--pdeathsig','TERM',sys.executable,'-c',gate,str(descriptor),str(os.getpid())],
                                pass_fds=(descriptor,),stdin=subprocess.PIPE,stdout=subprocess.DEVNULL)
        try:
            pidfd=os.pidfd_open(waiter.pid)
        except ProcessLookupError:
            return 75 if stop.exists() else waiter.wait()
        waiter.stdin.write(b'1')
        waiter.stdin.close()
        while waiter.poll() is None:
            if stop.exists():
                return 75
            select.select([pidfd],[],[],.1)
        return 75 if stop.exists() else waiter.wait()
    finally:
        # A second cancellation must not interrupt the child's join.
        blocked=signal.pthread_sigmask(signal.SIG_BLOCK,previous)
        try:
            if waiter is not None:
                if waiter.stdin is not None and not waiter.stdin.closed:
                    try:
                        waiter.stdin.close()
                    except BrokenPipeError:
                        pass
                if waiter.poll() is None and pidfd is not None:
                    try:
                        signal.pidfd_send_signal(pidfd,signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                waiter.wait()
            if pidfd is not None:
                os.close(pidfd)
        finally:
            for signum,handler in previous.items():
                signal.signal(signum,handler)
            signal.pthread_sigmask(signal.SIG_SETMASK,blocked)


def candidate_namespace_cli(root, archive_dir, version):
    """Extract only the verified candidate CLI into the marked private root."""
    root=Path(root).resolve()
    if (root/'.rehearse-update').is_symlink() or (root/'.rehearse-update').read_text().strip()!=str(root):
        raise RuntimeError('candidate probe requires a matching rehearsal root')
    if not version or version in ('.','..') or '/' in version:
        raise RuntimeError('candidate probe requires a single version name')
    archive_dir=Path(archive_dir)
    name=f'cagents-{version}-x86_64-unknown-linux-gnu.tar.gz'
    source=archive_dir/name
    with source.open('rb') as stream:
        digest=hashlib.file_digest(stream,'sha256').hexdigest()
    expected=[line.split()[0] for line in (archive_dir/'SHA256SUMS').read_text().splitlines()
              if len(line.split())==2 and line.split()[1].lstrip('*')==name]
    if expected!=[digest]:
        raise RuntimeError('candidate probe requires the checksum-verified archive')
    with tarfile.open(source,'r:gz') as archive:
        members=[member for member in archive.getmembers() if member.name==f'cagents-{version}/bin/cagents']
        if len(members)!=1 or not members[0].isreg() or not 0<members[0].size<=128*1024**2:
            raise RuntimeError('candidate probe requires one bounded regular CLI')
        destination=root/'candidate-namespace'
        destination.mkdir(mode=0o700)
        cli=destination/'cagents'
        with archive.extractfile(members[0]) as stream, cli.open('xb') as output:
            shutil.copyfileobj(stream,output)
        cli.chmod(0o700)
        with cli.open('rb') as stream:
            cli_digest=hashlib.file_digest(stream,'sha256').hexdigest()
        manifest=destination/'archive.json'
        manifest.write_text(json.dumps({'archive':str(source.resolve()),'archive_sha256':digest,
                                       'member':members[0].name,'cli_sha256':cli_digest},indent=2,sort_keys=True)+'\n')
        manifest.chmod(0o600)
    return cli


def candidate_namespace_contract(evidence, old_version, new_version):
    source=evidence['source']
    expected=source.get('data',{})
    snapshot=evidence['source_snapshot']
    root=Path(snapshot['root'])
    version_root=root/'home/.local/share/cagents-install/versions'/old_version
    owner=[row for row in snapshot['processes'] if row.get('executable')==str(version_root/'bin/cagentsd')
           and row.get('argv')==[str(version_root/'bin/cagentsd')] and row.get('pid',0)>1
           and row.get('start_ticks',0)>0]
    daemons=[row for row in snapshot['processes'] if Path(row.get('executable','')).name=='cagentsd'
             and row.get('argv',[])[:1]==[row['executable']]]
    census=evidence['census']
    receipt=evidence['receipt']
    proof=snapshot.get('serving')
    outcome=receipt.get('outcome',{})
    if (source.get('ok') is not True or expected.get('running') is not True
            or expected.get('instance')!='production' or expected.get('daemon_version')!=old_version
            or expected.get('socket')!=str(root/'rt/cagents/cagentsd.sock')
            or evidence['candidate_version'].strip()!=f'cagents {new_version}'
            or snapshot['errors'] or len(owner)!=1 or daemons!=owner
            or not isinstance(proof,dict) or any(not isinstance(proof.get(field),str) or not proof[field]
                for field in ('host_session_id','boot_id','host_incarnation'))
            or census.get('source')!=proof or outcome.get('state')!='usable' or outcome.get('serving')!=proof
            or census.get('transaction')!=receipt.get('transaction')
            or evidence['census_digest']!=receipt.get('census_digest')):
        raise RuntimeError('candidate does not observe the serving production baseline in the same namespace')
    # A strict current-protocol status may reject a qualified frozen source.
    # It proves neither absence nor presence; the successful read-only census
    # and require-original verification above bind the actual Ready socket.
    status=evidence['candidate_status']
    if status['exit']==0:
        actual=status['payload'].get('data',{})
        if (status['payload'].get('ok') is not True or actual.get('running') is not True
                or actual.get('instance')!='production' or actual.get('daemon_version')!=old_version
                or actual.get('socket')!=expected['socket'] or actual.get('cli_version')!=new_version):
            raise RuntimeError('candidate status disagrees with the serving production baseline')


def provider_activity(root):
    root=Path(root)
    claude=(root/'home/.claude/fixture-launches').read_text().splitlines()
    claude_prompts=0
    for path in (root/'home/.claude/projects').glob('*/*.jsonl'):
        if path.is_symlink() or not path.is_file() or path.stat().st_size>16*1024**2:
            raise RuntimeError('unsafe or oversized private Claude fixture transcript')
        for line in path.read_text().splitlines():
            try:
                record=json.loads(line)
            except ValueError:
                continue
            if not isinstance(record,dict):
                continue
            origin=record.get('origin')
            if (record.get('type')!='user'
                    or (isinstance(origin,dict) and origin.get('kind')=='task-notification')
                    or any(record.get(flag) is True for flag in ('isMeta','isCompactSummary','repeatsCompactSummary'))):
                continue
            message=record.get('message')
            content=message.get('content') if isinstance(message,dict) else None
            if isinstance(content,str) or (isinstance(content,list) and any(
                    isinstance(part,dict) and part.get('type')!='tool_result' for part in content)):
                claude_prompts+=1
    events=[json.loads(line) for line in (root/'logs/codex-provider.jsonl').read_text().splitlines()]
    methods=Counter(event.get('method') for event in events)
    listeners=[event for event in events if event.get('event')=='launch'
               and any(arg=='--listen' or arg.startswith('--listen=') for arg in event.get('argv',[]))]
    conversations={event['pid'] for event in listeners}
    conversations.update(event['pid'] for event in events if event.get('method') in
        ('thread/start','thread/resume','turn/start','turn/interrupt'))
    launches=sum(event.get('event')=='launch' and event['pid'] in conversations for event in events)
    return {'claude_chat_launches':len(claude),'claude_user_prompts':claude_prompts,'codex_chat_launches':launches,
            'codex_thread_starts':methods['thread/start'],'codex_turn_starts':methods['turn/start']}


def terminal_shortcut(root):
    path=Path(root)/'home/.config/cagents/settings.toml'
    raw=path.read_bytes()
    configured=tomllib.loads(raw.decode()).get('keyboard_shortcuts',{}).get('new_terminal')
    if not isinstance(configured,list) or not configured or not isinstance(configured[0],str):
        raise RuntimeError('the actual private New terminal key binding is unavailable')
    chord=configured[0].lower()
    if not re.fullmatch(r'(?:(?:ctrl|shift|alt|super)\+)+(?:[a-z0-9]|f(?:[1-9]|1[0-9]|2[0-4]))',chord):
        raise RuntimeError('the actual private New terminal key binding is unsupported by this rehearsal')
    return {'path':str(path),'sha256':hashlib.sha256(raw).hexdigest(),'configured':configured,'chord':chord}


def terminal_cards(root, slot='main'):
    data=Path(root)/'home/.local/share/cagents'
    database=data/'sessions.sqlite3' if slot=='main' else data/'windows'/slot/'sessions.sqlite3'
    with closing(sqlite3.connect(database.as_uri()+'?mode=ro',uri=True,timeout=1)) as connection:
        connection.execute('PRAGMA query_only=ON')
        connection.execute('BEGIN')
        return [{'database':str(database),'card':row[0],'workspace':row[1],'focused':row[2]==row[0],
                 'terminal_id':row[3],'recipe':json.loads(row[4]),'continuity':json.loads(row[5])}
                for row in connection.execute(
                    "SELECT c.card_id,c.workspace_id,w.focused_card_id,r.terminal_id,r.recipe_json,r.continuity_json "
                    "FROM cards c JOIN workspaces w ON w.workspace_id=c.workspace_id "
                    "JOIN terminal_recipes r ON r.card_id=c.card_id "
                    "WHERE c.kind_tag='terminal' AND c.existence_json=?",[json.dumps('open')])]


def terminal_probe_command(session, token, receipt):
    shell=Path(session['launch']['program']).name
    if shell=='fish':
        variable='$fish_pid'
    elif shell in ('sh','bash','dash','zsh','ksh'):
        variable='$$'
    else:
        raise RuntimeError(f'the issued terminal shell is unsupported by the output probe: {shell}')
    return {'program':session['launch']['program'],'pid_variable':variable,
            'command':f"umask 077; printf '{token} pid=%s\\n' \"{variable}\" | tee {shlex.quote(str(receipt))}\n"}


def terminal_live_proof(continuity):
    if not isinstance(continuity,dict) or len(continuity)!=1:
        return None
    proof=continuity.get('live',continuity.get('detached_same_boot'))
    if not isinstance(proof,dict) or any(not isinstance(proof.get(field),str) or not proof[field]
            for field in ('host_session_id','boot_id','host_incarnation')):
        return None
    return proof


def same_serving_terminal(left, right, proof):
    return (terminal_live_proof(left.get('continuity'))==proof
            and terminal_live_proof(right.get('continuity'))==proof
            and {**left,'continuity':{'live':proof}}=={**right,'continuity':{'live':proof}})


def new_terminal_action_contract(evidence, version_root):
    version_root=Path(version_root)
    def ui(snapshot):
        return {(row['pid'],row['start_ticks']) for row in snapshot['processes']
                if row.get('executable')==str(version_root/'bin/cagents-ui')}
    original=ui(evidence['before_processes'])
    if (evidence['before_processes']['errors'] or evidence['after_processes']['errors']
            or len(original)!=2 or original!=ui(evidence['after_processes'])
            or evidence['before_windows']!=evidence['after_windows']):
        raise RuntimeError('the normal user action did not remain in the two relaunched UI processes')
    status=evidence['after_status'].get('data',{})
    show=evidence['terminal'].get('data',{})
    terminal=show.get('terminal',{}).get('session',{})
    proof=show.get('proof')
    receipt=evidence['receipt']
    outcome=receipt.get('outcome',{})
    census=evidence['census']
    if (not isinstance(proof,dict) or proof==evidence['before_serving']
            or any(not proof.get(field) for field in ('host_session_id','boot_id','host_incarnation'))
            or evidence['after_status'].get('ok') is not True or status.get('running') is not True
            or status.get('daemon_version')!=version_root.name or status.get('instance')!='production'
            or status.get('socket')!=str(Path(evidence['after_processes']['root'])/'rt/cagents/cagentsd.sock')
            or census.get('source')!=proof or outcome.get('state')!='usable' or outcome.get('serving')!=proof
            or receipt.get('transaction')!=census.get('transaction')
            or receipt.get('census_digest')!=evidence['census_digest']
            or not any(same_serving_terminal(terminal,row,proof) for row in census.get('terminals',[]))):
        raise RuntimeError('the new terminal has no verified Ready daemon and exact live descriptor')
    card=evidence['selected_card']
    if (not card['focused'] or card['card'] in evidence['cards_before_input']
            or card['terminal_id']!=terminal.get('id') or card['recipe']!=terminal.get('launch')
            or terminal_live_proof(card['continuity'])!=proof or terminal_live_proof(terminal.get('continuity'))!=proof):
        raise RuntimeError('the native user action did not focus a newly issued terminal card')
    line=evidence['output_line']
    snapshot=evidence['terminal_snapshot'].get('data',{}).get('snapshot',{})
    if (snapshot.get('terminal_id')!=terminal.get('id') or line not in snapshot.get('lines',[])
            or line!=f"{evidence['token']} pid={evidence['shell_pid']}"):
        raise RuntimeError('the typed shell command did not produce the exact terminal output')
    shells=[row for row in evidence['after_processes']['processes']
            if row['pid']==evidence['shell_pid'] and row.get('start_ticks',0)>0
            and row.get('terminal_id')==terminal['id']
            and row.get('executable')==str(Path(terminal['launch']['program']).resolve())]
    daemons=[row for row in evidence['after_processes']['processes']
             if row.get('executable')==str(version_root/'bin/cagentsd') and row.get('argv')==[row['executable']]]
    if len(shells)!=1 or len(daemons)!=1:
        raise RuntimeError('the user terminal shell or its matched daemon lacks kernel identity evidence')
    for field in ('claude_user_prompts','codex_thread_starts','codex_turn_starts'):
        if evidence['activity_before'][field]!=evidence['activity_after'][field]:
            raise RuntimeError('a provider conversation or prompt was replayed during the normal terminal action')
    return {'shell':shells[0],'daemon':daemons[0],'serving':proof,'terminal_id':terminal['id']}


def terminal_restart_failure_contract(evidence, version_root):
    """Only the observed old-policy failure qualifies as a before example."""
    version_root=Path(version_root)
    def ui(snapshot):
        return {(row['pid'],row['start_ticks']) for row in snapshot['processes']
                if row.get('executable')==str(version_root/'bin/cagents-ui')}
    original=ui(evidence['before_processes'])
    status=evidence['after_status'].get('data',{})
    card=evidence['selected_card']
    daemons=[row for row in evidence['after_processes']['processes']
             if Path(row.get('executable','')).name=='cagentsd' and row.get('argv')==[row['executable']]]
    if (evidence['before_processes']['errors'] or evidence['after_processes']['errors']
            or len(original)!=2 or original!=ui(evidence['after_processes'])
            or evidence['before_windows']!=evidence['after_windows']
            or evidence['after_status'].get('ok') is not True or status.get('running') is not False
            or status.get('daemon_present') is not False or status.get('instance')!='production'
            or status.get('socket')!=str(Path(evidence['after_processes']['root'])/'rt/cagents/cagentsd.sock')
            or daemons or any(row.get('terminal_id')==card['terminal_id'] for row in evidence['after_processes']['processes'])
            or not card['focused'] or card['card'] in evidence['cards_before_input']
            or evidence['failure_observation'].get('window',{}).get('id')!=evidence['before_windows']['main']):
        raise RuntimeError('the before example did not prove a failed new terminal in the same native window')
    if evidence.get('failure_kind')=='pending_without_daemon':
        samples=evidence.get('pending_observations',[])
        if (len(samples)<2 or samples[-1]['monotonic']-samples[0]['monotonic']<3
                or not all(pending_terminal_failure(sample,card,evidence['before_windows'],original,version_root) for sample in samples)):
            raise RuntimeError('the pending terminal example lacks bounded native and typed absence observations')
    else:
        diagnostic=evidence.get('failure_diagnostic','')
        fields=dict(line.split(': ',1) for line in diagnostic.splitlines() if ': ' in line)
        if (len(diagnostic)>16384 or not fields.get('Code') or not fields.get('Message')
                or fields.get('Terminal')!=card['terminal_id']):
            raise RuntimeError('the before example has no exact terminal error diagnostic')
    for field in ('claude_user_prompts','codex_thread_starts','codex_turn_starts'):
        if evidence['activity_before'][field]!=evidence['activity_after'][field]:
            raise RuntimeError('the before example replayed a provider conversation or prompt')
    return {'outcome':'expected_terminal_restart_failure','observed_state':evidence.get('failure_kind','runtime_error'),
            'observed_card':card,'status':evidence['after_status']}


def pending_terminal_failure(sample, card, windows, ui_identities, version_root):
    """Recognize the observed old-policy spinner, never an arbitrary timeout."""
    observed=sample['observation']
    text=[' '.join(run.get('text','').casefold().split()) for run in observed.get('runs',[])]
    status=sample['status'].get('data',{})
    snapshot=sample['snapshot_refusal']
    processes=sample['processes']
    uis={(row['pid'],row['start_ticks']) for row in processes['processes']
         if row.get('executable')==str(Path(version_root)/'bin/cagents-ui')}
    daemons=[row for row in processes['processes'] if Path(row.get('executable','')).name=='cagentsd'
             and row.get('argv')==[row['executable']]]
    return (sample['card']==card and sample['windows']==windows and uis==ui_identities and not processes['errors']
            and observed.get('window',{}).get('id')==windows['main']
            and any('starting terminal' in line for line in text)
            and any('unreachable' in line for line in text) and any('daemon' in line for line in text)
            and sample['snapshot_terminal_id']==card['terminal_id'] and sample['snapshot_exit']!=0
            and snapshot.get('ok') is False and snapshot.get('error',{}).get('kind')=='daemon_not_running'
            and sample['status'].get('ok') is True and status.get('running') is False
            and status.get('daemon_present') is False and status.get('instance')=='production'
            and status.get('socket')==str(Path(processes['root'])/'rt/cagents/cagentsd.sock')
            and not daemons and not any(row.get('terminal_id')==card['terminal_id'] for row in processes['processes']))


def existing_only_contract(before, after, activity_before, activity_after, version_root):
    """A UI process replacement must leave serving owners and work untouched."""
    version_root=Path(version_root)
    if before['errors'] or after['errors'] or before.get('serving') != after.get('serving'):
        raise RuntimeError('existing-only reopen changed daemon ownership or has incomplete evidence')
    if activity_before != activity_after:
        raise RuntimeError('existing-only reopen started a provider, conversation or turn')
    def identities(snapshot, predicate):
        return {(row['pid'],row['start_ticks']) for row in snapshot['processes'] if predicate(row)}
    def image(product):
        return lambda row: row.get('executable')==str(version_root/'bin'/product)
    daemon=identities(before,image('cagentsd'))
    if not daemon or daemon != identities(after,image('cagentsd')):
        raise RuntimeError('existing-only reopen replaced a matched daemon process')
    root=Path(before['root'])
    providers={
        'claude':lambda row: str(root/'bin/claude') in row.get('argv',[]),
        'codex':lambda row: str(root/'libexec/codex/rehearsal-codex.py') in row.get('argv',[])
            and any(arg=='--listen' or arg.startswith('--listen=') for arg in row['argv']),
    }
    stable={}
    for provider,predicate in providers.items():
        original=identities(before,predicate)
        if not original or original != identities(after,predicate):
            raise RuntimeError(f'existing-only reopen replaced the {provider} provider process')
        stable[provider]=sorted(original)
    child=identities(before,lambda row: row.get('role')=='long-lived-child')
    if child != identities(after,lambda row: row.get('role')=='long-lived-child'):
        raise RuntimeError('existing-only reopen lost the original terminal child')
    is_ui=image('cagents-ui')
    old_ui=identities(before,is_ui); new_ui=identities(after,is_ui)
    ui_rows=[row for row in after['processes'] if is_ui(row)]
    if (len(old_ui)!=2 or len(new_ui)!=2 or not old_ui.isdisjoint(new_ui)
            or any('--restore-existing-only' not in row['argv'] for row in ui_rows)):
        raise RuntimeError('the two replacement UI processes did not run with --restore-existing-only')
    return {'serving':after['serving'],'daemon_processes':sorted(daemon),'provider_processes':stable,
            'terminal_children':sorted(child),'ui_before':sorted(old_ui),'ui_after':sorted(new_ui),
            'ui_processes':[{key:row[key] for key in ('pid','start_ticks','executable','argv')} for row in ui_rows],
            'activity_before':activity_before,'activity_after':activity_after}


def sidebar_channel_ref(observation, title):
    """Resolve the visible sidebar row, excluding headers and hover tooltips."""
    candidates = [run['ref'] for run in observation.get('runs',[])
                  if run.get('x',260)<260 and run.get('y',0)>80
                  and re.sub(r'^\W+','',run.get('text','')).strip().casefold()==title.casefold()]
    if len(candidates) != 1:
        raise RuntimeError(f'canonical channel label has no unique sidebar reference: {title}')
    return candidates[0]


def selected_chat(root, slot, card_id, channel_id, expected, required_thread=None):
    """Read the exact saved selection; visible labels grant no identity."""
    data = Path(root)/'home/.local/share/cagents'
    database = data/'sessions.sqlite3' if slot == 'main' else data/'windows'/slot/'sessions.sqlite3'
    with closing(sqlite3.connect(database.as_uri()+'?mode=ro', uri=True, timeout=1)) as connection:
        connection.execute('PRAGMA query_only=ON')
        connection.execute('BEGIN')
        row = connection.execute(
            'SELECT c.workspace_id,c.kind_tag,c.content_json,w.focused_card_id,c.existence_json '
            'FROM cards c JOIN workspaces w ON w.workspace_id=c.workspace_id WHERE c.card_id=?',
            [card_id]).fetchone()
        if not row or row[1] != 'chat' or row[3] != card_id or json.loads(row[4]) != 'open':
            return None
        descriptor = json.loads(row[2])
        active = connection.execute('SELECT value FROM metadata WHERE key=?',
            [f'workspace:{row[0]}:navigation:active_thread']).fetchone()
        launch = descriptor.get('launch', {})
        proof = expected.get('proof')
        thread = launch.get('session_id')
        if (descriptor.get('id') != expected['id']
                or launch.get('provider') != expected['launch']['provider']
                or launch.get('recipe') != expected['launch'].get('recipe')
                or not isinstance(proof,dict) or descriptor.get('proof') != proof
                or any(not isinstance(proof.get(field),str) or not proof[field]
                       for field in ('host_session_id','boot_id','host_incarnation'))
                or not isinstance(thread,str) or not thread
                or (expected['launch'].get('session_id') is not None
                    and thread != expected['launch']['session_id'])
                or (required_thread is not None and thread != required_thread)
                or not active or json.loads(active[0]) != channel_id):
            return None
        return {'database':str(database), 'workspace':row[0], 'card':card_id,
                'channel':channel_id, 'session':descriptor}


def refusal_preserved_processes(records):
    """A refusal keeps the original daemon and UI processes, not just their version."""
    try:
        proofs = [json.loads((records/name/'terminals.json').read_text())['data']['proof']
                  for name in ('before','after')]
        snapshots = [json.loads((records/name/'snapshot.json').read_text())
                     for name in ('before','after')]
        windows = [{(row['pid'], row['start_ticks']) for row in snapshot['processes']
                    if row.get('argv') and Path(row['argv'][0]).name == 'cagents-ui'}
                   for snapshot in snapshots]
        errors = []
        if not proofs[0] or proofs[0] != proofs[1]:
            errors.append('console refusal replaced the original daemon')
        if len(windows[0]) != 2 or windows[0] != windows[1]:
            errors.append('console refusal closed or replaced the original windows')
        return errors
    except (OSError, ValueError, KeyError, TypeError) as exc:
        return [f'console refusal identity evidence missing: {exc}']


class Rehearsal:
    def __init__(self, root, shell):
        self.root = Path(root)
        self.shell = shell
        self.steps = []
        self.errors = []

    def call(self, *args, timeout=90, check=True):
        start = time.monotonic()
        try:
            result = subprocess.run([self.shell, '--root', str(self.root), *map(str, args)],
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                    text=True, timeout=timeout)
            code, output = result.returncode, result.stdout
        except subprocess.TimeoutExpired as exc:
            code, output = 124, (exc.stdout or b'').decode(errors='replace') if isinstance(exc.stdout, bytes) else exc.stdout or ''
            output += '\nCommand timed out\n'
        self.steps.append({'command': list(map(str, args)), 'seconds': round(time.monotonic()-start, 3),
                           'exit': code, 'output': output})
        if check and code:
            raise RuntimeError(f'{args}: exit {code}: {output[-1500:]}')
        return code, output

    def cli(self, *args, project=None, check=True):
        command = ['env', 'cagents', '--json', *args]
        if project:
            command = ['env', 'sh', '-c', 'cd "$1"; shift; exec cagents --json "$@"', 'sh',
                       str(self.root/'home/src'/project), *args]
        code, output = self.call(*command, check=check)
        try:
            data = json.loads(output)
        except ValueError:
            data = {'raw': output}
        return code, data

    def mapped(self):
        _, output = self.call('wm', '-j', 'windows', timeout=20)
        windows = json.loads(output)
        slots = {}
        for window in windows:
            if 'cagents' not in (window.get('app_id') or '').lower() and 'ceditor' not in (window.get('app_id') or '').lower():
                continue
            try:
                argv = Path(f'/proc/{window["pid"]}/cmdline').read_bytes().decode().split('\0')
            except OSError:
                continue
            slot = 'ceditor' if 'ceditor' in (window.get('app_id') or '').lower() else 'main'
            if '--window' in argv:
                slot = argv[argv.index('--window')+1]
            # Relaunched windows name the main window explicitly
            # (instance::PRIMARY_WINDOW); it is the same window and store.
            if slot == 'primary':
                slot = 'main'
            if slot in slots:
                raise RuntimeError(f'more than one mapped window for {slot}')
            slots[slot] = window
        return slots

    def wait_windows(self, required, timeout=60, version=None):
        deadline = time.monotonic()+timeout
        while time.monotonic() < deadline:
            slots = self.mapped()
            if all(name in slots for name in required):
                if version is None:
                    return slots
                try:
                    expected = self.root/'home/.local/share/cagents-install/versions'/version/'bin/cagents-ui'
                    if all(Path(os.readlink(f'/proc/{slots[name]["pid"]}/exe')) == expected for name in required):
                        return slots
                except OSError:
                    pass
            time.sleep(1)
        raise RuntimeError(f'windows did not map on {version or "the desktop"}: wanted {required}, saw {list(slots)}')

    def observe(self, output, shots=None):
        observed = {}
        for slot, window in sorted(self.mapped().items()):
            self.call('wm', 'action', 'focus-window', '--id', window['id'])
            _, text = self.call('drive', '--json', 'observe', '--no-text', timeout=30)
            evidence = json.loads(text)
            if evidence['window']['id'] != window['id']:
                raise RuntimeError(f'observe selected the wrong window for {slot}')
            observed[slot] = evidence
            if shots:
                self.call('shot', f'{shots}-{slot}', timeout=45)
        Path(output).write_text(json.dumps(observed, indent=2, sort_keys=True)+'\n')
        return observed

    def capture_chats(self, side):
        """Exercise the seeded chats themselves, preserving UUID-bound evidence."""
        seeds = json.loads((self.root/'seeds.json').read_text())['items']
        targets = [item for item in seeds if item['state'] in ('named-chat','codex-chat')]
        if not targets or not any(item['state']=='named-chat' for item in targets):
            raise RuntimeError('the seeded Claude chat identity is unavailable for native evidence')
        _, response = self.cli('--no-start','agent','list')
        agents = {row['agent']['id']:row['agent'] for row in response['data']['agents']}
        _, response = self.cli('--no-start','channel','list')
        channels = response['data']['channels']
        originals = (json.loads((self.root/'ui-chats-before.json').read_text())
                     if side != 'before' else [])
        evidence = []
        for item in targets:
            original = item['response']['data']['agent']
            current = agents[original['id']]
            channel_id = original['placement']['Channel']
            if (current['placement'] != original['placement']
                    or current['spec']['target']['repository'] != original['spec']['target']['repository']):
                raise RuntimeError(f'seeded chat {original["id"]} changed its canonical placement')
            chat = current['session']['Chat']
            provider = original['spec']['provider'].lower()
            if chat['launch']['provider'] != original['spec']['provider']:
                raise RuntimeError('seeded chat changed provider')
            channel = next(row for row in channels if row['id']==channel_id)
            title = channel['title']
            if not title or sum(row['title']==title for row in channels) != 1:
                raise RuntimeError(f'canonical chat channel {channel_id} has no unique visible label')
            windows = self.mapped()
            if not {'main','second'} <= windows.keys():
                raise RuntimeError('both native windows are required for chat evidence')
            for slot, window in sorted(windows.items()):
                if slot not in ('main','second'):
                    continue
                self.call('wm','action','focus-window','--id',window['id'])
                _, navigation = self.call('drive','--json','observe',timeout=30)
                navigation = json.loads(navigation)
                if navigation['window']['id'] != window['id']:
                    raise RuntimeError(f'chat navigation selected the wrong window for {slot}')
                reference = sidebar_channel_ref(navigation,title)
                self.call('drive','click','--stale-ok',reference,timeout=30)
                # Keep hover labels away from subsequent navigation and shots.
                self.call('drive','move','850,750',timeout=30)
                required_thread = None
                if side != 'before':
                    previous = next(view for view in originals
                                    if view['agent']==original['id'] and view['slot']==slot)
                    required_thread = previous['selection']['session']['launch']['session_id']
                _, status = self.cli('--no-start','agent','status',original['id'])
                actual = status['data']['agent']['session']['Chat']
                if actual['id'] != chat['id'] or actual['launch']['provider'] != chat['launch']['provider']:
                    raise RuntimeError('opening a seeded chat allocated a different session')
                deadline = time.monotonic()+15
                selected = None
                while time.monotonic()<deadline:
                    try:
                        selected = selected_chat(self.root,slot,original['id'],channel_id,actual,required_thread)
                    except (sqlite3.Error, OSError):
                        selected = None
                    if selected:
                        break
                    time.sleep(.1)
                if not selected:
                    self.call('shot',f'{side}-{provider}-{slot}-selection-failed',timeout=45,check=False)
                    raise RuntimeError(f'{slot} did not focus exact seeded {provider} card {original["id"]}')
                if any(view['agent']==original['id']
                       and view['selection']['session']['launch']['session_id'] != selected['session']['launch']['session_id']
                       for view in evidence):
                    raise RuntimeError('two windows disagree on the exact seeded provider thread')
                _, status = self.cli('--no-start','agent','status',original['id'])
                settled = status['data']['agent']['session']['Chat']
                if (settled['id'] != actual['id'] or settled.get('proof') != actual.get('proof')
                        or selected_chat(self.root,slot,original['id'],channel_id,settled,required_thread) is None):
                    raise RuntimeError('seeded chat ownership or thread changed during native capture')
                _, observed = self.call('drive','--json','observe',timeout=30)
                observation = json.loads(observed)
                if observation['window']['id'] != window['id']:
                    raise RuntimeError(f'chat observation selected the wrong window for {slot}')
                shot = f'{side}-{provider}-{slot}'
                self.call('shot',shot,timeout=45)
                evidence.append({'provider':provider,'slot':slot,'agent':original['id'],
                                 'selection':selected,'navigation_label':title,
                                 'observation':observation,'shot':f'shots/{shot}.webp'})
        (self.root/f'ui-chats-{side}.json').write_text(json.dumps(evidence,indent=2,sort_keys=True)+'\n')
        return evidence

    def reopen_existing_only(self, version):
        self.call('record','existing-only-before',timeout=150)
        before=json.loads((self.root/'records/existing-only-before/snapshot.json').read_text())
        activity_before=provider_activity(self.root)
        windows=self.mapped()
        for slot in ('main','second'):
            self.call('wm','action','close-window','--id',windows[slot]['id'])
        old={row['pid']:row['start_ticks'] for row in before['processes']
             if row.get('executable')==str(self.root/f'home/.local/share/cagents-install/versions/{version}/bin/cagents-ui')}
        deadline=time.monotonic()+30
        while time.monotonic()<deadline:
            alive=False
            for pid,start in old.items():
                try:
                    stat=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
                    alive |= int(stat[19])==start and stat[0] not in ('Z','X')
                except (OSError,ValueError,IndexError):
                    pass
            if not alive and not {'main','second'} & self.mapped().keys():
                break
            time.sleep(.1)
        else:
            raise RuntimeError('ordinary UI processes did not close before existing-only relaunch')
        for slot in ('main','second'):
            self.call('launch','--restore-existing-only',slot)
        self.wait_windows(['main','second'],version=version)
        self.capture_chats('existing-only')
        self.call('record','existing-only-after',timeout=150)
        after=json.loads((self.root/'records/existing-only-after/snapshot.json').read_text())
        activity_after=provider_activity(self.root)
        result=existing_only_contract(before,after,activity_before,activity_after,
            self.root/f'home/.local/share/cagents-install/versions/{version}')
        self.call('compare','existing-only-before','existing-only-after',timeout=30)
        result['records']={'before':'records/existing-only-before/snapshot.json',
                           'after':'records/existing-only-after/snapshot.json'}
        result['views']={'before':'ui-chats-after.json','after':'ui-chats-existing-only.json'}
        (self.root/'existing-only.json').write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')

    def private_processes(self, after_intentional_stop=False):
        flags=['--after-intentional-stop'] if after_intentional_stop else []
        _, output=self.call('env','/usr/bin/python3',Path(__file__).with_name('rehearse_update_records.py'),
                           'private-processes',self.root,*flags)
        return json.loads(output)

    def new_terminal_after_restore(self, version, expect_failure=False):
        """Exercise a normal native action after the completed restore boundary."""
        destination=self.root/'new-terminal-after-restore'
        destination.mkdir(mode=0o700)
        version_root=self.root/f'home/.local/share/cagents-install/versions/{version}'
        _, before_status=self.cli('--no-start','daemon','status')
        _, inventory=self.cli('--no-start','terminal','list','--all')
        before=self.private_processes()
        before_windows={slot:window['id'] for slot,window in self.mapped().items() if slot in ('main','second')}
        shortcut=terminal_shortcut(self.root)
        status=before_status.get('data',{})
        owners=[row for row in before['processes'] if row.get('executable')==str(version_root/'bin/cagentsd')
                and row.get('argv')==[row['executable']]]
        if (before['errors'] or len(owners)!=1 or before_windows.keys()!={'main','second'}
                or status.get('instance')!='production' or status.get('running') is not True
                or status.get('daemon_version')!=version
                or status.get('socket')!=str(self.root/'rt/cagents/cagentsd.sock')):
            raise RuntimeError('normal-action test requires the matched private serving daemon and both windows')
        self.call('wm','action','focus-window','--id',before_windows['main'])
        self.call('shot','new-terminal-before-stop',timeout=45)
        evidence={'before_processes':before,'before_windows':before_windows,'before_status':before_status,
                  'before_serving':inventory['data']['proof'],'original_terminals':inventory,
                  'initial_verified_record':'records/existing-only-after',
                  'activity_before':provider_activity(self.root),'shortcut':shortcut,'events':[]}
        artifact=self.root/'new-terminal-after-restore.json'
        def save():
            artifact.write_text(json.dumps(evidence,indent=2,sort_keys=True)+'\n')
            artifact.chmod(0o600)
        save()
        self.call('stop-daemon',timeout=45)
        evidence['events'].append({'event':'counted_private_stop_completed','monotonic':time.monotonic()})
        deadline=time.monotonic()+20
        while time.monotonic()<deadline:
            observed=self.private_processes(after_intentional_stop=True)
            if not any((row['pid'],row['start_ticks'])==(owners[0]['pid'],owners[0]['start_ticks'])
                       for row in observed['processes']):
                break
            time.sleep(.1)
        else:
            raise RuntimeError('the deliberately stopped daemon process did not exit')
        evidence['after_stop_processes']=observed
        _, stopped_status=self.cli('--no-start','daemon','status',check=False)
        evidence['after_stop_status']=stopped_status
        evidence['events'].append({'event':'status_after_counted_stop','monotonic':time.monotonic(),
                                   'running':stopped_status.get('data',{}).get('running')})
        evidence['cards_before_input']=[card['card'] for card in terminal_cards(self.root)]
        self.call('wm','action','focus-window','--id',before_windows['main'])
        self.call('shot','new-terminal-before-action',timeout=45)
        _, input_status=self.cli('--no-start','daemon','status',check=False)
        evidence['before_input_status']=input_status
        evidence['events'].append({'event':'status_before_user_input','monotonic':time.monotonic(),
                                   'running':input_status.get('data',{}).get('running')})
        evidence['daemon_start_observation']=('serving_before_new_terminal_input'
            if input_status.get('data',{}).get('running') is True else
            'not_serving_before_input; later_start_may_be_action_or_background_reconnect')
        save()
        self.call('drive','key',shortcut['chord'],timeout=30)
        evidence['events'].append({'event':'native_new_terminal_shortcut_sent','monotonic':time.monotonic()})
        save()
        deadline=time.monotonic()+45
        selected=None
        original_uis={(row['pid'],row['start_ticks']) for row in before['processes']
                      if row.get('executable')==str(version_root/'bin/cagents-ui')}
        while time.monotonic()<deadline:
            try:
                candidates=[card for card in terminal_cards(self.root) if card['focused']
                            and card['card'] not in evidence['cards_before_input']]
                if len(candidates)==1:
                    selected=candidates[0]
                    evidence['selected_card']=selected
                    save()
                    if expect_failure:
                        snapshot_code,snapshot=self.cli('--no-start','terminal','snapshot',selected['terminal_id'],check=False)
                        if not snapshot_code:
                            raise RuntimeError('the expected before failure instead produced a terminal snapshot')
                        _, observation=self.call('drive','--json','observe',timeout=30)
                        _, current_status=self.cli('--no-start','daemon','status')
                        if current_status.get('data',{}).get('running') is True:
                            raise RuntimeError('the expected before failure instead has a serving daemon')
                        sample={'monotonic':time.monotonic(),'card':selected,'snapshot_terminal_id':selected['terminal_id'],
                                'snapshot_exit':snapshot_code,'snapshot_refusal':snapshot,'status':current_status,
                                'observation':json.loads(observation),'processes':self.private_processes(after_intentional_stop=True),
                                'windows':{slot:window['id'] for slot,window in self.mapped().items() if slot in ('main','second')}}
                        if pending_terminal_failure(sample,selected,before_windows,original_uis,version_root):
                            samples=evidence.setdefault('pending_observations',[])
                            samples.append(sample)
                            if sample['monotonic']-samples[0]['monotonic']>=3:
                                evidence['failure_kind']='pending_without_daemon'
                                save()
                                break
                            save()
                            time.sleep(.2)
                            continue
                        evidence['pending_observations']=[]
                        code, copied=self.call('drive','click','Copy details',timeout=30,check=False)
                        if not code:
                            code, diagnostic=self.call('app','wl-paste','--no-newline',timeout=10,check=False)
                            if not code:
                                fields=dict(line.split(': ',1) for line in diagnostic.splitlines() if ': ' in line)
                                if fields.get('Terminal')==selected['terminal_id'] and fields.get('Code'):
                                    evidence['failure_diagnostic']=diagnostic
                                    break
                        save()
                    code, shown=self.cli('--no-start','terminal','show',selected['terminal_id'],check=False)
                    actual=shown.get('data',{}).get('terminal',{}).get('session',{})
                    if (not code and isinstance(shown.get('data',{}).get('proof'),dict)
                            and terminal_live_proof(actual.get('continuity'))==shown.get('data',{}).get('proof')
                            and terminal_live_proof(selected['continuity'])==shown.get('data',{}).get('proof')
                            and selected['recipe']==actual['launch']):
                        if expect_failure:
                            raise RuntimeError('the expected before failure instead opened a live terminal')
                        break
            except (sqlite3.Error,OSError):
                pass
            time.sleep(.2)
        else:
            self.call('shot','new-terminal-unverified',timeout=45,check=False)
            raise RuntimeError('the native New terminal action did not reach the required observed failure'
                               if expect_failure else 'the native New terminal action did not create a focused live terminal')
        evidence['selected_card']=selected
        if expect_failure:
            _, observed=self.call('drive','--json','observe',timeout=30)
            evidence['failure_observation']=json.loads(observed)
            _, evidence['after_status']=self.cli('--no-start','daemon','status')
            evidence['after_processes']=self.private_processes(after_intentional_stop=True)
            evidence['after_windows']={slot:window['id'] for slot,window in self.mapped().items() if slot in ('main','second')}
            evidence['activity_after']=provider_activity(self.root)
            save()
            evidence['verified']=terminal_restart_failure_contract(evidence,version_root)
            self.call('drive','move','850,750',timeout=30)
            self.call('shot','new-terminal-failed',timeout=45)
            save()
            return
        evidence['terminal']=shown
        self.call('shot','new-terminal-opened',timeout=45)
        token='REHEARSAL_TERMINAL_'+uuid.uuid4().hex
        receipt=destination/'typed-output.txt'
        probe=terminal_probe_command(shown['data']['terminal']['session'],token,receipt)
        command=probe['command']
        evidence['token']=token
        evidence['shell_probe']=probe
        evidence['typed_command']=command
        self.call('drive','type',command,timeout=30)
        deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            line=receipt.read_text().strip() if receipt.exists() else ''
            match=re.fullmatch(re.escape(token)+r' pid=([1-9][0-9]*)',line)
            if match:
                break
            time.sleep(.1)
        else:
            raise RuntimeError('typing into the new native terminal did not execute the shell command')
        evidence.update(output_line=line,shell_pid=int(match[1]))
        _, evidence['terminal_snapshot']=self.cli('--no-start','terminal','snapshot',selected['terminal_id'])
        _, evidence['after_status']=self.cli('--no-start','daemon','status')
        evidence['after_processes']=self.private_processes(after_intentional_stop=True)
        evidence['after_windows']={slot:window['id'] for slot,window in self.mapped().items() if slot in ('main','second')}
        census,outcome=destination/'census.json',destination/'outcome.json'
        self.cli('--no-start','daemon','capture-update-census','--output',census)
        self.cli('--no-start','daemon','verify-update-census','--require-original','--census',census,'--outcome',outcome)
        evidence.update(census=json.loads(census.read_text()),receipt=json.loads(outcome.read_text()),
                        census_digest=hashlib.sha256(census.read_bytes()).hexdigest(),activity_after=provider_activity(self.root))
        evidence['events'].append({'event':'new_terminal_output_and_ready_owner_verified','monotonic':time.monotonic()})
        save()
        evidence['verified']=new_terminal_action_contract(evidence,version_root)
        self.call('drive','move','850,750',timeout=30)
        self.call('shot','new-terminal-working',timeout=45)
        save()

    def seed(self, args):
        evidence = []
        def seed(name, argv, project=None, provider=None):
            if provider:
                (self.root/'home/.claude'/(Path(provider).stem+'.started')).unlink(missing_ok=True)
                source = Path(__file__).resolve().parents[1]/'tests/fixtures'/provider
                target = self.root/'bin/claude'
                shutil.copyfile(source, target.with_suffix('.next'))
                target.with_suffix('.next').chmod(0o755)
                target.with_suffix('.next').replace(target)
            try:
                code, response = self.cli(*argv, project=project, check=False)
                entry = {'state': name, 'command': argv, 'exit': code, 'response': response}
                evidence.append(entry)
                if provider and code==0:
                    marker = self.root/'home/.claude'/(Path(provider).stem+'.started')
                    deadline = time.monotonic()+30
                    while not marker.exists() and time.monotonic()<deadline:
                        time.sleep(.1)
                    entry['fixture_started'] = marker.read_text() if marker.exists() else None
                    if not marker.exists():
                        self.errors.append(f'{name}: fixture launch was not observed')
                else:
                    time.sleep(.2)
            finally:
                if provider:
                    source = Path(__file__).resolve().parents[1]/'tests/fixtures/chat-composer-claude.sh'
                    shutil.copyfile(source, self.root/'bin/claude.next')
                    (self.root/'bin/claude.next').chmod(0o755)
                    (self.root/'bin/claude.next').replace(self.root/'bin/claude')
            return entry
        alpha, beta = [str(self.root/'home/src'/name) for name in ('alpha','beta')]
        for path in (alpha,beta):
            seed('project', ['project','open',path])
        details = seed('details-title', ['channel','spawn','--repository',alpha,'--title','Release Details: rollout'])
        details_id = details['response'].get('data',{}).get('channel',{}).get('id')
        placement = ['--channel',details_id] if details_id else []
        seed('named-chat', ['agent','spawn','--provider','claude','--surface','chat','--name','chat-alpha',*placement,'Seeded alpha chat'], 'alpha')
        if not args.no_console:
            seed('console', ['agent','spawn','--provider','claude','--surface','terminal','--name','console-alpha','Seeded console'], 'alpha')
        if not args.no_codex:
            codex_channel = seed('codex-channel', ['channel','spawn','--repository',alpha,'--title','Codex rehearsal'])
            codex_channel_id = codex_channel['response']['data']['channel']['id']
            seed('codex-chat', ['agent','spawn','--provider','codex','--surface','chat','--name','codex-alpha',
                               '--channel',codex_channel_id,'Offline Codex rehearsal'], 'alpha')
        for index in range(args.untitled):
            # Empty channel title is the product's actual untitled state.
            channel = seed(f'untitled-channel-{index+1}', ['channel','spawn','--repository',beta])
            channel_id = channel['response'].get('data',{}).get('channel',{}).get('id')
            placement = ['--channel',channel_id] if channel_id else []
            seed(f'untitled-chat-{index+1}', ['agent','spawn','--provider','claude','--surface','chat',*placement,f'Untitled conversation {index+1}'], 'beta')
        seed('shell', ['terminal','spawn','--cwd',alpha])
        if not args.no_child:
            seed('long-lived-child', ['terminal','spawn','--cwd',alpha,'--','bash','-c',
                                      f'sleep 86400 & echo $! > {self.root}/child.pid; wait'])
        if args.ignore_term:
            seed('ignores-sigterm', ['terminal','spawn','--cwd',beta,'--','bash','-c',
                                    'trap "" TERM; while :; do sleep 60; done'])
        if args.no_init:
            seed('no-provider-init', ['agent','spawn','--provider','claude','--surface','chat','--name','no-init','Never initialize'],
                 'beta', 'rehearsal-claude-no-init.py')
        if args.exit_mid_turn:
            seed('provider-exits-mid-turn', ['agent','spawn','--provider','claude','--surface','chat','--name','mid-turn','Exit before result'],
                 'beta', 'rehearsal-claude-exit-mid-turn.py')
        if args.uncarryable:
            seed('uncarryable-job', ['terminal','spawn','--cwd',beta,'--','sleep','86400'])
        if args.unreachable:
            entry = seed('unreachable-holder', ['terminal','spawn','--cwd',beta,'--','bash','-c','sleep 86400 & wait'])
            # A holder is the installed cagents __terminal-host process, scoped by private env.
            _, listing = self.cli('terminal','list', check=False)
            entry['before_stop'] = listing
            terminal_id = entry.get('response',{}).get('data',{}).get('terminal',{}).get('session',{}).get('id')
            matches = []
            registry = self.root/'rt/cagents/holders'/f'{terminal_id}.json'
            try:
                holder = json.loads(registry.read_text())['holder']
                proc = Path(f'/proc/{holder["pid"]}')
                env = proc.joinpath('environ').read_bytes().split(b'\0')
                stat = proc.joinpath('stat').read_text().rsplit(')',1)[1].split()
                argv = proc.joinpath('cmdline').read_bytes().split(b'\0')
                if (f'HOME={self.root}/home'.encode() in env
                        and '--cagents-terminal-holder'.encode() in argv
                        and int(stat[19]) == holder['start']):
                    matches.append(holder['pid'])
            except (OSError,ValueError,KeyError):
                pass
            unreachable_entry = entry
            unreachable_matches = matches
        # Verify actual old inventory rather than treating spawn's ACK as startup proof.
        time.sleep(2)
        for noun in ('agent','terminal','channel'):
            code, listing = self.cli(noun,'list',check=False)
            evidence.append({'state':f'verified-{noun}-list','exit':code,'response':listing})
        # Provider startup includes account probing and the owned listener handshake.
        if not args.no_codex:
            deadline = time.monotonic()+40
            while True:
                _, listing = self.cli('agent','list',check=False)
                agents = listing.get('data',{}).get('agents',[])
                matching = [a for a in agents if a.get('agent',{}).get('name') == 'codex-alpha']
                states = [str(a.get('state','')).lower() for a in matching]
                if states and all(state in ('running','idle') for state in states):
                    break
                if time.monotonic() >= deadline or any(state in ('failed','interrupted') for state in states):
                    self.errors.append('Codex did not reach Running/Idle; see verified agent status and logs/codex-provider.jsonl')
                    break
                time.sleep(.5)
            for entry in evidence:
                if entry['state'] == 'codex-chat':
                    entry['verified_agents'] = matching
                    if matching:
                        _, status = self.cli('agent','status',matching[0]['agent']['id'],check=False)
                        entry['verified_status'] = status
        # Verify each spawn against OLD's actual inventories, including chat titles.
        _, session_list = self.cli('sessions','--all',check=False)
        _, agent_list = self.cli('agent','list',check=False)
        _, terminal_list = self.cli('terminal','list',check=False)
        _, channel_list = self.cli('channel','list',check=False)
        agents = agent_list.get('data',{}).get('agents',[])
        sessions = session_list.get('data',{}).get('sessions',[])
        for entry in evidence:
            data = entry.get('response',{}).get('data',{})
            agent_id = data.get('agent',{}).get('id')
            terminal_id = data.get('terminal',{}).get('session',{}).get('id')
            channel_id = data.get('channel',{}).get('id')
            if agent_id:
                entry['verified_agents'] = [a for a in agents if a['agent']['id']==agent_id]
                chat_id = next(((a['agent'].get('session') or {}).get('Chat',{}).get('id') for a in entry['verified_agents']),None)
                entry['verified_chats'] = [row for row in sessions if row['id']==chat_id]
                if not entry['verified_agents']:
                    self.errors.append(f'{entry["state"]}: agent absent from OLD inventory')
                if entry['state'].startswith('untitled-chat') and (not entry['verified_chats'] or any(row.get('title') for row in entry['verified_chats'])):
                    self.errors.append(f'{entry["state"]}: did not verify an untitled conversation')
                if entry['state'] in ('no-provider-init','provider-exits-mid-turn'):
                    _, entry['verified_status'] = self.cli('agent','status',agent_id,check=False)
            if terminal_id:
                entry['verified_terminals'] = [t for t in terminal_list.get('data',{}).get('terminals',[]) if t['id']==terminal_id]
                if not entry['verified_terminals']:
                    self.errors.append(f'{entry["state"]}: terminal absent from OLD inventory')
            if channel_id:
                entry['verified_channels'] = [c for c in channel_list.get('data',{}).get('channels',[]) if c['id']==channel_id]
        if args.uncarryable:
            # Local jobs normally carry even through legacy preparation. Make
            # its preservation directory unavailable, reversibly, to exercise
            # OLD's actual active-job refusal rather than assuming sleep blocks.
            holders = self.root/'rt/cagents/holders'
            saved = holders.with_name('holders.rehearsal-saved')
            if saved.exists():
                raise RuntimeError('an uncarryable fault is already active')
            holders.rename(saved)
            holders.write_text('rehearse-update: preservation unavailable\n')
            evidence.append({'state':'preservation-directory-unavailable', 'path':str(holders), 'exit':0})
            code, result = self.cli('daemon','prepare-upgrade','--yes','--no-relaunch-agents',check=False)
            evidence.append({'state':'old-prepare-refusal','exit':code,'response':result})
            if code==0:
                self.errors.append('old prepare unexpectedly succeeded despite unavailable preservation')
        if args.unreachable:
            import signal
            for pid in unreachable_matches:
                os.kill(pid,signal.SIGSTOP)
            unreachable_entry['stopped_holder_pids'] = unreachable_matches
            if not unreachable_matches:
                self.errors.append('could not identify the unreachable terminal holder')
        evidence.append({'state':'verified-sessions-list','exit':0,'response':session_list})
        (self.root/'seeds.json').write_text(json.dumps({'items':evidence,'errors':self.errors},indent=2,sort_keys=True)+'\n')
        for entry in evidence:
            print(f'{entry["state"]}: exit={entry.get("exit")}')
        for error in self.errors:
            print('VERIFICATION FAILED: '+error)
        print(f'Seed evidence: {self.root}/seeds.json')
        return bool(self.errors or any(e.get('exit') for e in evidence if e['state'] != 'old-prepare-refusal'))

    def run(self, args):
        import signal
        def interrupted(signum, frame):
            raise RuntimeError(f'interrupted by signal {signum}')
        signal.signal(signal.SIGTERM,interrupted)
        signal.signal(signal.SIGINT,interrupted)
        start = time.monotonic()
        if self.root.exists():
            raise RuntimeError('run requires a fresh, absent root')
        failure = None
        try:
            self.call('init')
            self.call('install', args.old_dir, args.old_version, args.old_installer, timeout=180)
            candidate=candidate_namespace_cli(self.root,args.new_dir,args.new_version)
            current=self.root/'home/.local/share/cagents-install/current'
            original_current=current.resolve()
            _, source=self.cli('--no-start','daemon','status')
            code, output=self.call('env',candidate,'--no-start','--json','daemon','status',check=False)
            try:
                observed=json.loads(output)
            except ValueError:
                observed={'raw':output}
            _, version=self.call('env',candidate,'--version')
            probe=self.root/'candidate-namespace.json'
            evidence={'source':source,'candidate_status':{'exit':code,'payload':observed},
                      'candidate_version':version,'current':str(original_current),
                      'archive':json.loads((candidate.parent/'archive.json').read_text())}
            probe.write_text(json.dumps(evidence,indent=2,sort_keys=True)+'\n')
            probe.chmod(0o600)
            _, terminals=self.cli('--no-start','terminal','list','--all')
            baseline=candidate.parent/'baseline-terminals.json'
            baseline.write_text(json.dumps(terminals,indent=2,sort_keys=True)+'\n')
            baseline.chmod(0o600)
            _, output=self.call('env','/usr/bin/python3',Path(__file__).with_name('rehearse_update_records.py'),
                               'namespace-source',self.root,baseline)
            evidence['source_snapshot']=json.loads(output)
            census=candidate.parent/'census.json'
            receipt=candidate.parent/'receipt.json'
            self.call('env',candidate,'--no-start','--json','daemon','capture-update-census','--output',census)
            self.call('env',candidate,'--no-start','--json','daemon','verify-update-census',
                      '--require-original','--census',census,'--outcome',receipt)
            evidence.update(census=json.loads(census.read_text()),receipt=json.loads(receipt.read_text()),
                            census_digest=hashlib.sha256(census.read_bytes()).hexdigest())
            probe.write_text(json.dumps(evidence,indent=2,sort_keys=True)+'\n')
            candidate_namespace_contract(evidence,args.old_version,args.new_version)
            if current.resolve()!=original_current:
                raise RuntimeError('read-only candidate namespace probe changed current')
            self.call('desktop','start','--limit','900', timeout=None)
            self.call('launch','main')
            self.call('launch','second')
            self.wait_windows(['main','second'])
            code, _ = self.call('seed', *seed_flags(args), timeout=180, check=False)
            if code:
                self.errors.append('seed verification failed; see seeds.json')
            if args.ceditor:
                self.call('ceditor','start',args.ceditor,timeout=60)
                self.wait_windows(['main','second','ceditor'])
            self.capture_chats('before')
            code, _ = self.call('record','before',timeout=150,check=False)
            if code:
                self.errors.append('before record has evidence gaps')
            self.observe(self.root/'records/before/windows.json',shots='before')
            update_args = ['--yes'] if args.yes else ['--answer',args.answer]
            update_args += ['--working',args.working,'--stop',args.stop]
            if args.fail_new_daemon:
                update_args.append('--fail-new-daemon')
            for flag in args.upgrade_arg:
                update_args.append(flag)
            code, _ = self.call('update',args.new_dir,args.new_version,args.new_installer,*update_args,timeout=300,check=False)
            if args.fail_new_daemon:
                if not code:
                    self.errors.append('forced daemon startup failure unexpectedly succeeded')
                status_path = self.root/'home/.local/share/cagents-install/.update/status.json'
                try:
                    status = json.loads(status_path.read_text())
                    log = Path(status['log_path']).read_text()
                    receipt_path = self.root/'logs/daemon-start-failure.txt'
                    if status['phase'] != 'Failed':
                        self.errors.append('worker did not report a failed update')
                    if args.expect_console_refusal:
                        if 'cannot keep a running agent console connected' not in log or receipt_path.exists():
                            self.errors.append('unsafe console replacement was not refused before candidate startup')
                    else:
                        receipt = receipt_path.read_text().split()
                        if ('exit status: 1' not in log or len(receipt) != 2
                                or receipt[0] != args.new_version or int(receipt[1]) <= 1):
                            self.errors.append('worker did not report the executed daemon startup failure')
                except (OSError, KeyError, ValueError) as exc:
                    self.errors.append(f'forced failure evidence missing: {exc}')
                current = self.root/'home/.local/share/cagents-install/current'
                if not args.expect_console_refusal and current.resolve().name != args.old_version:
                    self.errors.append('failed update did not restore the old current version')
                serving_code, serving = self.cli('--no-start','daemon','status',check=False)
                if (serving_code or serving.get('data',{}).get('running') is not True
                        or serving.get('data',{}).get('daemon_version') != args.old_version):
                    self.errors.append('old daemon is not serving after failed update')
            elif code:
                self.errors.append(f'update exited {code}')
            try:
                self.wait_windows(['main','second'],timeout=args.reopen_timeout,version=args.old_version if args.fail_new_daemon else args.new_version if code==0 else None)
            except RuntimeError as exc:
                self.errors.append(str(exc))
            self.capture_chats('after')
            code, _ = self.call('record','after',timeout=150,check=False)
            if code:
                self.errors.append('after record has evidence gaps')
            if args.expect_console_refusal:
                self.errors.extend(refusal_preserved_processes(self.root/'records'))
            self.observe(self.root/'records/after/windows.json',shots='after')
            code, _ = self.call('compare','before','after',timeout=30,check=False)
            if code:
                self.errors.append('comparison found regressions or missing evidence; see comparison step')
            if args.reopen_existing_only and not self.errors:
                self.reopen_existing_only(args.new_version)
            if args.new_terminal_after_restore and not self.errors:
                self.new_terminal_after_restore(args.new_version,args.expect_terminal_restart_failure)
        except Exception as exc:
            failure = str(exc)
            self.errors.append(failure)
            # The later destructive test must never overwrite the verified
            # preservation inventory captured before its intentional stop.
            if not (self.root/'new-terminal-after-restore.json').exists():
                self.call('record','after',timeout=150,check=False)
        finally:
            if ((self.root/'records/before/snapshot.json').exists()
                    and (self.root/'records/after/snapshot.json').exists()
                    and not any(step['command'][0]=='compare' for step in self.steps)):
                code, _ = self.call('compare','before','after',timeout=30,check=False)
                if code:
                    self.errors.append('comparison found regressions or missing evidence')
            code, _ = self.call('stop',timeout=150,check=False)
            if code:
                self.errors.append('cleanup failed; inspect private processes and native lock')
            report = [f'Rehearsal: {self.root}',f'OLD: {args.old_version}',f'NEW: {args.new_version}',
                      f'Ceditor: {args.ceditor or "disabled"}',f'Total duration: {time.monotonic()-start:.3f}s',
                      f'Outcome: {"FAIL" if self.errors else "PASS"}', '', 'Regressions / evidence gaps:',
                      *self.errors, '', 'Steps (seconds, exit):']
            for step in self.steps:
                report += [f'{step["seconds"]:.3f}s exit={step["exit"]}: {" ".join(step["command"])}']
                if step['command'][0] in ('compare','install') or (step['exit'] and step['command'][0] not in ('seed','update')):
                    report.append(step['output'])
            report += ['', 'Update transcript (includes every prompt and refusal):']
            for path in sorted((self.root/'logs').glob('update-*.log')):
                report += [str(path),path.read_text(errors='replace')]
            receipt = self.root/'records/after/recovery-current.json'
            if receipt.exists():
                report += ['', f'Fresh readiness and inventory verification: {receipt}', receipt.read_text()]
            policy=self.root/'existing-only.json'
            if policy.exists():
                report += ['',f'Existing-only UI process replacement: {policy}',policy.read_text()]
            action=self.root/'new-terminal-after-restore.json'
            if action.exists():
                report += ['',f'Explicit daemon stop and native New terminal action: {action}',action.read_text()]
            for side in ('before','after','existing-only'):
                views = self.root/f'ui-chats-{side}.json'
                if views.exists():
                    report += ['', f'Native chat selection evidence: {views}']
                    for view in json.loads(views.read_text()):
                        selected = view['selection']
                        report.append(f'{side} {view["provider"]} {view["slot"]}: card={selected["card"]} '
                                      f'channel={selected["channel"]} session={selected["session"]["id"]} '
                                      f'thread={selected["session"]["launch"].get("session_id")} '
                                      f'screenshot={view["shot"]}')
            seeds = self.root/'seeds.json'
            if seeds.exists():
                report += ['', 'Seed verification (full inventories and diagnostics: seeds.json):']
                evidence = json.loads(seeds.read_text())
                for item in evidence['items']:
                    status = [a.get('state') for a in item.get('verified_agents',[])]
                    report.append(f'{item["state"]}: exit={item.get("exit")} states={status} verified chats={len(item.get("verified_chats",[]))} terminals={len(item.get("verified_terminals",[]))} channels={len(item.get("verified_channels",[]))}')
                    if item['state']=='old-prepare-refusal' or 'verified_status' in item:
                        details = item.get('verified_status',item.get('response'))
                        if item['state']=='old-prepare-refusal':
                            report.append(json.dumps(details,sort_keys=True))
                        else:
                            data = details.get('data',{})
                            run = data.get('run') or {}
                            report.append('status: '+json.dumps({'state':run.get('state'),'result':run.get('result'),'error':details.get('error')},sort_keys=True))
                report += evidence['errors']
            self.root.mkdir(parents=True, exist_ok=True)
            (self.root/'report.txt').write_text('\n'.join(report)+'\n')
            (self.root/'steps.json').write_text(json.dumps(self.steps,indent=2)+'\n')
            print(f'Report: {self.root}/report.txt')
        return int(bool(self.errors or failure or any(s['command'][0]=='compare' and s['exit'] for s in self.steps)))


def seed_options(parser):
    parser.add_argument('--untitled',type=int,choices=(0,1,2),default=2)
    parser.add_argument('--no-codex',action='store_true')
    parser.add_argument('--no-console',action='store_true',help='seed native chats and ordinary terminals without a managed agent console')
    parser.add_argument('--no-child',action='store_true')
    for flag in ('ignore-term','no-init','exit-mid-turn','uncarryable','unreachable'):
        parser.add_argument('--'+flag,action='store_true')


def seed_flags(args):
    flags = ['--untitled',str(args.untitled)]
    for flag in ('no_codex','no_console','no_child','ignore_term','no_init','exit_mid_turn','uncarryable','unreachable'):
        if getattr(args,flag):
            flags.append('--'+flag.replace('_','-'))
    return flags


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root')
    parser.add_argument('shell')
    commands=parser.add_subparsers(dest='command',required=True)
    run=commands.add_parser('run')
    for prefix in ('old','new'):
        for field in ('dir','version','installer'):
            run.add_argument('--'+prefix+'-'+field,required=True)
    run.add_argument('--ceditor',metavar='BIN_DIR')
    run.add_argument('--answer',default='yes')
    run.add_argument('--yes',action='store_true')
    run.add_argument('--working',default='',help='typed at the working-agents choice; empty waits for them')
    run.add_argument('--stop',default='',help='typed at the destructive stop list; empty cancels')
    run.add_argument('--upgrade-arg',action='append',default=[],help='use --upgrade-arg=--interrupt, for example')
    run.add_argument('--expect-console-refusal', action='store_true', help='with --fail-new-daemon, require console refusal before startup and preserve all work')
    run.add_argument('--fail-new-daemon', action='store_true', help='fail private candidate startup; require old current, serving daemon and preserved work')
    run.add_argument('--reopen-existing-only',action='store_true',help='after a healthy update reopen both UI windows with --restore-existing-only; require unchanged daemon, providers and work')
    run.add_argument('--new-terminal-after-restore',action='store_true',help='after verified existing-only reopening deliberately stop the private daemon, then open and use a new terminal through the same native UI')
    run.add_argument('--expect-terminal-restart-failure',action='store_true',help='before example only: require the native new-terminal action to fail with no daemon after the deliberate stop')
    run.add_argument('--reopen-timeout',type=float,default=45)
    seed_options(run)
    seed_options(commands.add_parser('seed'))
    wait=commands.add_parser('wait')
    wait.add_argument('names',nargs='+')
    observe=commands.add_parser('observe')
    observe.add_argument('output')
    lock=commands.add_parser('wait-native-lock')
    lock.add_argument('--fd',type=int,required=True)
    lock.add_argument('--parent-pid',type=int,required=True)
    args=parser.parse_args()
    if args.command == 'run' and args.expect_console_refusal and (not args.fail_new_daemon or args.no_console):
        parser.error('--expect-console-refusal requires --fail-new-daemon and a seeded console')
    if args.command == 'run' and args.reopen_existing_only and (args.fail_new_daemon or args.no_codex or args.ceditor):
        parser.error('--reopen-existing-only requires a healthy update with Claude and Codex and no ceditor')
    if args.command == 'run' and args.new_terminal_after_restore and not args.reopen_existing_only:
        parser.error('--new-terminal-after-restore requires --reopen-existing-only')
    if args.command == 'run' and args.expect_terminal_restart_failure and not args.new_terminal_after_restore:
        parser.error('--expect-terminal-restart-failure requires --new-terminal-after-restore')
    rehearsal=Rehearsal(args.root,args.shell)
    try:
        if args.command=='wait-native-lock':
            return wait_native_lock(args.root,args.fd,args.parent_pid)
        if args.command=='wait':
            rehearsal.wait_windows(args.names)
            return 0
        if args.command=='observe':
            rehearsal.observe(args.output)
            return 0
        return rehearsal.run(args) if args.command=='run' else rehearsal.seed(args)
    except (RuntimeError,OSError,ValueError) as exc:
        print(f'rehearse-update: {exc}',file=sys.stderr)
        return 2


if __name__=='__main__':
    sys.exit(main())
