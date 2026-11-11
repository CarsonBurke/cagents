#!/usr/bin/env python3
"""Pure staging/working-intent contracts; no native app or provider is started."""
import copy
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
from unittest.mock import patch

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
