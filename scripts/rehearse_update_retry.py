#!/usr/bin/python3
"""Two installed update attempts in one live private rehearsal.

Run from a genuine user shell. This module never clears recovery checkpoints,
replaces staged binaries or manufactures verification receipts.
"""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import sys
import time

import rehearse_update_pty as pty
import rehearse_update_records as records
import rehearse_update_restart as restart
import rehearse_update_run as scenario


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def sha256(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def fixture_sources():
    repository = Path(__file__).resolve().parents[1]
    names = ('scripts/rehearse-update', 'scripts/rehearse_update_retry.py', 'scripts/rehearse_update_pty.py',
             'scripts/rehearse_update_run.py', 'scripts/rehearse_update_records.py', 'scripts/rehearse_update_restart.py',
             'tests/fixtures/rehearsal-startup-bind.c', 'tests/fixtures/rehearsal-codex.py',
             'tests/fixtures/codex-tool-rows.py', 'tests/fixtures/chat-composer-claude.sh')
    return {name: sha256(repository / name) for name in names}


def printed_apply_command(output, candidate):
    """Follow only the exact installed command the stage-only installer prints."""
    commands = re.findall(r'Run "([^"\r\n]+)" daemon upgrade to apply it when ready\.', output)
    if commands != [str(candidate)]:
        raise RuntimeError('staging did not print exactly the selected candidate apply command')
    return [commands[0], 'daemon', 'upgrade']


def review_prompts(output, cancelled):
    reviews = re.findall(r'(?:^|[\r\n])Update cagents [^\r\n]+ -> [^\r\n]+', output)
    prompts = []
    for line in output.splitlines():
        line = line.strip()
        if not line.startswith('[rehearsal prompt='):
            continue
        if not line.endswith(']'):
            raise RuntimeError('incomplete controlling-PTY prompt marker')
        prompt, answer = line[len('[rehearsal prompt='):-1].rsplit(' answer=', 1)
        prompt, answer = ast.literal_eval(prompt), ast.literal_eval(answer)
        if not isinstance(prompt, str) or not isinstance(answer, str):
            raise RuntimeError('invalid controlling-PTY prompt marker')
        matched = pty.PROMPTS.fullmatch(prompt.encode()) or pty.PROMPTS.fullmatch(prompt.encode() + b'\n')
        if matched is None:
            raise RuntimeError('prompt does not match the controlling-PTY grammar')
        kind = matched.lastgroup
        expected = ('' if kind == 'stop' else 'no' if kind == 'confirm' else 'n') if cancelled else ('yes' if kind == 'confirm' else 'stop' if kind == 'stop' else '')
        if answer != expected:
            raise RuntimeError('retry transcript contains unexpected consent')
        prompts.append({'kind': kind, 'prompt': prompt, 'answer': answer})
    if len(reviews) != 1 or not prompts:
        raise RuntimeError('each attempt must contain one full review and observed consent')
    return prompts


def reviewed_inventory(output, directory, baseline, from_version, to_version):
    """Bind the rendered full review to the real pre-answer confirmation."""
    files = sorted(directory.glob('prompt-*.status.json'))
    if not files:
        raise RuntimeError('attempt did not retain pre-answer confirmation bytes')
    jobs = [records.read_json(path) for path in files]
    for job in jobs:
        preview = job['confirmation']['preview']
        if (job.get('version') != to_version or preview.get('from_version') != from_version
                or preview.get('candidate', {}).get('version') != to_version):
            raise RuntimeError('review source or selected target changed')
    if len({job['id'] for job in jobs}) != 1:
        raise RuntimeError('prompt confirmation crossed update jobs')
    preview = jobs[0]['confirmation']['preview']
    header = f'Update cagents {from_version} -> {to_version}'
    headers = list(re.finditer(rf'^{re.escape(header)}\r?$', output, re.M))
    if len(headers) != 1:
        raise RuntimeError('transcript did not show the exact source and selected target once')
    body = output[headers[0].end():].split('[rehearsal prompt=', 1)[0]
    sessions = (preview.get('daemon') or {}).get('sessions', [])
    ids = set(preview.get('labels', {})) | set(preview.get('review_details', {}).get('panes', {}))
    ids.update(row['id'] for row in sessions)
    ids.update(row['pane'] for row in preview.get('stopped', []))
    expected = set(baseline['terminals']) | {row['id'] for row in baseline['sessions'].values()}
    if not expected <= ids:
        raise RuntimeError('review omitted original conversations or terminals')
    counts = re.findall(r'^\s{2}(?:Keep running|Resumes|Unchanged|Working now|Interrupted and resumed|Come back paused|Needs attention|Stops) \((\d+)\)', body, re.M)
    if sum(map(int, counts)) != len(ids) or not ids:
        raise RuntimeError('visible review groups did not account for the reviewed inventory')
    windows = preview.get('windows', 0)
    window_action = 'window closes and reopens' if windows == 1 else 'windows close and reopen'
    if windows and f'{windows} {window_action}' not in body:
        raise RuntimeError('visible review omitted its window action')
    labels = preview.get('labels', {})
    expanded = [row['label'] for row in preview.get('stopped', [])]
    expanded += [labels.get(row['id'], row['title']) for row in sessions
                 if row['disposition'] in ('waits_for_idle', 'interrupted', 'resume_picker', 'blocked')]
    if any(label not in body for label in expanded):
        raise RuntimeError('visible review omitted an expanded work or recovery label')
    return jobs[0]['id']


def workload_preserved(before, after):
    regressions = [row for row in records.compare(before, after) if row['regression']]
    if regressions:
        raise RuntimeError(f'workload preservation failed: {regressions}')
    workload_identities(before, after)


def workload_identities(before, after):
    for collection in records.COLLECTIONS:
        if before[collection].keys() != after[collection].keys():
            raise RuntimeError(f'retry changed {collection} membership')
    if records.panes(before).keys() != records.panes(after).keys():
        raise RuntimeError('retry changed saved pane membership')
    for key, agent in before['agents'].items():
        if not agent.get('run') or agent['run'] != after['agents'][key].get('run'):
            raise RuntimeError('retry replaced an accepted agent run')


def activity(root):
    value = scenario.provider_activity(root)
    events = [json.loads(line) for line in (root / 'logs/codex-provider.jsonl').read_text().splitlines()]
    value['codex_thread_resumes'] = sum(row.get('event') == 'request' and row.get('method') == 'thread/resume' for row in events)
    return value


def require_no_replay(before, after):
    if before != after:
        raise RuntimeError(f'retry launched or replayed provider work: {before} -> {after}')


def archive_job(root, destination, expected_phase):
    """Copy the actual current job before a later attempt overwrites status."""
    state = root / 'home/.local/share/cagents-install/.update'
    job = records.read_json(state / 'status.json')
    identifier = job.get('id')
    if not isinstance(identifier, str) or not re.fullmatch(r'[0-9a-f-]{36}', identifier):
        raise RuntimeError('invalid update job identity')
    if job.get('phase') != expected_phase:
        raise RuntimeError(f'expected {expected_phase}, found {job.get("phase")}')
    log = state / f'{identifier}.log'
    if Path(job['log_path']) != log or log.is_symlink() or not log.is_file():
        raise RuntimeError('job log does not belong to the private transaction')
    destination.mkdir()
    for path in [state / 'status.json', *state.glob(identifier + '.*')]:
        if path.is_symlink() or not path.is_file():
            raise RuntimeError('job evidence is not a regular private file')
        shutil.copy2(path, destination / path.name)
    return job


def require_restored(root, before, after, old_version, cancelled):
    """No fault may be released until the real service and full workload qualify."""
    workload_preserved(before, after)
    prefix = root / 'home/.local/share/cagents-install'
    if (prefix / 'current').resolve() != prefix / 'versions' / old_version:
        raise RuntimeError('first attempt did not restore the old current')
    executable = prefix / 'versions' / old_version / 'bin/cagentsd'
    pid = restart.owned_daemon(root, executable, after['serving'])
    if cancelled:
        if after['serving'] != before['serving']:
            raise RuntimeError('review cancellation replaced the serving daemon')
        def uis(snapshot):
            return {(row['pid'], row['start_ticks']) for row in snapshot['processes']
                    if Path(row.get('executable', '')).name == 'cagents-ui'}
        if uis(before) != uis(after) or not uis(before):
            raise RuntimeError('review cancellation replaced the original windows')
    elif records.verified_recovery(before, after) is None:
        raise RuntimeError('failed startup lacks full source-bound usable recovery')
    return pid


def bind_decisions(root):
    result = []
    for line in (root / 'logs/startup-bind.tsv').read_text().splitlines():
        pid, executable, address, decision = line.split('\t')
        if not pid.isdigit() or int(pid) <= 1:
            raise RuntimeError('invalid startup bind fixture PID')
        result.append({'pid': int(pid), 'executable': executable, 'socket': address, 'decision': decision})
    return result


def verify_bind_fixture(root):
    directory = root / '.fixture'
    manifest = records.read_json(directory / 'manifest.json')
    library = directory / 'startup-bind.so'
    if library.is_symlink() or sha256(library) != manifest['library_sha256']:
        raise RuntimeError('private startup bind injector bytes changed')
    return manifest['library_sha256']


def disarm_after_restore(root, before, after, old_version, original_activity, candidate, job_log, diagnostics):
    """Keep the fault armed if any independent recovery or workload check fails."""
    pid = require_restored(root, before, after, old_version, cancelled=False)
    require_no_replay(original_activity, activity(root))
    verify_bind_fixture(root)
    decisions = bind_decisions(root)
    serving_socket = str(root / 'rt/cagents/cagentsd.sock')
    failed = [row for row in decisions if row['executable'] == str(candidate / 'bin/cagentsd')
              and row['socket'] == serving_socket and row['decision'] == 'fail-armed']
    restored = [row for row in decisions if row['pid'] == pid and row['socket'] == serving_socket
                and row['executable'] == str(root / 'home/.local/share/cagents-install/versions' / old_version / 'bin/cagentsd')
                and row['decision'] == 'pass-source']
    if not failed or not restored or not re.search(r'daemon exited before becoming ready', job_log, re.I) or 'exit status: 1' not in job_log:
        raise RuntimeError('missing exact injected bind failure, product error or restored source bind')
    products = diagnostics.get('data', {}).get('products', []) if diagnostics.get('ok') is True else []
    daemon = next((row for row in products if row.get('product') == 'daemon'), {})
    if daemon.get('readable') is not True:
        raise RuntimeError('product startup diagnostics are unavailable')
    for failure in failed:
        events = [row for row in daemon.get('records', [])
                  if row.get('pid') == failure['pid'] and row.get('version') == candidate.name]
        starts = {row['run'] for row in events if row.get('event', {}).get('kind') == 'startup_failed'}
        exits = {row['run'] for row in events if row.get('event') == {'kind': 'exit', 'success': False}}
        if not starts & exits or any(row.get('event', {}).get('kind') == 'panic' for row in events):
            raise RuntimeError('injected failure did not follow the product normal startup-error exit')
    sentinel = root / '.fixture/fail-new-daemon.armed'
    if sentinel.is_symlink() or not sentinel.is_file() or sentinel.read_text() != str(candidate / 'bin/cagentsd') + '\n':
        raise RuntimeError('startup fault sentinel changed before explicit release')
    release = root / 'retry/first/disarm-authorized.json'
    with release.open('x') as stream:
        json.dump({'event': 'explicit_fixture_disarm', 'time': time.time(), 'source_pid': pid,
                   'serving': after['serving'], 'bind_failures': failed, 'source_binds': restored,
                   'bind_records_before_disarm': len(decisions)}, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    sentinel.rename(sentinel.with_suffix('.disarmed'))
    release.rename(root / 'retry/first/disarm.json')
    return len(decisions)


def install_bind_fixture(rehearsal, candidate):
    if socket.gethostname().split('.')[0] != 'fw16':
        raise RuntimeError('compile the startup bind fixture only in the supervised fw16 shell')
    directory = rehearsal.root / '.fixture'
    directory.mkdir(mode=0o700)
    source = Path(__file__).resolve().parents[1] / 'tests/fixtures/rehearsal-startup-bind.c'
    shutil.copy2(source, directory / 'startup-bind.c')
    output = directory / 'startup-bind.next.so'
    rehearsal.call('env', 'cc', '-shared', '-fPIC', '-Wall', '-Wextra', '-Werror',
                   '-DRETRY_ROOT=' + json.dumps(str(rehearsal.root)),
                   '-DRETRY_CANDIDATE=' + json.dumps(str(candidate / 'bin/cagentsd')),
                   directory / 'startup-bind.c', '-o', output, '-ldl', timeout=60)
    output.chmod(0o700)
    output.rename(directory / 'startup-bind.so')
    sentinel = directory / 'fail-new-daemon.armed'
    with sentinel.open('x') as stream:
        stream.write(str(candidate / 'bin/cagentsd') + '\n')
    sentinel.chmod(0o600)
    write_json(directory / 'manifest.json', {'source_sha256': sha256(source),
                                           'library_sha256': sha256(directory / 'startup-bind.so'),
                                           'candidate': str(candidate / 'bin/cagentsd')})


class RetryRehearsal(scenario.Rehearsal):
    def record(self, name):
        self.call('record', name, timeout=150)
        return records.read_json(self.root / 'records' / name / 'snapshot.json')

    def attempt(self, name, command, cancelled, baseline):
        destination = self.root / 'retry' / name
        destination.mkdir()
        started = time.time()
        write_json(self.root / 'rehearsal-target.json', {'version': self.version, 'working': 'n' if cancelled else '', 'started': started})
        with records.WindowCloseMonitor(self.root) as monitor:
            code, output = self.call('app', '/usr/bin/python3', Path(__file__).with_name('rehearse_update_pty.py'),
                                     '--answer', 'no' if cancelled else 'yes', '--working', 'n' if cancelled else '',
                                     '--review-root', self.root, '--review-output', destination / 'previews',
                                     '--stop', '' if cancelled else 'stop', '--', *command, timeout=300, check=False)
            monitor.command_exit = code
        (destination / 'transcript.txt').write_text(output)
        shutil.move(self.root / 'window-close-trace.jsonl', destination / 'window-close-trace.jsonl')
        write_json(self.root / 'rehearsal-target.json', {'version': self.version, 'working': 'n' if cancelled else '',
                                                       'started': started, 'finished': time.time()})
        prompts = review_prompts(output, cancelled)
        job = reviewed_inventory(output, destination / 'previews', baseline, self.old_version, self.version)
        write_json(destination / 'intent.json', {'exit': code, 'command': command, 'job': job, 'prompts': prompts})
        return code, destination, job

    def run_retry(self, args):
        if self.root.exists():
            raise RuntimeError('retry requires a fresh absent root; both attempts then share it')
        failure = None
        self.version = args.new_version
        self.old_version = args.old_version
        try:
            self.call('init')
            (self.root / 'retry').mkdir()
            sources = fixture_sources()
            write_json(self.root / 'retry/source.json', sources)
            write_json(self.root / 'codex-fixture.json', {'pause': args.codex_pause})
            self.call('install', args.old_dir, args.old_version, args.old_installer, timeout=180)
            self.call('desktop', 'start', '--limit', '1200', timeout=None)
            for slot in ('main', 'second'):
                self.call('launch', slot)
            self.wait_windows(['main', 'second'])
            self.call('seed', *(['--no-console'] if args.no_console else []), timeout=180)
            if args.ceditor:
                self.call('ceditor', 'start', args.ceditor, timeout=60)
                self.wait_windows(['main', 'second', 'ceditor'])
            self.capture_chats('before')
            self.capture_console('before')
            before = self.record('before')
            original_activity = activity(self.root)
            prefix = self.root / 'home/.local/share/cagents-install'
            candidate = prefix / 'versions' / self.version
            archive = Path(args.new_dir) / f'cagents-{self.version}-x86_64-unknown-linux-gnu.tar.gz'
            archive_digest = sha256(archive)
            _, staged = self.call('install', args.new_dir, self.version, args.new_installer, '--stage-only', timeout=180)
            command = printed_apply_command(staged, candidate / 'bin/cagents')
            (self.root / 'retry/staging.txt').write_text(staged)
            binaries = {name: sha256(candidate / 'bin' / name) for name in ('cagents', 'cagents-ui', 'cagentsd')}
            if args.mode == 'startup':
                install_bind_fixture(self, candidate)
                verify_bind_fixture(self.root)
            cancelled = args.mode == 'cancel'
            code, first_dir, first_review = self.attempt('first', command, cancelled=cancelled, baseline=before)
            if code in (0, 124):
                raise RuntimeError('first attempt did not return the expected normal refusal')
            first_job = archive_job(self.root, first_dir / 'job', 'Cancelled' if cancelled else 'Failed')
            if first_job['id'] != first_review:
                raise RuntimeError('first terminal job differs from its reviewed confirmation')
            self.wait_windows(['main', 'second'], version=args.old_version, timeout=60)
            self.record('after')
            self.capture_chats('after')
            self.capture_console('after')
            restored = self.record('after')
            shutil.copytree(self.root / 'records/after', self.root / 'records/after-first')
            for name in ('ui-chats-after.json', 'ui-console-after.json'):
                if (self.root / name).exists():
                    shutil.copy2(self.root / name, first_dir / name)
                    shutil.copy2(self.root / name, self.root / name.replace('-after.json', '-after-first.json'))
            shutil.copytree(self.root / 'shots', first_dir / 'shots')
            require_restored(self.root, before, restored, args.old_version, cancelled=cancelled)
            require_no_replay(original_activity, activity(self.root))
            pending_code, pending_output = self.call('env', candidate / 'bin/cagents', '--no-start', '--json',
                                                     'daemon', 'inspect-pending-update-recovery',
                                                     '--candidate-cli', candidate / 'bin/cagents', check=False)
            (first_dir / 'pending-recovery-inspection.json').write_text(pending_output)
            if pending_code:
                raise RuntimeError('retained recovery is unresolved; follow the product command in pending-recovery-inspection.json')
            inspected = json.loads(pending_output)
            if inspected.get('ok') is not True or inspected.get('data', {}).get('pending') is not False:
                raise RuntimeError('product did not confirm that retained recovery is resolved')
            if binaries != {name: sha256(candidate / 'bin' / name) for name in binaries} or sha256(archive) != archive_digest:
                raise RuntimeError('archive or candidate bytes changed before fault release')
            write_json(first_dir / 'qualification.json', {'job': first_job['id'], 'activity': original_activity,
                                                         'restored_serving': restored['serving'], 'candidate_sha256': binaries,
                                                         'archive_sha256': archive_digest})
            if not cancelled:
                _, diagnostic_output = self.call('env', candidate / 'bin/cagents', '--no-start', '--json', 'diagnostics')
                (first_dir / 'diagnostics.json').write_text(diagnostic_output)
                released_at = disarm_after_restore(self.root, before, restored, args.old_version, original_activity, candidate,
                                                   (first_dir / 'job' / f'{first_job["id"]}.log').read_text(), json.loads(diagnostic_output))
            # The actual candidate command printed by staging is used unchanged.
            code, second_dir, second_review = self.attempt('second', command, cancelled=False, baseline=restored)
            if code:
                raise RuntimeError(f'second attempt failed with exit {code}')
            second_job = archive_job(self.root, second_dir / 'job', 'Complete')
            if second_job['id'] != second_review:
                raise RuntimeError('second terminal job differs from its reviewed confirmation')
            if first_job['id'] == second_job['id']:
                raise RuntimeError('second attempt reused the first transaction')
            self.wait_windows(['main', 'second'], version=self.version, timeout=60)
            self.record('after')
            self.capture_chats('after', baseline='after-first')
            self.capture_console('after')
            after = self.record('after')
            # The second receipt is bound to the restored source, not its
            # pre-failure predecessor. Preserve that proof chain; separately
            # require original membership and accepted runs across both hops.
            workload_preserved(restored, after)
            workload_identities(before, after)
            require_no_replay(original_activity, activity(self.root))
            if records.verified_recovery(restored, after) is None:
                raise RuntimeError('second attempt lacks fresh source-bound usable recovery')
            if (prefix / 'current').resolve() != candidate:
                raise RuntimeError('second attempt did not publish the selected target')
            successor = restart.owned_daemon(self.root, candidate / 'bin/cagentsd', after['serving'])
            if binaries != {name: sha256(candidate / 'bin' / name) for name in binaries} or sha256(archive) != archive_digest:
                raise RuntimeError('candidate bytes changed between attempts')
            if not cancelled:
                verify_bind_fixture(self.root)
                decisions = bind_decisions(self.root)[released_at:]
                if not any(row['pid'] == successor and row['executable'] == str(candidate / 'bin/cagentsd')
                           and row['socket'] == str(self.root / 'rt/cagents/cagentsd.sock')
                           and row['decision'] == 'pass-disarmed' for row in decisions):
                    raise RuntimeError('second attempt did not bind through the disarmed fixture')
            write_json(second_dir / 'qualification.json', {'job': second_job['id'], 'activity': original_activity,
                                                          'serving': after['serving'], 'candidate_sha256': binaries,
                                                          'archive_sha256': archive_digest})
            if fixture_sources() != sources:
                raise RuntimeError('retry fixture source changed during native execution')
        except Exception as error:
            failure = str(error)
        finally:
            if self.root.exists():
                blocked = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
                try:
                    stop, output = self.call('stop', timeout=150, check=False)
                    if stop:
                        failure = (failure or '') + f'; private cleanup failed: {output[-1000:]}'
                    write_json(self.root / 'retry-result.json', {'mode': args.mode, 'passed': failure is None,
                                                                'error': failure, 'steps': self.steps})
                    print(f'Retry {args.mode}: {"PASS" if failure is None else "FAIL"}; {self.root / "retry-result.json"}')
                    if failure:
                        print(failure, file=sys.stderr)
                finally:
                    signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
        return int(failure is not None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--mode', choices=('cancel', 'startup'), required=True)
    for prefix in ('old', 'new'):
        for name in ('dir', 'version', 'installer'):
            parser.add_argument(f'--{prefix}-{name}', required=True)
    parser.add_argument('--ceditor')
    parser.add_argument('--no-console', action='store_true')
    parser.add_argument('--codex-pause', type=float, default=90)
    args = parser.parse_args()
    if not 0 < args.codex_pause <= 300:
        parser.error('--codex-pause must be between 0 and 300 seconds')
    if any(name in os.environ for name in ('CAGENTS_AGENT_AUTHORITY', 'CAGENTS_AGENT_TOKEN', 'CAGENTS_SESSION_TOKEN')):
        parser.error('run from a genuine user shell such as plain ssh fw16; authority was not modified')
    def interrupted(signum, frame):
        raise RuntimeError(f'interrupted by signal {signum}')
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, interrupted)
    rehearsal = RetryRehearsal(args.root.resolve(), str(Path(__file__).with_name('rehearse-update')))
    try:
        return rehearsal.run_retry(args)
    except Exception as error:
        print(f'rehearse-update retry: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
