"""Algorithm-only fixtures for the bounded checker, never native/run evidence.

Run directly only after independent source review and explicit pure-data grant.
No fake complete native packet, official artifact or qualification is produced.
"""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

path = Path(__file__).with_name('final-main-check.py')
spec = importlib.util.spec_from_file_location('final_main_check', path)
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def algorithm_fixture():
    root = '/tmp/algorithm-only-2018'
    initial = dict(id='00000000-0000-4000-8000-000000002022', proof=None, terminal=None,
        owned_codex_listener=None, native_continuity='ReconnectOnly',
        launch=dict(provider='Claude', session_id=None, recipe=dict(program='synthetic-offline-claude-never-execute',
            arguments=['--permission-mode','default'], working_directory=root + '/project', environment={}, unset_environment=[])))
    chosen = copy.deepcopy(initial)
    chosen['launch']['recipe'].update(working_directory=root + '/project/peer-directory',
        environment={'CLAUDE_CONFIG_DIR':root + '/peer-home','SYNTHETIC_RECIPE_DIMENSION':'peer-B'}, unset_environment=['SYNTHETIC_UNUSED_VALUE'])
    rejected = copy.deepcopy(initial)
    rejected.update(id='00000000-0000-4000-8000-000000002023', proof=dict(host_session_id='synthetic-host',boot_id='boot',host_incarnation='issued'))
    rejected['launch']['session_id'] = rejected['id']
    issued = copy.deepcopy(rejected)
    issued['proof']['host_incarnation'] = 'synthetic-wrong-issuer'
    scope = {'Channel':'00000000-0000-4000-8000-000000002020'}
    panes = [dict(id=card, kind=dict(kind='chat',**session), title=title, user_title=None,
                  pending_terminal_start=False,width_fraction=.5,scope=None)
             for card,session,title in [(check.A,initial,'Idle image draft'),(check.B,rejected,'Unavailable saved sibling')]]
    model = dict(identity='00000000-0000-4000-8000-000000002027',sequence=10,
                 agents=[dict(id=check.B,closed=False,placement=scope,session={'Chat':issued})],
                 views=[dict(scope=scope,revision=7,panes=panes,retired=[])])
    peer = copy.deepcopy(model)
    peer['views'][0]['panes'][0]['kind'] = dict(kind='chat',**chosen)
    peer['views'][0]['revision'] += 1
    peer['sequence'] += 1
    source = dict(event='source',initial=initial,chosen=chosen,rejected=rejected,connection_proof=copy.deepcopy(rejected['proof']),full_model=model,unix_ns=1)
    fixtures = [source,dict(event='seed-request',scope=scope,request_id=11,panes=[panes[0]],closed=[],unix_ns=2),
        dict(event='seed-ack',scope=scope,request_id=11,full_model=model,unix_ns=3),
        dict(event='peer-source-witness',full_model=peer,unix_ns=7),dict(event='peer-delivered',unix_ns=8)]
    # Thin byte fixture: not offered to a desktop or claimed as a real PNG.
    png = b'\x89PNG\r\n\x1a\nalgorithm-only-byte-fixture'
    sha = hashlib.sha256(png).hexdigest()
    image = dict(path=root + '/data/managed.png',reference=dict(id='algorithm-image',number=1,device='algorithm-device',
        name='Pasted image',sha256=sha,extension='png'))
    def storage(session, text):
        return dict(cards={check.A:dict(source=copy.deepcopy(session),existence='open'),check.B:dict(source=copy.deepcopy(rejected),existence='open')},
            metadata=dict(quarantined_panes={check.B:dict(kind='chat',**rejected)},local_only_cards=[],
                chat_drafts={check.A:text,check.B:'Rejected sibling keeps its saved café draft.\nNever submit or start it.'},
                chat_draft_attachments={check.A:[image],check.B:[dict(path=root + '/rejected-saved.png',reference={'id':'unavailable'})]},
                threads=[dict(id=scope['Channel'],cards=[check.A,check.B],focused=check.A)],active_thread=scope['Channel']))
    first = dict(pid=10,window=100,start_stamp='42',editor_bounds=[20,40,400,60],text='unsent image',caret=12,selections=0)
    # Unicode character count is used by the public AT-SPI Text interface.
    first['caret'] = len(first['text'])
    continued = {**first,'text':first['text'] + ' Keyboard focus stayed here.'}
    continued['caret'] = len(continued['text'])
    request = '/org/freedesktop/portal/desktop/request/1_5/fixture_token'
    call = dict(sender=':1.5',serial=14,body=['','Attach files to chat',dict(modal=True,multiple=True,handle_token='fixture_token')],unix_ns=4)
    reply = dict(sender=':1.6',destination=':1.5',reply_serial=14,body=[request],unix_ns=5)
    opened = dict(call=call,returned=reply,frontend_owner=':1.6',request_path=request,ui_pid=10,frontend_pid=20)
    cancelled = dict(path=request,sender=':1.6',body=[1,{}],unix_ns=12)
    receipt = dict(revision=check.PRODUCT,ui_pid=10,ui_start_stamp='42',window=100,ui_sha256='a'*64,
        matched_triplet={name:dict(path='/target/dev-runtime/' + name,sha256='a'*64) for name in ('cagents','cagentsd','cagents-ui')},
        portal_open=opened,portal_cancel=cancelled,portal_frontend=dict(pid=20,owner=':1.6'),portal_backend=dict(pid=21),
        descriptor=dict(original='/usr/share/xdg-desktop-portal/portals/gtk.portal',sha256='b'*64,original_sha256='b'*64),
        restored_focus=first,continued_input=continued,final_text=continued['text'],actual_image=image,
        asset=dict(path=image['path'],sha256=sha,bytes=len(png),links=1),
        catalog_observed=[dict(name='Fixture model A'),dict(name='Fixture model B')],
        postcancel_footer=['Synthetic peer account','Claude · Default model','Ask before edits'],
        postcancel_headers=['Chat pane Draft peer fixture, ' + root + '/project/peer-directory, focused'],
        inspector_before=dict(present=True),inspector_after=dict(present=False))
    native = [dict(event='chooser-pending',portal=opened,gtk_pid=21,window=101,chooser_directory=root + '/chooser-data',unix_ns=6),
        dict(event='native-click',target='Cancel',pid=21,bounds=[0,0,4,6],unix_ns=9),
        dict(event='command',argv=['xdotool','mousemove','--sync','2','3','click','1'],started_unix_ns=10,unix_ns=13,exit_code=0),
        dict(event='command',argv=['xdotool','type','--clearmodifiers','--delay','1',' Keyboard focus stayed here.'],started_unix_ns=14,unix_ns=15,exit_code=0),
        dict(event='postcancel-continuation',before=first,after=continued,unix_ns=16)]
    return dict(receipt=receipt,source=source,fixtures=fixtures,native=native,payload=storage(initial,first['text']),
                final=storage(chosen,continued['text']),png=png)


