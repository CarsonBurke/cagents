#!/usr/bin/python3
"""Plain restart of the rehearsal's authenticated private daemon only."""
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import socket
import struct
import time

import rehearse_update_records as records


def owned_daemon(root, executable, proof, no_recovery_environment=False):
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as peer:
        peer.connect(str(root/'rt/cagents/cagentsd.sock'))
        pid, uid, _ = struct.unpack('3i',peer.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,struct.calcsize('3i')))
    proc = Path('/proc')/str(pid)
    environment = dict(row.split(b'=',1) for row in (proc/'environ').read_bytes().split(b'\0') if b'=' in row)
    lock = json.loads((root/'rt/cagents/cagentsd.lock').read_text())
    if (uid != os.getuid() or (proc/'exe').readlink() != executable
            or lock.get('pid') != pid or lock.get('proof') != proof
            or environment.get(b'HOME') != os.fsencode(root/'home')
            or environment.get(b'XDG_RUNTIME_DIR') != os.fsencode(root/'rt')):
        raise RuntimeError('private daemon socket peer, executable, namespace and lock disagree')
    if no_recovery_environment and any(key.startswith(b'CAGENTS_UPDATE_RECOVERY')
                                       or key in (b'CAGENTS_INSTALL_CENSUS_PATH',b'CAGENTS_INSTALL_OUTCOME_PATH')
                                       for key in environment):
        raise RuntimeError('ordinary daemon start inherited update recovery authority')
    return pid


def immutable_evidence(root):
    result = {}
    directory = root/'home/.local/share/cagents/daemon-upgrade'
    for path in directory.rglob('*.json'):
        value = json.loads(path.read_text())
        if value.get('phase')=='Prepared' or path.name.startswith('daemon-upgrade.recovery.'):
            result[str(path.relative_to(directory))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def provider_identities(snapshot,root):
    return {(row['pid'],row['start_ticks']) for row in snapshot['processes']
            if row.get('state') not in ('Z','X') and (row.get('role')=='long-lived-child'
                or str(root/'bin/claude') in row.get('argv',[]))}


def restart(rehearsal, version):
    started = time.monotonic()
    root = rehearsal.root
    baseline = records.read_json(root/'records/before/snapshot.json')
    rehearsal.call('record','before-restart',timeout=150)
    before = records.read_json(root/'records/before-restart/snapshot.json')
    outcome = records.verified_recovery(baseline,before)
    if not outcome or not outcome.get('paused_chats'):
        raise RuntimeError('plain restart requires verified recovery with a paused conversation')
    original_census = (root/'records/before-restart/recovery-census.json').read_bytes()
    immutable = immutable_evidence(root)
    if not immutable:
        raise RuntimeError('recovery left no immutable checkpoint evidence for restart')
    executable = root/'home/.local/share/cagents-install/versions'/version/'bin/cagentsd'
    previous = owned_daemon(root,executable,before['serving'])
    restart_at = time.time()
    descriptor = os.pidfd_open(previous)
    try:
        if owned_daemon(root,executable,before['serving']) != previous:
            raise RuntimeError('private daemon ownership changed before pinned restart')
        signal.pidfd_send_signal(descriptor,signal.SIGKILL)
        if not select.select([descriptor],[],[],15)[0]:
            raise RuntimeError('pinned private daemon did not exit')
    finally:
        os.close(descriptor)
    rehearsal.cli('daemon','start')
    deadline = time.monotonic()+60
    while True:
        rehearsal.call('record','after',timeout=150,check=False)
        after = records.read_json(root/'records/after/snapshot.json')
        if records.verified_recovery(baseline,after):
            break
        if time.monotonic()>=deadline:
            raise RuntimeError('ordinary restart did not pass the original census verifier')
        time.sleep(.2)
    if (after['serving']==before['serving']
            or after['serving']['boot_id']!=before['serving']['boot_id']):
        raise RuntimeError('ordinary restart did not establish a fresh same-boot owner')
    successor = owned_daemon(root,executable,after['serving'],no_recovery_environment=True)
    successor_proof = after['serving']
    rehearsal.capture_chats('after')
    rehearsal.capture_console('after')
    rehearsal.call('record','after',timeout=150)
    after = records.read_json(root/'records/after/snapshot.json')
    if records.verified_recovery(baseline,after) is None or after['errors']:
        raise RuntimeError('post-restart native presentation lost verified readiness')
    if (after['serving'] != successor_proof
            or owned_daemon(root,executable,after['serving'],no_recovery_environment=True) != successor):
        raise RuntimeError('daemon ownership changed during post-restart native presentation')
    if (root/'records/after/recovery-census.json').read_bytes()!=original_census:
        raise RuntimeError('ordinary restart replaced the original raw census')
    if immutable_evidence(root)!=immutable:
        raise RuntimeError('ordinary restart changed immutable Prepared or outcome bytes')
    for collection in (*records.COLLECTIONS,'windows'):
        if before[collection].keys()!=after[collection].keys():
            raise RuntimeError(f'ordinary restart changed {collection} identities')
    if records.panes(before).keys()!=records.panes(after).keys():
        raise RuntimeError('ordinary restart changed saved pane identities')
    for key, row in before['agents'].items():
        if row.get('run')!=after['agents'][key].get('run'):
            raise RuntimeError('ordinary restart replaced an accepted run')
    if provider_identities(before,root)!=provider_identities(after,root):
        raise RuntimeError('ordinary restart replaced a retained provider or long-lived child')
    replay = [row for row in after.get('fixture_events',[]) if row.get('time',0)>=restart_at and (
        row.get('event')=='request' and row.get('method') in ('thread/start','thread/resume','turn/start')
        or row.get('event')=='launch' and any(str(arg).startswith('--listen') for arg in row.get('argv',[])))]
    if replay:
        raise RuntimeError('ordinary restart or history Reconnect started Codex work')
    summary = {'previous_pid':previous,'new_pid':successor,'proof_before':before['serving'],
               'proof_after':after['serving'],'immutable_sha256':immutable,'original_census_unchanged':True,
               'provider_work_replayed':False,'prepare_upgrade_invoked':False,
               'inherited_recovery_environment':[],'restart_at':restart_at}
    (root/'logs/plain-restart.json').write_text(json.dumps(summary,indent=2)+'\n')
    rehearsal.steps.append({'command':['plain-restart-after-recovery'],
                            'seconds':round(time.monotonic()-started,3),'exit':0,'output':json.dumps(summary)})
