#!/usr/bin/python3
"""Pure retry orchestration contracts; no build, daemon or compositor."""
import copy
import io
import json
from pathlib import Path
import sys
import tempfile
import subprocess
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import rehearse_update_retry as retry


class RetryAdmission(unittest.TestCase):
    def test_only_exact_installer_command_can_select_retry(self):
        candidate = Path('/private/versions/new/bin/cagents')
        printed = f'Staged cagents new; current preserved. Run "{candidate}" daemon upgrade to apply it when ready.\n'
        self.assertEqual(retry.printed_apply_command(printed, candidate), [str(candidate), 'daemon', 'upgrade'])
        for bad in ('', printed + printed, printed.replace('/new/', '/other/')):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                retry.printed_apply_command(bad, candidate)

    def test_each_attempt_requires_one_review_and_actual_prompt_consent(self):
        header = 'Update cagents old -> new\r\n'
        confirm = '[rehearsal prompt=\'Proceed? [Y/n] \' answer=\'no\']\n'
        self.assertEqual(retry.review_prompts(header + confirm, True)[0]['kind'], 'confirm')
        accepted = confirm.replace("answer='no'", "answer='yes'")
        self.assertEqual(retry.review_prompts(header + accepted, False)[0]['answer'], 'yes')
        idle = "[rehearsal prompt='Enter: review what stops   n: cancel' answer='yes']\n"
        self.assertEqual(retry.review_prompts(header + idle, False)[0]['kind'], 'confirm')
        self.assertEqual(retry.review_prompts(header + idle.replace("answer='yes'", "answer='no'"), True)[0]['kind'], 'confirm')
        for bad in (header, header + header + confirm, header + accepted,
                    header + idle.replace("answer='yes'", "answer='i'"),
                    header + "[rehearsal prompt='unrecognized' answer='no']\n"):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                retry.review_prompts(bad, True)

    def test_provider_replay_is_a_hard_failure(self):
        before = {'claude_chat_launches': 3, 'codex_thread_resumes': 0}
        retry.require_no_replay(before, dict(before))
        for key in before:
            after = {**before, key: before[key] + 1}
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                retry.require_no_replay(before, after)

    def test_changed_run_or_membership_is_not_covered_by_a_green_comparison(self):
        snapshot = {name: {} for name in retry.records.COLLECTIONS}
        snapshot.update(agents={'a': {'run': 'accepted'}}, stores={})
        with patch.object(retry.records, 'compare', return_value=[]):
            retry.workload_preserved(snapshot, copy.deepcopy(snapshot))
            changed = copy.deepcopy(snapshot)
            changed['agents']['a']['run'] = 'new-run'
            with self.assertRaisesRegex(RuntimeError, 'accepted agent run'):
                retry.workload_preserved(snapshot, changed)
            changed = copy.deepcopy(snapshot)
            changed['terminals']['extra'] = {}
            with self.assertRaisesRegex(RuntimeError, 'membership'):
                retry.workload_preserved(snapshot, changed)

    def test_console_relaunch_uses_each_hops_real_source_receipt(self):
        root = '/private'
        def proof(incarnation):
            return {'host_session_id': 'host', 'boot_id': 'boot', 'host_incarnation': incarnation}
        def snapshot(incarnation, version):
            program = f'{root}/home/.local/share/cagents-install/versions/{version}/bin/cagents'
            return {'root': root, 'errors': [], 'serving': proof(incarnation), 'windows': {}, 'sessions': {},
                    'channels': {}, 'projects': {}, 'processes': [],
                    'terminals': {'terminal': {'program': program, 'state': 'running'}},
                    'agents': {'agent': {'agent': {'id': 'agent', 'console': {'id': 'terminal',
                        'launch': {'program': program, 'arguments': ['agent', 'console', 'agent']}}},
                        'run': 'original-run', 'state': 'Running'}},
                    'stores': {'sessions': {'rows': {'cards': [{'card_id': 'console', 'kind_tag': 'terminal',
                        'existence': 'open'}], 'terminal_recipes': [{'card_id': 'console', 'terminal_id': 'terminal'}]}}}}
        def bind_receipt(source, target, transaction):
            census = {'version': 1, 'transaction': transaction, 'source': source['serving']}
            receipt = {'version': 1, 'transaction': transaction, 'census_digest': transaction,
                       'outcome': {'state': 'usable', 'serving': target['serving']}}
            target['recovery'] = {'census': census, 'issued': receipt, 'current': receipt, 'digest': transaction}
            target['target'] = {'version': 'new'}
            target['update_status'] = {'id': transaction, 'version': 'new', 'phase': 'Complete'}
        before, restored, after = snapshot('original', 'old'), snapshot('restored', 'old'), snapshot('target', 'new')
        bind_receipt(before, restored, 'first')
        bind_receipt(restored, after, 'second')
        retry.workload_preserved(before, restored)
        retry.workload_preserved(restored, after)
        retry.workload_identities(before, after)
        with self.assertRaisesRegex(RuntimeError, 'workload preservation failed'):
            retry.workload_preserved(before, after)