def validate(value, label='after'):
    return check.check_flow(label,value['receipt'],value['source'],value['fixtures'],value['native'],value['payload'],value['final'],value['png'])


class BoundedCheckerTests(unittest.TestCase):
    def baseline(self):
        value = algorithm_fixture()
        validate(value)
        return copy.deepcopy(value)

    def rejects(self, reason, mutation):
        value = self.baseline()
        mutation(value)
        with self.assertRaisesRegex(ValueError, '^' + reason + '$'):
            validate(value)

    def test_N01_exact_variant_source_and_same_executable_paths(self):
        self.rejects('arm-source',lambda v:v['receipt'].update(revision=check.BEFORE))
        # Same Cargo filenames are a positive, not an old-vs-new origin oracle.
        before = self.baseline()
        before['receipt']['revision'] = check.BEFORE
        before['receipt']['postcancel_footer'][0] = 'Synthetic initial account'
        validate(before,'before')

    def test_N02_ten_ui_records_and_exact_parent_ownership(self):
        plan = json.loads(Path(__file__).with_name('final-main-plan.json').read_text())
        lane = ['--locked','--no-default-features','--features','ui']
        product, target = '/work/product', '/target'
        library_binary, repository_binary = target + '/debug/deps/cagents-same-name', target + '/debug/deps/repository_git_contract-same-name'
        package = 'path+file:///work/product#cagents@0.1.0'
        def artifact(name, kind, source, executable, test=True):
            return dict(reason='compiler-artifact',package_id=package,manifest_path=product + '/Cargo.toml',
                target=dict(name=name,kind=[kind],crate_types=['lib' if kind == 'lib' else 'bin'],src_path=product + '/' + source),
                profile=dict(test=test),features=['ui'],executable=executable)
        library = artifact('cagents','lib','src/lib.rs',library_binary)
        repository = artifact('repository_git_contract','test','tests/repository_git_contract.rs',repository_binary)
        # Shuffled real-shaped targets, including bin/test/lib distractors.
        compiled = [artifact('other_contract','test','tests/other_contract.rs',target + '/debug/deps/other'),
                    repository,artifact('cagents','lib','src/lib.rs',None,False),library,
                    artifact('cagents-ui','bin','src/bin/cagents-ui.rs',target + '/debug/cagents-ui')]
        def receipt(value):
            return dict(artifact=copy.deepcopy(value),executable=value['executable'],observed_binary_sha256='a'*64,compile_stdout_sha256='b'*64)
        selected, repository_selected = receipt(library), receipt(repository)
        all_selectors = plan['selectors'] + plan['repository_selectors']
        argvs = [['cargo','check',*lane,'--bin','cagents-ui'],['cargo','test',*lane,'--all-targets','--no-run','--message-format=json']]
        argvs += [[library_binary,selector,'--exact','--test-threads=1','--nocapture','--color=never'] for selector in plan['selectors']]
        argvs += [[repository_binary,selector,'--exact','--test-threads=1','--nocapture','--color=never'] for selector in plan['repository_selectors']]
        argvs += [['cargo','test',*lane,'--all-targets','--','--test-threads=1','--color=never']]
        rows = [dict(name=name,argv=argv,exit_code=0,product=check.PRODUCT,stdout=dict(sha256='b'*64))
                for name,argv in zip(plan['fresh_rust_records'],argvs,strict=True)]
        logs = {name:'test ' + selector + ' ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 20 filtered out;\n'
                for name,selector in zip(plan['fresh_rust_records'][2:9],all_selectors,strict=True)}
        logs['ui-all-targets-suite'] = ''.join('test ' + selector + ' ... ok\n' for selector in all_selectors)
        logs['ui-all-targets-suite'] += 'test result: ok. 400 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n'
        logs['ui-all-targets-suite'] += 'test result: ok. 60 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n'
        data = dict(plan=plan,rows=rows,logs=logs,compiled=compiled,selected=selected,repository_selected=repository_selected)
        def rebind_compile(value):
            raw = ''.join(json.dumps(row) + '\n' for row in value['compiled'])
            value['logs']['ui-all-targets-compile'] = raw
            sha = hashlib.sha256(raw.encode()).hexdigest()
            value['rows'][1]['stdout']['sha256'] = sha
            for key in ('selected','repository_selected'): value[key]['compile_stdout_sha256'] = sha
        rebind_compile(data)
        def valid(value):
            check.check_rust(value['plan'],value['rows'],value['logs'],value['compiled'],value['selected'],value['repository_selected'],product,target)
        def reject(reason, mutate):
            value = copy.deepcopy(data); valid(value); mutate(value)
            with self.assertRaisesRegex(ValueError,'^' + reason + '$'): valid(value)
        def change_repository(value, mutate):
            mutate(value['compiled'][1])
            value['repository_selected']['artifact'] = copy.deepcopy(value['compiled'][1])
            rebind_compile(value)
        valid(copy.deepcopy(data))  # T1: correct routing, same-origin Cargo filenames are not a freshness oracle.
        # T2: every repository target predicate is independently necessary.
        for field, replacement in [('kind',['bin']),('name','cagents'),('src_path','/work/other/tests/repository_git_contract.rs')]:
            with self.subTest(group='T2',field=field):
                reject('selected-repository-artifact',lambda d,f=field,r=replacement:change_repository(d,lambda a:a['target'].__setitem__(f,r)))
        for field, mutate in [('profile',lambda a:a['profile'].update(test=False)),('executable',lambda a:a.pop('executable'))]:
            with self.subTest(group='T2',field=field):
                reject('selected-repository-artifact',lambda d,m=mutate:change_repository(d,m))
        def change_compilation(value, mutate):
            mutate(value['compiled'])
            rebind_compile(value)
        reject('selected-repository-artifact',lambda d:change_compilation(d,lambda rows:rows.pop(1)))
        reject('selected-repository-artifact',lambda d:change_compilation(d,lambda rows:rows.append(copy.deepcopy(rows[1]))))
        reject('selected-repository-package',lambda d:change_repository(d,lambda a:a.update(package_id='path+file:///other#cagents@0.1.0')))
        reject('selected-repository-package',lambda d:change_repository(d,lambda a:a.update(manifest_path='/other/Cargo.toml')))
        # T3: artifact/receipt agree during feature mutations, so feature guards own the refusal.
        for features in [['ui','host'],['ui','full'],['ui','integrated-browser'],[]]:
            with self.subTest(group='T3',features=features):
                reject('selected-repository-features',lambda d,f=features:change_repository(d,lambda a:a.update(features=f)))
        reject('selected-repository-compile-ref',lambda d:d['repository_selected'].update(compile_stdout_sha256='c'*64))
        reject('selected-repository-artifact',lambda d:d['repository_selected']['artifact']['target'].update(name='wrong'))
        reject('selected-repository-identity',lambda d:d['repository_selected'].update(executable=library_binary))
        reject('selected-repository-identity',lambda d:d['repository_selected'].update(observed_binary_sha256='not-a-sha256'))
        reject('selected-repository-identity',lambda d:change_repository(d,lambda a:a.update(executable='/outside/repository_git_contract')))
        def forbidden_library_feature(value):
            value['compiled'][3]['features'].append('host')
            value['selected']['artifact'] = copy.deepcopy(value['compiled'][3])
            rebind_compile(value)
        reject('selected-features',forbidden_library_feature)
        reject('selected-compile-ref',lambda d:d['selected'].update(compile_stdout_sha256='c'*64))
        # T4: exact four-plus-three partition, record order and binary routing.
        for mutation in [lambda d:d['plan']['repository_selectors'].pop(),
                         lambda d:d['plan']['repository_selectors'].append(d['plan']['repository_selectors'][0]),
                         lambda d:d['plan']['repository_selectors'].__setitem__(0,'undeclared'),
                         lambda d:d['plan']['selectors'].__setitem__(1,d['plan']['selectors'][0])]:
            reject('selector-partition',mutation)
        reject('rust-records',lambda d:d['rows'].pop(6))
        reject('rust-records',lambda d:d['rows'].append(copy.deepcopy(d['rows'][6])))
        reject('rust-records',lambda d:d['rows'].__setitem__(7,copy.deepcopy(d['rows'][6])))
        reject('focused-argv',lambda d:d['rows'][6]['argv'].__setitem__(0,library_binary))
        reject('ui-check-argv',lambda d:d['rows'][0]['argv'].insert(2,'--features=host'))
        # T5: exact owning execution, not filtered/ignored text or a wrong argument.
        repository_name = plan['fresh_rust_records'][6]
        reject('rust-exit-source',lambda d:d['rows'][6].update(exit_code=101))
        reject('focused-argv',lambda d:d['rows'][6]['argv'].__setitem__(1,'wrong-selector'))
        for summary in ['ok. 0 passed; 0 failed; 0 ignored; 0 measured; 60 filtered out;',
                        'ok. 0 passed; 0 failed; 1 ignored; 0 measured; 59 filtered out;']:
            reject('focused-summary',lambda d,s=summary:d['logs'].__setitem__(repository_name,
                'test ' + plan['repository_selectors'][0] + ' ... ok\ntest result: ' + s + '\n'))
        reject('focused-owning-line',lambda d:d['logs'].__setitem__(repository_name,d['logs'][repository_name].replace(' ... ok',' ... ignored')))
        reject('focused-owning-line',lambda d:d['logs'].__setitem__(repository_name,d['logs'][repository_name] + 'test ' + plan['repository_selectors'][0] + ' ... ok\n'))
        # T6: real caught inner panic output is authenticated as opaque stderr;
        # the two injections remain one successful Rust parent, never two rows.
        preserved = plan['fresh_rust_records'][8]
        caught = b"thread panicked: injected failure after actual hook handoff\nthread panicked: injected failure after original command timeout\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / 'caught.stderr.log').write_bytes(caught)
            ref = dict(path='caught.stderr.log',sha256=hashlib.sha256(caught).hexdigest(),bytes=len(caught))
            self.assertEqual(check.member(root,ref),caught)
            positive = copy.deepcopy(data); positive['rows'][8]['stderr'] = ref; valid(positive)
        reject('focused-summary',lambda d:d['logs'].__setitem__(preserved,d['logs'][preserved].replace('1 passed','2 passed')))
        reject('rust-records',lambda d:d['rows'].append(dict(name='AfterHandoff')))
        reject('rust-records',lambda d:d['rows'].append(dict(name='AfterTimeout')))
        reject('focused-owning-line',lambda d:d['logs'].__setitem__(preserved,d['logs'][preserved] + 'test ' + plan['repository_selectors'][2] + ' ... ok\n'))
        # T7: all seven broad owning lines, exact nine-record pre-native prefix,
        # and ten-record metadata. No native cleanup prerequisite is bypassed.
        for selector in plan['repository_selectors']:
            reject('broad-owning-line',lambda d,s=selector:d['logs'].__setitem__('ui-all-targets-suite',d['logs']['ui-all-targets-suite'].replace('test ' + s + ' ... ok\n','')))
            reject('broad-owning-line',lambda d,s=selector:d['logs'].__setitem__('ui-all-targets-suite',d['logs']['ui-all-targets-suite'] + 'test ' + s + ' ... ok\n'))
        reject('broad-summary',lambda d:d['logs'].__setitem__('ui-all-targets-suite',d['logs']['ui-all-targets-suite'].replace('0 filtered out','4 filtered out')))
        check.check_rust_records(plan,data['rows'][:9],focused=True)
        for prefix in (data['rows'][:8],data['rows']):
            with self.assertRaisesRegex(ValueError,'^rust-focused-prefix$'): check.check_rust_records(plan,prefix,focused=True)
        source = dict(product=check.PRODUCT,rust_records=10,plan_sha256='a'*64,workflow_sha256='b'*64)
        check.check_rust_source(source,'a'*64,'b'*64)
        source['rust_records'] = 7
        with self.assertRaisesRegex(ValueError,'^rust-source-binding$'): check.check_rust_source(source,'a'*64,'b'*64)

    def test_N03_ui_owner_and_generation(self):
        self.rejects('portal-caller',lambda v:v['receipt'].update(ui_pid=11))
        self.rejects('ui-build-identity',lambda v:v['receipt']['matched_triplet']['cagents-ui'].update(sha256='c'*64))
        self.rejects('ui-process-generation',lambda v:v['receipt'].update(ui_start_stamp='43'))

    def test_N04_portal_request_and_backend_binding(self):
        self.rejects('portal-request-binding',lambda v:v['receipt']['portal_open'].update(request_path='/wrong/request'))
        self.rejects('portal-request-binding',lambda v:v['receipt']['portal_open']['returned'].update(reply_serial=99))
        self.rejects('gtk-dialog-owner',lambda v:v['receipt']['portal_backend'].update(pid=22))

    def test_N05_native_cancel_and_exact_response_one(self):
        self.rejects('one-real-cancel',lambda v:v['native'].pop(1))
        self.rejects('actual-cancel-response',lambda v:v['receipt']['portal_cancel']['body'].__setitem__(0,2))
        self.rejects('one-real-cancel',lambda v:v['native'].append(copy.deepcopy(v['native'][1])))
        self.rejects('actual-cancel-gesture',lambda v:v['native'].pop(2))
        self.rejects('actual-cancel-gesture',lambda v:v['native'][2]['argv'].__setitem__(3,'200'))

    def test_N06_peer_update_must_be_inside_real_chooser_interval(self):
        self.rejects('callback-order',lambda v:v['fixtures'][-1].update(unix_ns=3))
        self.rejects('callback-order',lambda v:v['receipt']['portal_cancel'].update(unix_ns=6))
        self.rejects('continuation-after-response',lambda v:v['native'][3].update(started_unix_ns=11))
        self.rejects('native-command-exit',lambda v:v['native'][3].update(exit_code=1))

    def test_N07_raw_rejected_sibling_and_real_seed_ack(self):
        self.rejects('raw-full-scope',lambda v:v['source']['full_model']['views'][0]['panes'].pop())
        self.rejects('wrong-issuer-proof',lambda v:v['source']['full_model']['agents'][0]['session']['Chat']['proof'].update(host_incarnation='issued'))
        self.rejects('real-seed-ack',lambda v:v['fixtures'][2].update(request_id=12))
        self.rejects('stored-quarantine',lambda v:v['final']['metadata'].update(quarantined_panes={}))
        def wrong_later_read(value):
            full = copy.deepcopy(value['source']['full_model'])
            full['views'][0]['panes'][1]['kind']['proof']['host_incarnation'] = 'different'
            value['fixtures'].append(dict(event='full-model',model=full,unix_ns=20))
        self.rejects('full-B-conservation',wrong_later_read)
        self.rejects('full-scope-conservation',lambda v:v['fixtures'][3]['full_model']['views'][0].update(scope={'Channel':'wrong-scope'}))

    def test_N08_any_forbidden_or_unknown_actor_request(self):
        for request in ('ChatStart','ChatAttach','ChatPrepareResume','ChatClose','Spawn','Submit'):
            with self.subTest(request=request):
                self.rejects('actor-or-unknown-request',lambda v:v['fixtures'].append(dict(event='UNEXPECTED-request',request=request)))

    def test_N09_exact_text_source_cursor_and_scope_custody(self):
        self.rejects('continued-text',lambda v:v['receipt'].update(final_text='plausible wrong draft'))
        self.rejects('editor-cursor-custody',lambda v:v['receipt']['restored_focus'].update(caret=0))
        self.rejects('stored-B-draft',lambda v:v['final']['metadata']['chat_drafts'].__setitem__(check.B,'lost'))
        self.rejects('stored-scope-focus',lambda v:v['final']['metadata']['threads'][0].update(focused=check.B))

    def test_N10_exact_managed_image_association_and_bytes(self):
        self.rejects('image-reference',lambda v:v.update(png=v['png'] + b'changed'))
        self.rejects('asset-custody',lambda v:v['receipt']['asset'].update(links=2))
        self.rejects('asset-custody',lambda v:v['receipt']['actual_image'].update(path='/tmp/other-arm/data/managed.png'))
        self.rejects('stored-A-payload',lambda v:v['final']['metadata']['chat_draft_attachments'].__setitem__(check.A,[]))

    def test_N11_complete_recipe_constant_permissions_and_known_footer(self):
        self.rejects('constant-mode-model-effort',lambda v:v['source']['chosen']['launch']['recipe']['arguments'].append('--model=other'))
        self.rejects('chosen-full-recipe',lambda v:v['source']['chosen']['launch']['recipe']['environment'].update(UNAPPROVED='value'))
        self.rejects('stored-source',lambda v:v['final']['cards'][check.A].update(source=copy.deepcopy(v['source']['initial'])))
        self.rejects('after-footer',lambda v:v['receipt']['postcancel_footer'].__setitem__(0,'Synthetic initial account'))
        self.rejects('known-catalog',lambda v:v['receipt']['catalog_observed'].pop())

    def test_N12_no_postcancel_focus_repair(self):
        self.rejects('no-focus-repair',lambda v:v['native'].insert(3,dict(event='command',argv=['xdotool','windowactivate','100'],started_unix_ns=13,unix_ns=13,exit_code=0)))
        self.rejects('actual-cancel-gesture',lambda v:v['native'][2].update(started_unix_ns=13))
        self.rejects('no-focus-repair',lambda v:v['native'].insert(3,dict(event='command',argv=['arbitrary-observer'],started_unix_ns=13,unix_ns=13,exit_code=0)))
        self.rejects('same-focused-owner',lambda v:v['receipt']['restored_focus'].update(window=101))

    def test_N13_real_member_hash_path_and_file_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); path = root / 'read.json'; path.write_bytes(b'algorithm-only')
            ref = dict(path='read.json',sha256=hashlib.sha256(path.read_bytes()).hexdigest(),bytes=path.stat().st_size)
            self.assertEqual(check.member(root,ref),b'algorithm-only')
            for changed, reason in [({**ref,'sha256':'c'*64},'member-bytes'),({**ref,'path':'../read.json'},'member-path')]:
                with self.assertRaisesRegex(ValueError,'^' + reason + '$'): check.member(root,changed)
            linked = root / 'linked.json'; linked.symlink_to(path)
            with self.assertRaisesRegex(ValueError,'^member-alias$'): check.member(root,{**ref,'path':'linked.json'})
            hard = root / 'hard.json'; hard.hardlink_to(path)
            with self.assertRaisesRegex(ValueError,'^member-type$'): check.member(root,ref)

    def test_N14_retained_original_is_not_fresh_final_head(self):
        value = json.loads(Path(__file__).with_name('final-main-plan.json').read_text())['retained_native_scope']
        check.check_retained(copy.deepcopy(value))
        for key, changed, reason in [('original_product',check.PRODUCT['sha'],'retained-source'),('arms',24,'retained-counts'),('original_run',1,'retained-origin')]:
            mutated = copy.deepcopy(value); check.check_retained(mutated); mutated[key] = changed
            with self.assertRaisesRegex(ValueError,'^' + reason + '$'): check.check_retained(mutated)

    def test_N15_exact_capsule_and_applicability_references(self):
        value = json.loads(Path(__file__).with_name('final-main-plan.json').read_text())['retained_native_scope']
        for key, reason in [('applicability','applicability-ref'),('qualified_capsule','capsule-ref')]:
            mutated = copy.deepcopy(value); check.check_retained(mutated); mutated[key]['sha256'] = 'c'*64
            with self.assertRaisesRegex(ValueError,'^' + reason + '$'): check.check_retained(mutated)
        # Bytes of the actual sealed original receipts are independently read by
        # lead aggregation; these algorithm fixtures do not impersonate them.
        self.assertEqual(check.OUTSIDE[3],'original22/100 applicability and custody')


if __name__ == '__main__':
    unittest.main()
