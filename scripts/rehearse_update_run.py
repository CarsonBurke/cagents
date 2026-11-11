#!/usr/bin/python3
"""Scenario orchestration for rehearse-update; all application commands use its private env."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time


def selected_chat(root, slot, card_id, channel_id, expected):
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
        if (descriptor.get('id') != expected['id']
                or launch.get('provider') != expected['launch']['provider']
                or launch.get('session_id') != expected['launch'].get('session_id')
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
                self.call('drive','click',title,timeout=30)
                deadline = time.monotonic()+15
                selected = None
                while time.monotonic()<deadline:
                    _, status = self.cli('--no-start','agent','status',original['id'])
                    actual = status['data']['agent']['session']['Chat']
                    if actual['id'] != chat['id'] or actual['launch']['provider'] != chat['launch']['provider']:
                        raise RuntimeError('opening a seeded chat allocated a different session')
                    try:
                        selected = selected_chat(self.root,slot,original['id'],channel_id,actual)
                    except (sqlite3.Error, OSError):
                        selected = None
                    if selected:
                        break
                    time.sleep(.1)
                if not selected:
                    raise RuntimeError(f'{slot} did not focus exact seeded {provider} card {original["id"]}')
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
        except Exception as exc:
            failure = str(exc)
            self.errors.append(failure)
            # Preserve an after inventory even when an earlier operation failed.
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
    seed_options(run)
    seed_options(commands.add_parser('seed'))
    wait=commands.add_parser('wait')
    wait.add_argument('names',nargs='+')
    observe=commands.add_parser('observe')
    observe.add_argument('output')
    args=parser.parse_args()
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
