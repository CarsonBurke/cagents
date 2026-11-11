#!/usr/bin/python3
"""Scenario orchestration for rehearse-update; all application commands use its private env."""
import argparse
import copy
from contextlib import closing
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import time

import rehearse_update_records as records


def sidebar_channel_ref(observation, title):
    """Resolve the visible sidebar row, excluding headers and hover tooltips."""
    candidates = [run['ref'] for run in observation.get('runs',[])
                  if run.get('x',260)<260 and run.get('y',0)>80
                  and re.sub(r'^\W+','',run.get('text','')).strip().casefold()==title.casefold()]
    if len(candidates) != 1:
        raise RuntimeError(f'canonical channel label has no unique sidebar reference: {title}')
    return candidates[0]


def visible_phrase(text, phrase):
    """Match OCR words in order, allowing repeated fragments between neighbors."""
    words = re.findall(r'[a-z]+',text.lower())
    expected = re.findall(r'[a-z]+',phrase.lower())
    for start, word in enumerate(words):
        if word != expected[0]:
            continue
        position = start
        for wanted in expected[1:]:
            following = next((index for index in range(position+1,min(position+4,len(words)))
                              if words[index]==wanted and all(fragment in words[position] or fragment in wanted
                                  for fragment in words[position+1:index])),None)
            if following is None:
                break
            position = following
        else:
            return True
    return False


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
        bound = copy.deepcopy(descriptor)
        if expected.get('launch',{}).get('session_id') is None:
            bound.get('launch',{}).update(session_id=None)
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
                or bound != expected
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