class RawJobs(unittest.TestCase):
    def test_first_raw_job_survives_second_status_and_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / 'home/.local/share/cagents-install/.update'
            state.mkdir(parents=True)
            first, second = 'a' * 36, 'b' * 36
            for identifier, phase in ((first, 'Cancelled'), (second, 'Complete')):
                payload = {'id': identifier, 'phase': phase, 'log_path': str(state / f'{identifier}.log')}
                (state / 'status.json').write_text(json.dumps(payload))
                (state / f'{identifier}.log').write_text('raw-' + identifier)
                (state / f'{identifier}.census.json').write_text('verbatim-census-' + identifier)
                retry.archive_job(root, root / identifier, phase)
            self.assertEqual((root / first / f'{first}.log').read_text(), 'raw-' + first)
            self.assertEqual((root / first / f'{first}.census.json').read_text(), 'verbatim-census-' + first)
            self.assertEqual(json.loads((root / first / 'status.json').read_text())['phase'], 'Cancelled')
            self.assertEqual(json.loads((root / second / 'status.json').read_text())['phase'], 'Complete')

    def test_wrong_phase_or_external_log_cannot_qualify_an_attempt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / 'home/.local/share/cagents-install/.update'
            state.mkdir(parents=True)
            payload = {'id': 'a' * 36, 'phase': 'Failed', 'log_path': '/outside/log'}
            (state / 'status.json').write_text(json.dumps(payload))
            for phase in ('Failed', 'Cancelled'):
                with self.subTest(phase=phase), self.assertRaises(RuntimeError):
                    retry.archive_job(root, root / 'copy', phase)


