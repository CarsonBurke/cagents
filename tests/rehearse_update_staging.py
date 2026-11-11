#!/usr/bin/env python3
"""Pure staging/working-intent contracts; no native app or provider is started."""
import copy
from contextlib import redirect_stderr
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

SCRIPTS=Path(__file__).resolve().parents[1]/'scripts'
sys.path.insert(0,str(SCRIPTS))
spec=importlib.util.spec_from_file_location('staging_scenario',SCRIPTS/'rehearse_update_run.py')
scenario=importlib.util.module_from_spec(spec)
spec.loader.exec_module(scenario)


class CandidateStaging(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory(prefix='ru-stage-',dir='/var/tmp')
        self.addCleanup(self.temporary.cleanup)
        self.root=Path(self.temporary.name)
        (self.root/'.rehearse-update').write_text(str(self.root)+'\n')
        self.prefix=self.root/'home/.local/share/cagents-install'
        self.candidate=self.prefix/'versions/new'
        (self.candidate/'bin').mkdir(parents=True)
        (self.prefix/'.cagents-install-root').write_text('cagents install root\n')
        (self.prefix/'versions/old').mkdir()
        (self.prefix/'current').symlink_to('versions/old')
        self.installer=self.root/'install.sh'
        self.installer.write_text('# installer source\n')
        self.archive=self.root/'cagents-new-x86_64-unknown-linux-gnu.tar.gz'
        with tarfile.open(self.archive,'w:gz') as archive:
            for name in ('cagents','cagents-ui','cagentsd'):
                payload=f'#!/bin/sh\nprintf "{name} new\\n"\n'.encode()
                path=self.candidate/'bin'/name
                path.write_bytes(payload);path.chmod(0o755)
                info=tarfile.TarInfo(f'cagents-new/bin/{name}');info.size=len(payload)
                archive.addfile(info,io.BytesIO(payload))
        self.checksum()
        self.log=self.root/'stage.log'
        self.log.write_text(f'Staged cagents new; current, running windows and daemon were preserved. Run "{self.candidate}/bin/cagents" daemon upgrade to apply it when ready.\n')

    def checksum(self):
        (self.root/'SHA256SUMS').write_text(f'{hashlib.sha256(self.archive.read_bytes()).hexdigest()}  {self.archive.name}\n')

    def stage(self,record=False):
        # Version subprocesses are mocked; tests never launch installed binaries.
        def version(argv,**kwargs):
            return subprocess.CompletedProcess(argv,0,f'{Path(argv[0]).name} new\n','')
        with patch.object(scenario.subprocess,'run',side_effect=version):
            return scenario.staged_candidate(self.root,self.root,'new',self.installer,
                                              record=record,transcript=self.log)

    def test_exact_stage_reuses_printed_command_without_installer_or_tar_reextraction(self):
        self.assertEqual(self.stage(True),self.candidate/'bin/cagents')
        receipt=json.loads((self.root/'staged-candidate.json').read_text())
        self.assertEqual(receipt['command'],[str(self.candidate/'bin/cagents'),'daemon','upgrade'])
        self.assertEqual((self.root/'staged-candidate.json').stat().st_mode&0o777,0o600)
        with patch.object(scenario.tarfile,'open',side_effect=AssertionError('late archive extraction')):
            self.assertEqual(self.stage(),self.candidate/'bin/cagents')
        with self.assertRaises(FileExistsError):
            self.stage(True)

    def test_any_changed_candidate_or_source_refuses_reuse(self):
        mutations={
            'binary':lambda f:(f.candidate/'bin/cagents').write_text('changed'),
            'extra':lambda f:(f.candidate/'unexpected').write_text('extra'),
            'installer':lambda f:f.installer.write_text('changed'),
            'archive':lambda f:f.archive.write_bytes(f.archive.read_bytes()+b'changed'),
            'checksum':lambda f:(f.root/'SHA256SUMS').write_text('0'*64+'  '+f.archive.name+'\n'),
            'current':lambda f:((f.prefix/'current').unlink(),(f.prefix/'current').symlink_to('versions/new')),
            'marker':lambda f:(f.prefix/'.cagents-install-root').write_text('wrong\n'),
            'symlink':lambda f:((f.candidate/'bin/cagents').unlink(),(f.candidate/'bin/cagents').symlink_to('/usr/bin/false')),
            'mode':lambda f:(f.candidate/'bin/cagentsd').chmod(0o777),
        }
        for name,mutate in mutations.items():
            with self.subTest(name=name):
                # Independent roots: no mutation can make the next case pass accidentally.
                fixture=CandidateStaging()
                fixture.setUp()
                try:
                    fixture.stage(True)
                    mutate(fixture)
                    with self.assertRaises((RuntimeError,FileNotFoundError)):
                        fixture.stage()
                finally:
                    fixture.doCleanups()

    def test_wrong_printed_command_or_binary_archive_binding_is_rejected(self):
        original=self.log.read_text()
        self.log.write_text(original.replace('daemon upgrade','daemon stop'))
        with self.assertRaisesRegex(RuntimeError,'exact installed upgrade command'):
            self.stage(True)
        self.log.write_text(original)
        (self.candidate/'bin/cagentsd').write_text('unmatched daemon')
        with self.assertRaisesRegex(RuntimeError,'differs from the checksum-verified archive'):
            self.stage(True)
        self.assertFalse((self.root/'staged-candidate.json').exists())

    def test_all_three_versions_must_match(self):
        with patch.object(scenario.subprocess,'run',return_value=subprocess.CompletedProcess([],0,'cagents wrong\n','')):
            with self.assertRaisesRegex(RuntimeError,'exact matched version'):
                scenario.staged_candidate(self.root,self.root,'new',self.installer,record=True,transcript=self.log)


class WorkingIntent(unittest.TestCase):
    def fixture(self):
        proof={'host_session_id':'source','boot_id':'boot','host_incarnation':'incarnation'}
        chat={'id':'chat','proof':proof,'launch':{'provider':'Codex','session_id':'thread'}}
        agent={'agent':{'id':'agent','session':{'Chat':chat}},'state':'Running','run':{'id':'run'}}
        before={'agents':{'agent':agent},'serving':proof}
        event={'event':'notification','method':'turn/started','parent':True,'thread_id':'thread',
               'turn_id':'turn','socket':'private-socket','time':1}
        before['fixture_events']=[copy.deepcopy(event)]
        return before,[copy.deepcopy(agent)],proof,[event],{'pane':{'session_id':'chat','content':chat}}

    def check(self,before,agents,proof,events,panes):
        with patch.object(scenario.records,'panes',return_value=panes):
            return scenario.active_working_contract(before,agents,proof,events,'agent')

    def test_exact_active_parent_is_qualified(self):
        self.assertEqual(self.check(*self.fixture())['parent']['turn_id'],'turn')

    def test_completed_or_replaced_parent_is_case_invalid(self):
        for mutation in ('idle','run','proof','thread','completed','interrupt','no-start','new-turn'):
            with self.subTest(mutation=mutation):
                before,agents,proof,events,panes=self.fixture()
                if mutation=='idle': agents[0]['state']='Idle'
                if mutation=='run': agents[0]['run']={'id':'replacement'}
                if mutation=='proof': proof={'host_session_id':'replacement'}
                if mutation=='thread': panes['pane']['content']['launch']['session_id']='other'
                if mutation in ('completed','interrupt'):
                    events.append({**events[0],'method':'turn/'+mutation})
                if mutation=='no-start': events=[]
                if mutation=='new-turn': events.append({**events[0],'turn_id':'replacement-turn'})
                with self.assertRaisesRegex(RuntimeError,'case-invalid'):
                    self.check(before,agents,proof,events,panes)


class NaturalIdle(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory(prefix='ru-idle-',dir='/var/tmp')
        self.addCleanup(self.temporary.cleanup)
        self.root=Path(self.temporary.name)
        (self.root/'logs').mkdir()
        self.proof={'host_session_id':'source','boot_id':'boot','host_incarnation':'incarnation'}
        self.agent={'agent':{'id':'agent','session':{'Chat':{'id':'chat','proof':self.proof,
            'launch':{'provider':'Codex','session_id':None},'owned_codex_listener':{'socket':'private-socket'}}}},
            'run':'run','state':'Running'}
        (self.root/'seeds.json').write_text(json.dumps({'items':[{'state':'codex-chat',
            'response':{'data':{'agent':{'id':'agent'},'run':{'id':'run','agent':'agent'}}},
            'verified_agents':[self.agent]}]}))
        self.start={'event':'notification','method':'turn/started','parent':True,'socket':'private-socket',
                    'thread_id':'thread','turn_id':'turn','time':1}
        self.events=[self.start,{**self.start,'method':'turn/completed','status':'completed','time':2}]

    def wait(self,current=None,events=None,proof=None,initial=None):
        (self.root/'logs/codex-provider.jsonl').write_text(''.join(json.dumps(row)+'\n' for row in (events or self.events)))
        rehearsal=scenario.Rehearsal(self.root,'/unused-shell')
        current=current or {**self.agent,'state':'Succeeded'}
        with patch.object(rehearsal,'cli',side_effect=[(0,{'data':{'agents':[initial or self.agent]}}),
                (0,{'data':{'proof':proof or self.proof}}),(0,{'data':{'agents':[current]}})]), \
             patch.object(scenario.time,'sleep',side_effect=AssertionError('unexpected provider wait')):
            rehearsal.wait_natural_idle(90)
        return json.loads((self.root/'natural-idle.json').read_text())

    def test_exact_parent_normal_completion_preserves_original_agent_and_run(self):
        result=self.wait()
        self.assertTrue(result['valid'])
        self.assertEqual((result['agent_id'],result['run'],result['chat_id']),('agent','run','chat'))
        self.assertEqual(result['completed']['status'],'completed')

    def test_interrupted_replaced_or_wrong_source_cannot_qualify_as_natural_idle(self):
        variants={
            'interrupt':{'events':self.events+[{**self.start,'event':'request','method':'turn/interrupt'}]},
            'cancelled':{'events':[self.start,{**self.events[1],'status':'interrupted'}]},
            'replay':{'events':self.events+[{**self.start,'turn_id':'another-turn'}]},
            'run':{'current':{**self.agent,'run':'another-run','state':'Succeeded'}},
            'initial-run':{'initial':{**self.agent,'run':'replacement-before-first-poll'}},
            'proof':{'proof':{'host_session_id':'replacement'}},
        }
        for name,options in variants.items():
            with self.subTest(name=name):
                with self.assertRaisesRegex(RuntimeError,'natural-idle preflight failed'):
                    self.wait(**options)
                self.assertFalse(json.loads((self.root/'natural-idle.json').read_text())['valid'])

    def test_natural_idle_rejects_incompatible_modes_before_running(self):
        command=['fixture',str(self.root),'/unused-shell','run','--old-dir','/old','--old-version','old',
            '--old-installer','/installer','--new-dir','/new','--new-version','new','--new-installer','/installer',
            '--natural-idle','--codex-pause','90']
        incompatible=(['--require-working'],['--codex-never-confirm-interrupt'],['--codex-never-confirm-close'],
                      ['--require-prepared'],['--yes'],['--upgrade-arg=--yes'],['--expect-refusal','cancel'],
                      ['--no-init'],['--exit-mid-turn'],['--ignore-term'],['--uncarryable'],['--unreachable'])
        for flags in incompatible:
            with self.subTest(flags=flags),patch.object(sys,'argv',command+flags), \
                 patch.object(scenario.Rehearsal,'run',side_effect=AssertionError('must not start')), \
                 redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    scenario.main()
                self.assertEqual(error.exception.code,2)
        with patch.object(sys,'argv',command),patch.object(scenario.Rehearsal,'run',return_value=0) as run:
            self.assertEqual(scenario.main(),0)
        self.assertEqual(run.call_args.args[0].codex_pause,90)
        self.assertTrue(run.call_args.args[0].natural_idle)

    def test_failure_console_snapshot_uses_original_terminal_even_when_cli_fails(self):
        (self.root/'ui-console-before.json').write_text(json.dumps({'terminal':'original-console'}))
        rehearsal=scenario.Rehearsal(self.root,'/unused-shell')
        with patch.object(rehearsal,'call',return_value=(1,'public original output')) as call:
            rehearsal.failure_console_snapshot()
        self.assertEqual(call.call_args.args[-1],'original-console')
        self.assertIn('--no-start',call.call_args.args)
        result=json.loads((self.root/'logs/failure-console-snapshot.json').read_text())
        self.assertEqual(result,{'terminal':'original-console','exit':1,'output':'public original output'})


class IdleReview(unittest.TestCase):
    def fixture(self):
        proof={'host_session_id':'source','boot_id':'boot','host_incarnation':'incarnation'}
        expected={'valid':True,'chat_id':'chat','serving':proof}
        job={'phase':'AwaitingConfirmation','confirmation':{'stop':None,'preview':{'daemon':{
            'policy':'when_idle','capability':{'proof':proof},'sessions':[
                {'id':'chat','kind':'native_chat','disposition':'resumes'},
                {'id':'console','kind':'terminal','disposition':'continues'}]}}}}
        return job,expected

    def test_only_the_exact_idle_resumes_one_preview_is_approved(self):
        spec=importlib.util.spec_from_file_location('idle_pty',SCRIPTS/'rehearse_update_pty.py')
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        job,expected=self.fixture()
        self.assertTrue(module.idle_review_matches(job,expected))
        for change in ('working','wrong-chat','wrong-proof','extra-resume','stop','unproven','stopped-chat','keeps-running'):
            with self.subTest(change=change):
                job,expected=self.fixture()
                daemon=job['confirmation']['preview']['daemon']
                if change=='working': daemon['sessions'][0]['disposition']='waits_for_idle'
                if change=='wrong-chat': daemon['sessions'][0]['id']='another-chat'
                if change=='wrong-proof': expected['serving']={'host_session_id':'another-source'}
                if change=='extra-resume': daemon['sessions'][1]['disposition']='resumes'
                if change=='stop': job['confirmation']['stop']={'items':['console']}
                if change=='unproven': expected['valid']=False
                if change=='stopped-chat': job['confirmation']['preview']['stopped']=[{'pane':'chat'}]
                if change=='keeps-running': job['confirmation']['preview']['review_details']={'panes':{'chat':{'outcome':'keeps_running','detail':'console keeps running'}}}
                self.assertFalse(module.idle_review_matches(job,expected))


class IdlePromptGuard(unittest.TestCase):
    def exercise(self,*,working=False,wrong_chat=False):
        with tempfile.TemporaryDirectory(prefix='ru-idle-prompt-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'.rehearse-update').write_text(str(root)+'\n')
            status=root/'home/.local/share/cagents-install/.update/status.json'
            status.parent.mkdir(parents=True)
            job,expected=IdleReview().fixture()
            if wrong_chat:
                job['confirmation']['preview']['daemon']['sessions'][0]['id']='another-chat'
            job['confirmation']['nonce']='actual-private-review'
            status.write_text(json.dumps(job));status.chmod(0o600)
            (root/'natural-idle.json').write_text(json.dumps(expected))
            prompt=('Enter: update when they finish   i: interrupt them now   n: cancel'
                    if working else 'Update now? [Y/n] ')
            answers=root/'answer'
            program='import pathlib,sys;print(sys.argv[1],flush=True);pathlib.Path(sys.argv[2]).write_text(sys.stdin.readline().strip())'
            result=subprocess.run([sys.executable,str(SCRIPTS/'rehearse_update_pty.py'),
                '--require-idle','--answer','yes','--review-root',str(root),
                '--review-output',str(root/'reviews/natural-idle'),'--timeout','5','--',
                sys.executable,'-c',program,prompt,str(answers)],capture_output=True,text=True,timeout=10)
            captured=json.loads((root/'reviews/natural-idle/prompt-1.status.json').read_text())
            return result,answers.read_text(),captured

    def test_real_first_review_is_captured_before_ordinary_consent(self):
        result,answer,captured=self.exercise()
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        self.assertEqual(answer,'yes')
        self.assertEqual(captured['confirmation']['nonce'],'actual-private-review')

    def test_working_or_other_chat_is_declined_before_consent(self):
        for option in ({'working':True},{'wrong_chat':True}):
            with self.subTest(option=option):
                result,answer,_=self.exercise(**option)
                self.assertEqual(result.returncode,2,result.stdout+result.stderr)
                self.assertEqual(answer,'n')
                self.assertIn('case-invalid',result.stdout)


class UpdateIntentClassification(unittest.TestCase):
    def test_invalid_prompt_cannot_be_classified_as_an_expected_cancel(self):
        rehearsal=scenario.Rehearsal('/var/tmp/unused-rehearsal','/unused-shell')
        monitor=Mock()
        monitor.__enter__=Mock(return_value=monitor)
        monitor.__exit__=Mock(return_value=False)
        module=Mock()
        module.WindowCloseMonitor.return_value=monitor
        spec=Mock()
        with patch.object(scenario.importlib.util,'spec_from_file_location',return_value=spec), \
             patch.object(scenario.importlib.util,'module_from_spec',return_value=module), \
             patch.object(rehearsal,'call',return_value=(2,'rehearse-update: case-invalid: declining')):
            with self.assertRaisesRegex(RuntimeError,'not an expected refusal'):
                rehearsal.observed_update('/archive','new','/installer','--require-working')
        self.assertEqual(monitor.command_exit,2)
        monitor.__exit__.assert_called_once()


class StagedShellOrchestration(unittest.TestCase):
    def test_preexisting_candidate_is_rejected_before_installer(self):
        source=(SCRIPTS/'rehearse-update').read_text()
        function=source[source.index('cmd_stage() {'):source.index('# What scripts/install-local')]
        with tempfile.TemporaryDirectory(prefix='ru-stage-existing-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'home/.local/share/cagents-install/versions/new').mkdir(parents=True)
            program='set -euo pipefail\n'+function+'''
root=$1
user_home=$root/home
logs=$root/logs
require_init() { :; }
die() { printf '%s\\n' "$1" >&2; exit "${2:-1}"; }
cmd_install() { touch "$root/unexpected-installer"; }
cmd_stage "$root" new "$root/installer"
'''
            result=subprocess.run(['bash','-c',program,'fixture',str(root)],capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,2,result.stdout+result.stderr)
            self.assertIn('requires an absent candidate',result.stderr)
            self.assertFalse((root/'unexpected-installer').exists())

    def test_use_staged_runs_exact_verified_cli_without_restaging_or_forwarding_fixture_flags(self):
        source=(SCRIPTS/'rehearse-update').read_text()
        function=source[source.index('cmd_update() {'):source.index('# Inside the desktop when one runs')]
        with tempfile.TemporaryDirectory(prefix='ru-stage-shell-',dir='/var/tmp') as temporary:
            root=Path(temporary)
            (root/'logs').mkdir()
            cli=str(root/'home/.local/share/cagents-install/versions/new/bin/cagents')
            program='set -euo pipefail\n'+function+'''
root=$1
user_home=$root/home
logs=$root/logs
self=/fixture/rehearse-update
tool_root=/fixture
require_init() { :; }
die() { printf '%s\\n' "$1" >&2; exit "${2:-1}"; }
cmd_install() { printf 'unexpected installer\\n' >&2; exit 99; }
user_env() { printf '%s\\n' "$user_home/.local/share/cagents-install/versions/new/bin/cagents"; }
cmd_app_or_env() { printf '%s\\n' "$@" > "$root/invocation"; }
cmd_update /archive new /installer --use-staged --require-working --working i --answer yes --interrupt
'''
            result=subprocess.run(['bash','-c',program,'fixture',str(root)],capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,0,result.stdout+result.stderr)
            invocation=(root/'invocation').read_text().splitlines()
            self.assertEqual(invocation[-4:],[cli,'daemon','upgrade','--interrupt'])
            self.assertIn('--require-working',invocation)
            self.assertNotIn('--use-staged',invocation)
            self.assertIn('verified against the early stage receipt',result.stdout)
            for override in ('--yes','--version other','--version=other'):
                (root/'invocation').unlink(missing_ok=True)
                declined=subprocess.run(['bash','-c',program.replace('--interrupt\n','--interrupt '+override+'\n'),
                    'fixture',str(root)],capture_output=True,text=True,timeout=10)
                self.assertEqual(declined.returncode,2,declined.stdout+declined.stderr)
                self.assertFalse((root/'invocation').exists())



class WorkingPromptGuard(unittest.TestCase):
    def exercise(self,prompts):
        with tempfile.TemporaryDirectory(prefix='ru-prompt-',dir='/var/tmp') as temporary:
            receipt=Path(temporary)/'answers.json'
            program=('import json,sys\nanswers=[]\n'
                     'for prompt in json.loads(sys.argv[1]):\n'
                     ' print(prompt,flush=True)\n answers.append(sys.stdin.readline().strip())\n'
                     'open(sys.argv[2],"w").write(json.dumps(answers))\n')
            result=subprocess.run([sys.executable,str(SCRIPTS/'rehearse_update_pty.py'),
                '--require-working','--answer','yes','--working','i','--timeout','5','--',
                sys.executable,'-c',program,json.dumps(prompts),str(receipt)],
                capture_output=True,text=True,timeout=10)
            return result,json.loads(receipt.read_text())

    def test_unexpected_ordinary_or_stop_prompt_is_declined_before_consent(self):
        for prompt in ('Update now? [Y/n] ', 'Enter: review what stops   n: cancel',
                       'Type STOP. Anything else cancels.'):
            with self.subTest(prompt=prompt):
                result,answers=self.exercise([prompt,'Update now? [Y/n] '])
                self.assertEqual(result.returncode,2,result.stdout+result.stderr)
                self.assertIn('case-invalid',result.stdout)
                self.assertEqual(answers,['' if 'Anything else' in prompt else 'n','n'])

    def test_observed_working_choice_preserves_requested_answers(self):
        result,answers=self.exercise(['Enter: update when they finish   i: interrupt them now   n: cancel',
                                      'Update now? [Y/n] '])
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        self.assertEqual(answers,['i','yes'])

    def test_yes_override_is_rejected_before_child_starts(self):
        with tempfile.TemporaryDirectory(prefix='ru-prompt-yes-',dir='/var/tmp') as temporary:
            marker=Path(temporary)/'unexpected-child'
            result=subprocess.run([sys.executable,str(SCRIPTS/'rehearse_update_pty.py'),
                '--require-working','--',sys.executable,'-c',
                'import pathlib,sys;pathlib.Path(sys.argv[1]).touch()',str(marker),'--yes'],
                capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,2,result.stdout+result.stderr)
            self.assertFalse(marker.exists())
            self.assertIn('forbids --yes',result.stderr)

    def test_success_without_review_is_invalid(self):
        result,answers=self.exercise([])
        self.assertEqual(result.returncode,2,result.stdout+result.stderr)
        self.assertEqual(answers,[])


if __name__=='__main__':
    unittest.main()