def expected_refusal(root, kind, old_version):
    before, after = [json.loads((root/'records'/side/'snapshot.json').read_text())
                     for side in ('before','after')]
    errors = refusal_preserved_processes(root/'records')
    status = json.loads((root/'home/.local/share/cagents-install/.update/status.json').read_text())
    phase = 'Failed' if kind=='interrupt' else 'Cancelled'
    if status.get('phase') != phase:
        errors.append(f'expected {phase}, saw {status.get("phase")}')
    target = after.get('target',{})
    if status.get('version') != target.get('version'):
        errors.append('refusal did not belong to the requested target')
    if kind=='interrupt' and 'did not stop in time; nothing was changed' not in status.get('message',''):
        errors.append('update did not report the interruption settlement bound')
    current = root/'home/.local/share/cagents-install/current'
    if current.resolve().name != old_version:
        errors.append('refusal changed the installed current version')
    executable = str(root/'home/.local/share/cagents-install/versions'/old_version/'bin/cagentsd')
    def daemons(snapshot):
        return {(row['pid'],row['start_ticks'],row.get('executable')) for row in snapshot['processes']
                if row.get('executable')==executable and row.get('state') not in ('Z','X')}
    if len(daemons(before))!=1 or daemons(before)!=daemons(after):
        errors.append('refusal replaced the original daemon process')
    for name in (*records.COLLECTIONS,'panes'):
        left, right = ((records.panes(before),records.panes(after)) if name=='panes' else (before[name],after[name]))
        if left.keys()!=right.keys() or any(records.project(row,name)!=records.project(right[key],name)
                                         for key,row in left.items() if key in right):
            errors.append(f'refusal changed the original {name}')
    if (root/'home/.local/share/cagents/daemon-upgrade/daemon-upgrade.json').exists():
        errors.append('refusal left a preparation checkpoint')
    started, finished = target.get('started',0), target.get('finished',0)
    events = [row for row in after.get('fixture_events',[]) if row.get('time',0)>=started]
    interrupts = [row for row in events if row.get('event')=='request' and row.get('method')=='turn/interrupt']
    if kind=='cancel':
        if interrupts:
            errors.append('review cancellation sent an interrupt request')
    else:
        parents = [row for row in interrupts if row.get('parent') is True and row.get('errors')==[]]
        if not parents or not 25 <= finished-min(row['time'] for row in parents) <= 40:
            errors.append('interruption refusal did not return at its expected 30-second bound')
        if any(row.get('event')=='notification' and row.get('method')=='turn/completed'
               and row.get('status')=='interrupted' for row in events):
            errors.append('unconfirmed interruption emitted an interrupted receipt')
    return errors


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
                     if side == 'after' else [])
        snapshots = ([json.loads((self.root/'records'/name/'snapshot.json').read_text())
                      for name in ('before','after')] if side == 'after' else None)
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
                if side == 'after':
                    previous = next(view for view in originals
                                    if view['agent']==original['id'] and view['slot']==slot)
                    required_thread = previous['selection']['session']['launch']['session_id']
                _, status = self.cli('--no-start','agent','status',original['id'])
                actual_agent = status['data']['agent']
                actual = status['data']['agent']['session']['Chat']
                if actual['id'] != chat['id'] or actual['launch']['provider'] != chat['launch']['provider']:
                    raise RuntimeError('opening a seeded chat allocated a different session')
                expected = actual
                projection = None
                if snapshots:
                    projection = records.paused_history_projection(*snapshots,previous['selection']['session'])
                    if projection is not None:
                        expected = projection
                        self.observe_paused(window,f'{side}-{provider}-{slot}')
                deadline = time.monotonic()+15
                selected = None
                while time.monotonic()<deadline:
                    try:
                        selected = selected_chat(self.root,slot,original['id'],channel_id,expected,required_thread)
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
                        or (projection is not None and status['data']['agent'] != actual_agent)
                        or selected_chat(self.root,slot,original['id'],channel_id,expected,required_thread) is None):
                    raise RuntimeError('seeded chat ownership or thread changed during native capture')
                _, observed = self.call('drive','--json','observe',timeout=30)
                observation = json.loads(observed)
                if (observation['window']['id'] != window['id'] or not observation.get('painted')
                        or observation['window'].get('focused') is False):
                    raise RuntimeError(f'chat observation selected the wrong window for {slot}')
                shot = f'{side}-{provider}-{slot}'
                self.call('shot',shot,timeout=45)
                evidence.append({'provider':provider,'slot':slot,'agent':original['id'],
                                 'selection':selected,'navigation_label':title,
                                 'observation':observation,'shot':f'shots/{shot}.webp'})
        (self.root/f'ui-chats-{side}.json').write_text(json.dumps(evidence,indent=2,sort_keys=True)+'\n')
        return evidence

    def observe_paused(self, window, label):
        deadline = time.monotonic()+30
        reconnected = False
        observations = []
        while time.monotonic()<deadline:
            _, text = self.call('drive','--json','observe',timeout=30)
            observed = json.loads(text)
            observations.append(observed)
            if observed['window']['id'] != window['id']:
                raise RuntimeError('paused history observation selected another window')
            words = ' '.join(row.get('text','') for row in observed.get('runs',[]))
            if (observed.get('painted') and visible_phrase(words,'Resume conversation')
                    and visible_phrase(words,'Paused after an incomplete update')):
                (self.root/'logs'/f'{label}-history.json').write_text(json.dumps({
                    'reconnected':reconnected,'observations':observations},indent=2)+'\n')
                return
            buttons = [row for row in observed.get('runs',[]) if row.get('text','').strip().lower()=='reconnect'
                       and row.get('x',0)>260]
            if len(buttons)==1 and not reconnected:
                self.call('drive','click','--stale-ok',buttons[0]['ref'],timeout=30)
                reconnected = True
            time.sleep(.2)
        (self.root/'logs'/f'{label}-history-failed.json').write_text(json.dumps(observations,indent=2)+'\n')
        self.call('shot',f'{label}-history-failed',timeout=45,check=False)
        raise RuntimeError('paused history did not show its recovery explanation and Resume control')

    def capture_console(self, side):
        """Select the seeded console by canonical identity in both windows."""
        seed = next((item for item in json.loads((self.root/'seeds.json').read_text())['items']
                     if item['state']=='console'),None)
        if seed is None:
            return
        agent = seed['response']['data']['agent']
        _, response = self.cli('--no-start','agent','list')
        status = next(row for row in response['data']['agents'] if row['agent']['id']==agent['id'])
        console = status['agent'].get('console')
        if not console or status['state'] != 'Running' or not status.get('run'):
            raise RuntimeError('the seeded console is not Running with a live run')
        original = (json.loads((self.root/'ui-console-before.json').read_text()) if side=='after' else None)
        if original and (status['run'] != original['run'] or console['id'] != original['terminal']):
            raise RuntimeError('the update replaced the console run or terminal identity')
        _, response = self.cli('--no-start','channel','list')
        channel_id = status['agent']['placement']['Channel']
        title = next(row['title'] for row in response['data']['channels'] if row['id']==channel_id)
        views = []
        for slot, window in sorted(self.mapped().items()):
            if slot not in ('main','second'):
                continue
            self.call('wm','action','focus-window','--id',window['id'])
            _, text = self.call('drive','--json','observe',timeout=30)
            observation = json.loads(text)
            if observation['window']['id'] != window['id']:
                raise RuntimeError('console navigation selected another window')
            self.call('drive','click','--stale-ok',sidebar_channel_ref(observation,title),timeout=30)
            self.call('drive','move','850,750',timeout=30)
            data = self.root/'home/.local/share/cagents'
            database = data/'sessions.sqlite3' if slot=='main' else data/'windows'/slot/'sessions.sqlite3'
            deadline = time.monotonic()+15
            selected = None
            while time.monotonic()<deadline:
                with closing(sqlite3.connect(database.as_uri()+'?mode=ro',uri=True,timeout=1)) as db:
                    db.execute('PRAGMA query_only=ON')
                    selected = db.execute(
                        'SELECT c.workspace_id,c.card_id,t.terminal_id FROM cards c '
                        'JOIN workspaces w ON w.workspace_id=c.workspace_id '
                        'JOIN terminal_recipes t ON t.card_id=c.card_id '
                        'WHERE c.card_id=? AND w.focused_card_id=c.card_id AND c.kind_tag=? '
                        'AND c.existence_json=? AND t.terminal_id=?',
                        [agent['id'],'terminal','"open"',console['id']]).fetchone()
                    if selected:
                        active = db.execute('SELECT value FROM metadata WHERE key=?',
                                            [f'workspace:{selected[0]}:navigation:active_thread']).fetchone()
                        if not active or json.loads(active[0]) != channel_id:
                            selected = None
                if selected:
                    break
                time.sleep(.1)
            if not selected:
                raise RuntimeError(f'{slot} did not select the original console pane')
            _, text = self.call('drive','--json','observe',timeout=30)
            observed = json.loads(text)
            if (observed['window']['id'] != window['id'] or not observed.get('painted')
                    or observed['window'].get('focused') is False):
                raise RuntimeError('console capture did not show the focused native window')
            shot = f'{side}-console-{slot}'
            self.call('shot',shot,timeout=45)
            views.append({'slot':slot,'selection':selected,'observation':observed,
                          'shot':f'shots/{shot}.webp'})
        if len(views)!=2:
            raise RuntimeError('both windows are required for console evidence')
        result = {'agent':agent['id'],'terminal':console['id'],'run':status['run'],'state':status['state'],'views':views}
        (self.root/f'ui-console-{side}.json').write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')

    def watch_checkpoint(self):
        import hashlib
        import threading
        self.checkpoint_stop = threading.Event()
        self.checkpoint_observations = []
        def watch():
            path = self.root/'home/.local/share/cagents/daemon-upgrade/daemon-upgrade.json'
            previous = None
            while not self.checkpoint_stop.is_set():
                try:
                    raw = path.read_bytes()
                    phase = json.loads(raw)['phase']
                    digest = hashlib.sha256(raw).hexdigest()
                except FileNotFoundError:
                    raw, phase, digest = None,'absent',None
                except (OSError,ValueError,KeyError):
                    self.checkpoint_stop.wait(.02)
                    continue
                observed = phase,digest
                if observed!=previous:
                    self.checkpoint_observations.append({'time':time.time(),'phase':phase,'sha256':digest})
                    previous = observed
                copy = self.root/'logs/checkpoint-first-prepared.json'
                if phase=='Prepared' and not copy.exists():
                    copy.write_bytes(raw)
                self.checkpoint_stop.wait(.02)
        self.checkpoint_thread = threading.Thread(target=watch,daemon=True)
        self.checkpoint_thread.start()

    def finish_checkpoint_watch(self):
        if not hasattr(self,'checkpoint_stop'):
            return
        self.checkpoint_stop.set()
        self.checkpoint_thread.join(2)
        (self.root/'logs/checkpoint-observations.json').write_text(json.dumps(self.checkpoint_observations,indent=2)+'\n')

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
            settings = {'pause': args.codex_pause,
                        'never_confirm_interrupt': args.codex_never_confirm_interrupt,
                        'never_confirm_close': args.codex_never_confirm_close}
            (self.root/'codex-fixture.json').write_text(json.dumps(settings,sort_keys=True)+'\n')
            (self.root/'rehearsal-target.json').write_text(json.dumps({'version':args.new_version,'working':args.working})+'\n')
            self.call('install', args.old_dir, args.old_version, args.old_installer, timeout=180)
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
            self.capture_console('before')
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
            (self.root/'rehearsal-target.json').write_text(json.dumps({
                'version':args.new_version,'working':args.working,'started':time.time()})+'\n')
            self.watch_checkpoint()
            code, _ = self.call('update',args.new_dir,args.new_version,args.new_installer,*update_args,timeout=300,check=False)
            self.finish_checkpoint_watch()
            if args.require_prepared:
                prepared = self.root/'logs/checkpoint-first-prepared.json'
                baseline = json.loads((self.root/'records/before/snapshot.json').read_text())
                if (not prepared.exists() or json.loads(prepared.read_text()).get('proof')!=baseline['serving']
                        or not self.checkpoint_observations or self.checkpoint_observations[0]['phase']!='absent'):
                    self.errors.append('this fresh job did not produce the required source-bound Prepared checkpoint')
            target = json.loads((self.root/'rehearsal-target.json').read_text())
            target['finished'] = time.time()
            (self.root/'rehearsal-target.json').write_text(json.dumps(target)+'\n')
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
            elif args.expect_refusal:
                if not code or code==124:
                    self.errors.append('expected refusal did not return a normal nonzero result')
            elif code:
                self.errors.append(f'update exited {code}')
            try:
                self.wait_windows(['main','second'],timeout=args.reopen_timeout,version=args.old_version if args.fail_new_daemon or args.expect_refusal else args.new_version if code==0 else None)
            except RuntimeError as exc:
                self.errors.append(str(exc))
            # The shared recovery verifier qualifies paused presentations before
            # inspecting them. Capture again afterwards to retain the final UI.
            self.call('record','after',timeout=150)
            self.capture_chats('after')
            self.capture_console('after')
            code, _ = self.call('record','after',timeout=150,check=False)
            if code:
                self.errors.append('after record has evidence gaps')
            if args.expect_console_refusal:
                self.errors.extend(refusal_preserved_processes(self.root/'records'))
            if args.expect_refusal:
                self.errors.extend(expected_refusal(self.root,args.expect_refusal,args.old_version))
            self.observe(self.root/'records/after/windows.json',shots='after')
            if args.plain_restart:
                import rehearse_update_restart
                recovered = self.root/'recovered'
                recovered.mkdir()
                for name in ('ui-chats-after.json','ui-console-after.json'):
                    source = self.root/name
                    if source.exists():
                        shutil.copyfile(source,recovered/name)
                for name in ('records/after','shots'):
                    shutil.copytree(self.root/name,recovered/name)
                rehearse_update_restart.restart(self,args.new_version)
                self.observe(self.root/'records/after/windows.json',shots='after-restart')
            code, _ = self.call('compare','before','after',timeout=30,check=False)
            if code:
                self.errors.append('comparison found regressions or missing evidence; see comparison step')
        except Exception as exc:
            failure = str(exc)
            self.errors.append(failure)
            # Preserve an after inventory even when an earlier operation failed.
            self.call('record','after',timeout=150,check=False)
        finally:
            self.finish_checkpoint_watch()
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
            for side in ('before','after'):
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
    run.add_argument('--reopen-timeout',type=float,default=45)
    run.add_argument('--expect-refusal',choices=('interrupt','cancel'),help='require the exact refusal and unchanged old service, install, windows and inventory')
    run.add_argument('--require-prepared',action='store_true',help='require this fresh job to reach source-bound Prepared before recovery')
    run.add_argument('--plain-restart',action='store_true',help='after verified recovery, crash only the private daemon and start it normally; verify the original census again')
    run.add_argument('--codex-pause',type=float,default=6,help='offline Codex turn pause in seconds (maximum 300)')
    run.add_argument('--codex-never-confirm-interrupt',action='store_true',help='offline peer acknowledges interrupt without settling the turn')
    run.add_argument('--codex-never-confirm-close',action='store_true',help='offline peer retains its connection to force Prepared recovery')
    seed_options(run)
    seed_options(commands.add_parser('seed'))
    wait=commands.add_parser('wait')
    wait.add_argument('names',nargs='+')
    observe=commands.add_parser('observe')
    observe.add_argument('output')
    args=parser.parse_args()
    if args.command == 'run' and not 0 < args.codex_pause <= 300:
        parser.error('--codex-pause must be between 0 and 300 seconds')
    if args.command == 'run' and args.plain_restart and (args.expect_refusal or args.fail_new_daemon):
        parser.error('--plain-restart requires a successful recovery scenario')
    if args.command == 'run' and args.expect_console_refusal and (not args.fail_new_daemon or args.no_console):
        parser.error('--expect-console-refusal requires --fail-new-daemon and a seeded console')
    rehearsal=Rehearsal(args.root,args.shell)
    try:
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