class ReviewEvidence(unittest.TestCase):
    def test_child_exit_race_before_final_kill_still_reaps_the_reserved_pid(self):
        with patch.object(sys, 'argv', ['pty', '--timeout', '.1', '--', 'unused']), \
                patch.object(retry.pty.pty, 'fork', return_value=(123, 99)), \
                patch.object(retry.pty.time, 'monotonic', side_effect=[0, 1]), \
                patch.object(retry.pty.time, 'sleep'), \
                patch.object(retry.pty.os, 'killpg', side_effect=ProcessLookupError), \
                patch.object(retry.pty.os, 'waitpid', side_effect=[(0, 0)] * 30 + [(123, 0)]) as wait, \
                patch.object(retry.pty.os, 'close') as close, \
                patch.object(retry.pty.signal, 'pthread_sigmask', return_value=set()), \
                patch.object(sys, 'stdout', io.StringIO()):
            self.assertEqual(retry.pty.main(), 124)
        wait.assert_called_with(123, 0)
        close.assert_called_once_with(99)

    def test_second_native_capture_reads_the_restored_baseline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            agent = {'id': 'a', 'placement': {'Channel': 'c'},
                     'spec': {'provider': 'Claude', 'target': {'repository': '/private/project'}},
                     'session': {'Chat': {'launch': {'provider': 'Claude'}}}}
            retry.write_json(root / 'seeds.json', {'items': [{'state': 'named-chat', 'response': {'data': {'agent': agent}}}]})
            retry.write_json(root / 'ui-chats-after-first.json', [])
            for name in ('after-first', 'after'):
                (root / 'records' / name).mkdir(parents=True)
                retry.write_json(root / 'records' / name / 'snapshot.json', {'source': name})
            rehearsal = retry.scenario.Rehearsal(root, 'unused')
            responses = [(0, {'data': {'agents': [{'agent': agent}]}}),
                         (0, {'data': {'channels': [{'id': 'c', 'title': 'Chat'}]}})]
            with patch.object(rehearsal, 'cli', side_effect=responses), \
                    patch.object(rehearsal, 'mapped', side_effect=RuntimeError('native boundary reached')):
                with self.assertRaisesRegex(RuntimeError, 'native boundary reached'):
                    rehearsal.capture_chats('after', baseline='after-first')
            self.assertFalse((root / 'ui-chats-before.json').exists())

    def test_real_confirmation_is_copied_verbatim_before_consent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.rehearse-update').write_text(str(root))
            state = root / 'home/.local/share/cagents-install/.update'
            state.mkdir(parents=True)
            (root / 'retry/first').mkdir(parents=True)
            path = state / 'status.json'
            raw = b'{"id":"actual-job", "confirmation":{"nonce":"actual-nonce","preview":{}}}\n'
            path.write_bytes(raw)
            path.chmod(0o600)
            retry.pty.capture_review(root, root / 'retry/first/previews', 1)
            self.assertEqual((root / 'retry/first/previews/prompt-1.status.json').read_bytes(), raw)
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                retry.pty.capture_review(root, root / 'retry/first/previews', 2)

    def test_truncated_wrong_target_or_missing_workload_review_cannot_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary)
            baseline = {'terminals': {'t': {}}, 'sessions': {'chat': {'id': 'c'}}}
            preview = {'from_version': 'old', 'candidate': {'version': 'new'}, 'windows': 2,
                       'daemon': {'sessions': [{'id': 'c', 'title': 'Chat', 'disposition': 'waits_for_idle'},
                                               {'id': 't', 'title': 'Shell', 'disposition': 'continues'}]}}
            path = folder / 'prompt-1.status.json'
            raw = {'id': 'actual-job', 'version': 'new', 'confirmation': {'preview': preview}}
            retry.write_json(path, raw)
            body = 'Update cagents old -> new\n2 windows close and reopen.\n  Keep running (1)\n  Working now (1)\n    Chat\n[rehearsal prompt='
            self.assertEqual(retry.reviewed_inventory(body, folder, baseline, 'old', 'new'), 'actual-job')
            self.assertEqual(retry.reviewed_inventory(body.replace('\n', '\r\n'), folder, baseline, 'old', 'new'), 'actual-job')
            for bad in (body.replace('new', 'other'), body.replace('  Working now (1)\n    Chat\n', ''),
                        body.replace('-> new', '-> new-wrong'), body.replace('cagents old', 'cagents old-wrong'),
                        body.replace('    Chat\n', ''), body.replace('2 windows close and reopen.\n', '')):
                with self.subTest(body=bad), self.assertRaises(RuntimeError):
                    retry.reviewed_inventory(bad, folder, baseline, 'old', 'new')
            baseline['terminals']['missing'] = {}
            with self.assertRaisesRegex(RuntimeError, 'omitted original'):
                retry.reviewed_inventory(body, folder, baseline, 'old', 'new')

    def test_capture_failure_terminates_the_prompt_child_instead_of_waiting_for_answer(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.rehearse-update').write_text(str(root))
            (root / 'retry/first').mkdir(parents=True)
            child = 'import time;print("Proceed? [Y/n] ",flush=True);time.sleep(60)'
            result = subprocess.run([sys.executable, '-B', retry.pty.__file__, '--review-root', root,
                                     '--review-output', root / 'retry/first/previews', '--', sys.executable, '-c', child],
                                    capture_output=True, text=True, timeout=8)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('FileNotFoundError', result.stderr)


class FaultRelease(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.candidate = self.root / 'home/.local/share/cagents-install/versions/new'
        (self.root / '.fixture').mkdir()
        (self.root / 'retry/first').mkdir(parents=True)
        (self.root / 'logs').mkdir()
        self.sentinel = self.root / '.fixture/fail-new-daemon.armed'
        self.sentinel.write_text(str(self.candidate / 'bin/cagentsd') + '\n')
        library = self.root / '.fixture/startup-bind.so'
        library.write_bytes(b'private immutable injector')
        retry.write_json(self.root / '.fixture/manifest.json', {'library_sha256': retry.sha256(library)})
        self.log = self.root / 'logs/startup-bind.tsv'
        address = self.root / 'rt/cagents/cagentsd.sock'
        self.log.write_text(f'81\t{self.candidate}/bin/cagentsd\t{address}\tfail-armed\n'
                            f'82\t{self.root}/home/.local/share/cagents-install/versions/old/bin/cagentsd\t{address}\tpass-source\n')

    def release(self):
        retry.disarm_after_restore(self.root, {}, {'serving': {'proof': 'real'}}, 'old', {'launches': 4},
                                   self.candidate, 'cagents daemon exited before becoming ready: exit status: 1', self.diagnostics())

    def diagnostics(self):
        return {'ok': True, 'data': {'products': [{'product': 'daemon', 'readable': True, 'records': [
            {'pid': 81, 'version': 'new', 'run': 'failed-run', 'event': {'kind': 'startup_failed'}},
            {'pid': 81, 'version': 'new', 'run': 'failed-run', 'event': {'kind': 'exit', 'success': False}}]}]}}

    def test_qualified_release_records_explicit_source_bound_disarm(self):
        with patch.object(retry, 'require_restored', return_value=82), patch.object(retry, 'activity', return_value={'launches': 4}):
            self.release()
        self.assertFalse(self.sentinel.exists())
        self.assertTrue(self.sentinel.with_suffix('.disarmed').is_file())
        record = json.loads((self.root / 'retry/first/disarm.json').read_text())
        self.assertEqual(record['source_pid'], 82)
        self.assertEqual(record['event'], 'explicit_fixture_disarm')

    def test_failed_recovery_or_provider_replay_cannot_disarm(self):
        for recovery_error, count in ((True, 4), (False, 5)):
            with self.subTest(recovery_error=recovery_error), patch.object(retry, 'require_restored',
                    side_effect=RuntimeError('recovery not proved') if recovery_error else None, return_value=82), \
                    patch.object(retry, 'activity', return_value={'launches': count}), self.assertRaises(RuntimeError):
                self.release()
            self.assertTrue(self.sentinel.exists())
            self.assertFalse((self.root / 'retry/first/disarm.json').exists())

    def test_release_requires_actual_candidate_error_and_restored_source_bind(self):
        original = self.log.read_text()
        for bad in ('', original.replace('pass-source', 'pass-other'), original.replace('fail-armed', 'pass-disarmed')):
            self.log.write_text(bad)
            with self.subTest(log=bad), patch.object(retry, 'require_restored', return_value=82), \
                    patch.object(retry, 'activity', return_value={'launches': 4}), self.assertRaises(RuntimeError):
                self.release()
            self.assertTrue(self.sentinel.exists())
        self.log.write_text(original)
        with patch.object(retry, 'require_restored', return_value=82), \
                patch.object(retry, 'activity', return_value={'launches': 4}), self.assertRaises(RuntimeError):
            retry.disarm_after_restore(self.root, {}, {'serving': {}}, 'old', {'launches': 4}, self.candidate, 'unrelated failure', self.diagnostics())
        self.assertTrue(self.sentinel.exists())

    def test_changed_sentinel_and_duplicate_release_fail_closed(self):
        self.sentinel.write_text('another candidate\n')
        with patch.object(retry, 'require_restored', return_value=82), \
                patch.object(retry, 'activity', return_value={'launches': 4}), self.assertRaises(RuntimeError):
            self.release()
        self.assertTrue(self.sentinel.exists())

    def test_changed_injector_cannot_disarm(self):
        (self.root / '.fixture/startup-bind.so').write_bytes(b'another library')
        with patch.object(retry, 'require_restored', return_value=82), \
                patch.object(retry, 'activity', return_value={'launches': 4}), self.assertRaisesRegex(RuntimeError, 'injector bytes changed'):
            self.release()
        self.assertTrue(self.sentinel.exists())

    def test_crash_or_wrong_process_diagnostics_cannot_disarm(self):
        for change in ('pid', 'panic', 'exit'):
            diagnostic = self.diagnostics()
            events = diagnostic['data']['products'][0]['records']
            if change == 'pid':
                events[0]['pid'] = 999
            elif change == 'panic':
                events.append({'pid': 81, 'version': 'new', 'run': 'failed-run', 'event': {'kind': 'panic'}})
            else:
                events[1]['event']['success'] = True
            with self.subTest(change=change), patch.object(retry, 'require_restored', return_value=82), \
                    patch.object(retry, 'activity', return_value={'launches': 4}), self.assertRaisesRegex(RuntimeError, 'normal startup-error exit'):
                retry.disarm_after_restore(self.root, {}, {'serving': {}}, 'old', {'launches': 4}, self.candidate,
                                           'cagents daemon exited before becoming ready: exit status: 1', diagnostic)
            self.assertTrue(self.sentinel.exists())


if __name__ == '__main__':
    unittest.main()
