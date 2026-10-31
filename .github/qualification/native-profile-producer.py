"""Contingent ORIGINAL compile-only producer. Current admission is NULL.

This source is not a grant, an old warm-cache discovery or native validation.
M is pre-save payload evidence; key/P/A are external to M. External publication
readback/entry-wait/whole-job acceptance cannot be certified by this process.
"""
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile
import time

PINNED_ADMISSION_INPUTS = None  # No actual policy change/grant/facts/contract supplied.
INPUT_NAMES = {'policy','grant','preflight','publication_contract'}
FEATURES = {'cli':['development'],'host':['development','host'],'ui':['development','ui']}
PHASE = 'producer-only'


def need(value, reason):
    if not value:
        raise ValueError(reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def reference(value):
    need(isinstance(value,dict) and set(value) == {'path','sha256','bytes'},'reference')
    need(PurePosixPath(value['path']).is_absolute() and '..' not in PurePosixPath(value['path']).parts,'reference-path')
    need(type(value['bytes']) is int and value['bytes'] > 0 and len(value['sha256']) == 64
         and set(value['sha256']) <= set('0123456789abcdef'),'reference-bytes')


def admit_references(plan, pinned=PINNED_ADMISSION_INPUTS):
    supplied = plan['admission_inputs']
    need(supplied is not None and pinned is not None,'SOURCE_ONLY: separate exact producer admission UNSUPPLIED')
    need(set(supplied) == set(pinned) == INPUT_NAMES,'admission-inputs')
    for name in sorted(INPUT_NAMES):
        reference(supplied[name]); reference(pinned[name])
        need(supplied[name] == pinned[name],'unpinned-admission')


def admitted_contract(plan, context, read_ref, pinned=PINNED_ADMISSION_INPUTS):
    admit_references(plan,pinned)
    documents = {name:read_ref(ref) for name,ref in plan['admission_inputs'].items()}
    validate_admission(plan,context,documents,plan['admission_inputs'])
    return documents


def validate_admission(plan, context, documents, refs):
    # These are PRE-admission contract/observation inputs, never future M/P/A.
    need(context['repository'] == plan['repository'] and context['event'] == plan['event']
         and context['branch'] == plan['branch'] and context['ref'] == plan['cache_ref']
         and context['pull_request'] == 1933 and context['attempt'] == 1,'original-producer-context')
    policy,grant,facts,publication = [documents[name] for name in ('policy','grant','preflight','publication_contract')]
    need(policy['kind'] == 'native-producer-policy-decision-v1'
         and policy['decision'] == 'allow-one-original-compile-only-producer'
         and policy['caps'] == plan['caps'],'producer-policy')
    need(grant['kind'] == 'native-producer-exact-grant-v1' and grant['context'] == context
         and grant['purpose'] == plan['purpose'] and grant['allocation'] == policy['allocation']
         and grant['policy_sha256'] == refs['policy']['sha256']
         and type(grant['job_id']) is int and grant['job_id'] > 0,'exact-producer-grant')
    need(facts['kind'] == 'native-producer-preflight-contract-v1' and facts['context'] == context
         and facts['disk_free_bytes'] is not None and facts['quota_headroom_bytes'] is not None
         and facts['cache_entry_limit_bytes'] is not None,'unsupported-capacity-facts')
    need(facts['disk_free_bytes'] >= plan['caps']['payload_bytes'] + plan['caps']['proof_bytes'] + plan['caps']['disk_reserve_bytes']
         and facts['quota_headroom_bytes'] >= plan['caps']['payload_bytes'] + plan['caps']['manifest_bytes']
         and facts['cache_entry_limit_bytes'] >= plan['caps']['payload_bytes'] + plan['caps']['manifest_bytes'],'insufficient-declared-budget')
    need(set(facts['tools']) == {'cargo','rustc','zig','clang','libclang'},'tool-predicates')
    for ref in facts['tools'].values(): reference(ref)
    need(publication['kind'] == 'native-producer-publication-contract-v1'
         and publication['cache_ref'] == context['ref'] and publication['save_action'] == plan['publication_contract']['cache_save_action']
         and publication['official_readback'] == 'separate-original-collector'
         and publication['service_archive_sha256'] is None,'publication-contract')


def proposed_layout(workspace, runner_temp):
    workspace,runner_temp = PurePosixPath(workspace),PurePosixPath(runner_temp)
    need(workspace.is_absolute() and runner_temp.is_absolute() and '..' not in workspace.parts + runner_temp.parts,'layout')
    payload = workspace / 'native-profile-v1'
    result = dict(checkouts={'before':str(workspace/'before'),'after':str(workspace/'product')},
        targets={label:str(payload/label) for label in ('before','after')},manifest=str(payload/'M.json'),
        proof=str(runner_temp/'native-profile-proof-v1'),debug_target=str(runner_temp/'held-scope-target'))
    roots = [PurePosixPath(path) for path in [*result['targets'].values(),result['debug_target']]]
    need(len(set(roots)) == 3 and all(not a.is_relative_to(b) and not b.is_relative_to(a)
         for i,a in enumerate(roots) for b in roots[i+1:]),'shared-native-root')
    return result


def run_matrix(plan, layout, environments, prep, run):
    arms = {}
    for label in ('before','after'):
        checkout,target = layout['checkouts'][label],layout['targets'][label]
        graphs = {graph:run(label,graph,prep.cargo_argv(graph),checkout,environments[label],plan['caps']['cargo_seconds'])
                  for graph in ('cli','host','ui')}
        triplet = {name:prep.select_binary(graphs[graph],name,graph,checkout,target)
                   for graph,name in [('cli','cagents'),('host','cagentsd'),('ui','cagents-ui')]}
        library,libraries,linked = prep.select_libraries(graphs['ui'],checkout,target)
        need(library['package_id'] == triplet['cagents-ui']['package_id'],'ui-package')
        helper = str(PurePosixPath(target)/'.qualification/fixture')
        argv = prep.fixture_argv(layout['fixture'],target,libraries,linked,helper)
        run(label,'fixture',argv,checkout,environments[label],plan['caps']['fixture_seconds'])
        arms[label] = dict(source=plan['products'][label],checkout=checkout,target=target,
            triplet=triplet,libraries=libraries,helper=helper,linked_paths=linked,
            helper_storage='cached-normal-rustc-output',cargo_json_storage='proof-only',
            external_linked_inputs='Not cached; actual Cargo search paths only. Tool/native-input receipts remain separate.')
    return arms


def validate_leaves(leaves, caps):
    need(isinstance(leaves,list) and len(leaves) <= caps['leaf_count'],'payload-count')
    paths = [leaf['path'] for leaf in leaves]
    path_set = set(paths)
    need(paths == sorted(path_set),'payload-leaf-set')
    total = 0
    for leaf in leaves:
        path = PurePosixPath(leaf['path'])
        need(path.parts and not path.is_absolute() and '..' not in path.parts and str(path) == leaf['path'],'payload-traversal')
        need(type(leaf['mode']) is int and 0 <= leaf['mode'] <= 0o777,'payload-mode')
        if leaf['type'] == 'file':
            need(set(leaf) == {'path','type','mode','bytes','sha256'} and type(leaf['bytes']) is int
                 and 0 <= leaf['bytes'] <= caps['single_leaf_bytes'],'payload-file')
            need(len(leaf['sha256']) == 64 and set(leaf['sha256']) <= set('0123456789abcdef'),'payload-hash')
            total += leaf['bytes']
        elif leaf['type'] == 'directory':
            need(set(leaf) == {'path','type','mode'},'payload-directory')
        else:
            need(leaf['type'] == 'symlink' and set(leaf) == {'path','type','mode','target'},'payload-type')
            target = PurePosixPath(leaf['target'])
            resolved = PurePosixPath(posixpath.normpath(str(path.parent / target)))
            need(not target.is_absolute() and str(resolved) not in ('.','..') and '..' not in resolved.parts,'payload-link-escape')
            need(str(resolved) in path_set,'payload-link-target')
    by_path = {leaf['path']:leaf for leaf in leaves}
    terminals = {}  # Memoized once per link; no quadratic chain rewalks.
    for original in leaves:
        if original['type'] != 'symlink': continue
        current = original['path']; trail = []; visited = set()
        while current not in terminals and by_path[current]['type'] == 'symlink':
            need(current not in visited,'payload-link-cycle')
            visited.add(current); trail.append(current)
            leaf = by_path[current]
            current = posixpath.normpath(str(PurePosixPath(current).parent/PurePosixPath(leaf['target'])))
        terminal = terminals.get(current,current)
        need(by_path[terminal]['type'] == 'file','payload-directory-link')
        for path in trail: terminals[path] = terminal
    need(total <= caps['payload_bytes'],'payload-byte-cap')
    return total


def validate_coverage(arm, leaves, graph_rows):
    paths = {leaf['path']:leaf for leaf in leaves}
    target = PurePosixPath(arm['target'])
    artifacts = [value['executable'] for value in arm['triplet'].values()] + list(arm['libraries'].values()) + [arm['helper']]
    all_outputs = [filename for rows in graph_rows.values() for row in rows if row.get('reason') == 'compiler-artifact' for filename in row.get('filenames',[])]
    need(all(PurePosixPath(filename).is_relative_to(target) for filename in all_outputs),'compiler-output-outside-role')
    artifacts += [filename for rows in graph_rows.values() for row in rows if row.get('reason') == 'compiler-artifact'
                  for filename in row.get('filenames',[]) if PurePosixPath(filename).is_relative_to(target)]
    for value in artifacts:
        path = PurePosixPath(value)
        need(path.is_relative_to(target),'uncached-artifact')
        need(str(path.relative_to(target)) in paths,'missing-cargo-output')
    for path in [*arm['triplet'].values()]:
        need(paths[str(PurePosixPath(path['executable']).relative_to(target))]['mode'] & 0o111,'artifact-executable-mode')
    need(paths[str(PurePosixPath(arm['helper']).relative_to(target))]['mode'] & 0o111,'fixture-executable-mode')


def validate_manifest(manifest, plan):
    need(manifest['kind'] == 'native-payload-manifest-v2' and manifest['products'] == plan['products'],'manifest-source')
    need(not {'key','cache_key','sha256','P','A','proof_artifact'} & set(manifest),'M-cycle')
    need(manifest['compiler_settlement']['kernel_echild'] is True
         and manifest['compiler_settlement']['errors'] == [] and manifest['compiler_settlement']['no_more_spawns'] is True
         and positive_roots(manifest['compiler_settlement']['original_roots']),'writer-unknown')
    need(list(manifest['arms']) == ['before','after'],'manifest-roles')
    total = 0
    for label in ('before','after'):
        arm = manifest['arms'][label]
        need(arm['source'] == plan['products'][label] and arm['target'] == manifest['layout']['targets'][label],'manifest-role-source')
        total += validate_leaves(arm['leaves'],plan['caps'])
        need(not PurePosixPath(manifest['layout']['manifest']).is_relative_to(PurePosixPath(arm['target'])),'M-self-inventory')
    need(total <= plan['caps']['payload_bytes'],'combined-payload-cap')
    need(manifest['fixture_sha256'] == plan['fixture_sha256'],'fixture-source')
    need(len(encoded(manifest)) <= plan['caps']['manifest_bytes'],'manifest-cap')


def verify_payload(manifest, observed, plan):
    validate_manifest(manifest,plan)
    need(set(observed) == {'before','after'},'restored-role-set')
    for label in ('before','after'):
        validate_leaves(observed[label],plan['caps'])
        need(observed[label] == manifest['arms'][label]['leaves'],'complete-payload-mismatch')


def compiler_json(manifest, read_member):
    graphs = {}
    for row in manifest['commands']:
        outputs = {}
        for name in ('stdout','stderr'):
            raw = read_member(row[name+'_member'])
            need(len(raw) == row[name]['bytes'] and digest(raw) == row[name]['sha256'],'compiler-log-bytes')
            outputs[name] = raw
        if row['graph'] in FEATURES:
            graphs[row['arm'],row['graph']] = [json.loads(line) for line in outputs['stdout'].decode().splitlines() if line.startswith('{')]
    return graphs


def verify_compiler_evidence(manifest, graph_rows, prep):
    builds = [row for row in manifest['commands'] if row['graph'] != 'metadata']
    need([(row['arm'],row['graph']) for row in builds] ==
         [(label,graph) for label in ('before','after') for graph in ('cli','host','ui','fixture')],'compiler-matrix')
    for label in ('before','after'):
        arm = manifest['arms'][label]
        for graph,name in [('cli','cagents'),('host','cagentsd'),('ui','cagents-ui')]:
            row = next(row for row in builds if (row['arm'],row['graph']) == (label,graph))
            need(row['argv'] == prep.cargo_argv(graph) and row['cwd'] == arm['checkout'] and row['exit_code'] == 0,'compiler-command')
            need(prep.select_binary(graph_rows[label,graph],name,graph,arm['checkout'],arm['target']) == arm['triplet'][name],'compiler-selection')
        library,libraries,linked = prep.select_libraries(graph_rows[label,'ui'],arm['checkout'],arm['target'])
        need(libraries == arm['libraries'] and linked == arm['linked_paths'] and library['package_id'] == arm['triplet']['cagents-ui']['package_id'],'compiler-linkage')
        row = next(row for row in builds if (row['arm'],row['graph']) == (label,'fixture'))
        need(row['argv'] == prep.fixture_argv(manifest['layout']['fixture'],arm['target'],libraries,linked,arm['helper'])
             and row['cwd'] == arm['checkout'] and row['exit_code'] == 0,'fixture-command')
        validate_coverage(arm,arm['leaves'],{graph:graph_rows[label,graph] for graph in ('cli','host','ui')})


def cache_key(raw_manifest):
    return 'native2018-v1-linux-x64-' + digest(raw_manifest)


def publication_proof(manifest_raw, context, outcome, before_rows, after_rows, save_log, expected_ref):
    # POST-TERMINAL prototype reader only, NOT a pre-upload P supplier.
    # Current --post-save refuses and keeps P UNKNOWN.
    # Internal binding only. Official authenticity/whole-job/entry wait and A
    # are supplied independently; a same-looking DTO is not that authority.
    manifest = json.loads(manifest_raw)
    need(manifest['kind'] == 'native-payload-manifest-v2'
         and all(manifest['context'][key] == value for key,value in context.items())
         and manifest['compiler_settlement']['kernel_echild'] is True
         and manifest['compiler_settlement']['errors'] == [],'publication-M-binding')
    need(set(save_log) == {'run','attempt','controller_sha','key','ref','started_at','completed_at','stdout','stderr'},'save-log-shape')
    reference(save_log['stdout']); reference(save_log['stderr'])
    key = cache_key(manifest_raw)
    need(outcome == 'success','save-not-successful')
    need(save_log['run'] == context['run'] and save_log['attempt'] == context['attempt']
         and save_log['controller_sha'] == context['controller_sha'] and save_log['key'] == key
         and save_log['ref'] == expected_ref and save_log['started_at'] < save_log['completed_at'],'save-log-binding')
    rows = [row for row in after_rows if row.get('key') == key and row.get('ref') == expected_ref]
    need(len(rows) == 1,'publication-missing-or-ambiguous')
    row = rows[0]
    need(type(row.get('id')) is int and row['id'] > 0 and row['id'] not in {value['id'] for value in before_rows}
         and row.get('version') and type(row.get('size_in_bytes')) is int and row['size_in_bytes'] > 0
         and save_log['started_at'] <= row['created_at'] <= save_log['completed_at'],'publication-not-this-save')
    return dict(kind='native-cache-publication-proof-v1',M_sha256=digest(manifest_raw),context=context,
        key=key,official_metadata={name:row[name] for name in ('id','ref','key','version','size_in_bytes','created_at')},
        service_archive_sha256=None,save_log=save_log,external_acceptance=None,qualified=False)


def positive_roots(summary):
    return (summary['streams_closed'] is True and bool(summary['children'])
        and all(child['identity'] is not None and child['published'] is True and child['error'] is None
                and child['waited'] is True and child['exit_code'] == 0 for child in summary['children']))


def settle_writers(owner, waitpid, now, sleep, deadline, nohang):
    owner.spawning_closed = True
    summary = owner.preparation.settle_preparation(owner)
    errors = list(summary['errors'])
    adopted = []
    kernel_echild = False
    children = summary['children']
    positive = positive_roots(summary)
    if not positive: errors.append('Original root/stream ledger is not positive')
    if all(child['waited'] and child['exit_code'] is not None for child in children):
        while now() < deadline:
            try:
                pid,status = waitpid(-1,nohang)
            except ChildProcessError:
                kernel_echild = True
                break
            if pid:
                adopted.append(dict(pid=pid,status=status))  # A real kernel wait result, never kill authority.
                if status != 0: errors.append('Nonzero/signalled adopted writer; publication refused')
            else:
                sleep(.05)
        if not kernel_echild: errors.append('Scoped subreaper ECHILD not observed; writer custody UNKNOWN')
    else:
        errors.append('Original root ledger unresolved; adopted closure UNKNOWN')
    return dict(original_roots=summary,adopted_waits=adopted,kernel_echild=kernel_echild,
                no_more_spawns=True,errors=errors,external_entry_wait=None)


def drain_streams(poll, read, write, now, deadline, limit, total_limit, checkpoint):
    """Bounded synchronous pipe drain; no background Python writer thread."""
    counts = {'stdout':0,'stderr':0}
    while True:
        checkpoint()
        need(now() < deadline,'producer-command-deadline')
        events,finished = poll()
        for name,token in events:
            raw = read(token)
            if raw:
                counts[name] += len(raw)
                need(counts[name] <= limit and sum(counts.values()) <= total_limit,'producer-log-cap')
                write(name,raw)
        if finished: return counts


def owner_type(preparation):
    class ProducerChildren(preparation.PreparationChildren):
        def __init__(self, *args):
            super().__init__(*args)
            self.preparation = preparation
            self.spawning_closed = False
            self.selectors = []
            self.log_total = 0
            self.disk_check = lambda:None

        def start(self, *args, **kwargs):
            need(not self.spawning_closed,'Popen-after-writer-boundary')
            return super().start(*args,**kwargs)

        def logged(self, argv, cwd, env, timeout, prefix, caps, compile_deadline):
            with self.cancellation.work():
                output = {name:self.open_output(Path(str(prefix)+'.'+name)) for name in ('stdout','stderr')}
                process = self.start(argv,cwd,env,subprocess.PIPE,subprocess.PIPE)
                with self.cancellation.defer():
                    selector = selectors.DefaultSelector(); self.selectors.append(selector)
                    for name,stream in [('stdout',process.stdout),('stderr',process.stderr)]:
                        selector.register(stream,selectors.EVENT_READ,name)
                def poll():
                    ready = [(key.data,key.fileobj) for key,mask in selector.select(.1)]
                    return ready,not selector.get_map()
                def read(stream):
                    raw = os.read(stream.fileno(),65536)
                    if not raw: selector.unregister(stream)
                    return raw
                deadline = min(time.monotonic()+timeout,compile_deadline)
                counts = drain_streams(poll,read,lambda name,raw:output[name].write(raw),time.monotonic,deadline,
                    caps['stream_bytes'],caps['log_bytes']-self.log_total,
                    lambda:(self.cancellation.checkpoint(),self.disk_check()))
                code = process.wait(timeout=max(.001,deadline-time.monotonic()))
                self.records[process].update(waited=True,exit_code=code)
                need(code == 0,'producer-command-failed')
                with self.cancellation.defer():
                    selector.close()
                    for stream in output.values(): stream.close()
                self.log_total += sum(counts.values())
                return counts

        def cleanup(self):
            summary = super().cleanup()
            for selector in self.selectors:
                try: selector.close()
                except BaseException as error:
                    self.cancellation.latch(error); summary['errors'].append('Pipe selector: ' + repr(error))
            return summary
    return ProducerChildren


def file_ref(path):
    raw = path.read_bytes()
    return dict(path=str(path),sha256=digest(raw),bytes=len(raw))


def load_ref(ref):
    reference(ref); path = Path(ref['path'])
    need(path.resolve(strict=True) == path and file_ref(path) == ref,'input-reference-bytes')
    return json.loads(path.read_bytes())


def inventory(root, caps, deadline, inode_roles, label):
    leaves = []; total = 0; inode_counts = {}; observed = {}
    need(root.resolve(strict=True) == root and root.is_dir(),'payload-root-alias')
    for directory,dirs,names in os.walk(root,followlinks=False):
        need(time.monotonic() < deadline,'inventory-deadline')
        for name in sorted(dirs):
            path = Path(directory)/name
            need(len(leaves) < caps['leaf_count'] and not path.is_symlink(),'directory-link-alias-or-cap')
            # Bind directory modes/empty directories too, not only file bytes.
            leaves.append(dict(path=str(path.relative_to(root)),type='directory',mode=stat.S_IMODE(path.lstat().st_mode)))
        for name in sorted(names):
            need(len(leaves) < caps['leaf_count'] and time.monotonic() < deadline,'inventory-cap')
            path = Path(directory)/name; info = path.lstat(); relative = str(path.relative_to(root))
            leaf = dict(path=relative,mode=stat.S_IMODE(info.st_mode))
            if stat.S_ISREG(info.st_mode):
                total += info.st_size
                need(info.st_size <= caps['single_leaf_bytes'] and total <= caps['payload_bytes'],'payload-disk-cap')
                identity = (info.st_dev,info.st_ino)
                need(inode_roles.setdefault(identity,label) == label,'cross-role-hardlink-alias')
                inode_counts[identity] = inode_counts.get(identity,0)+1; observed[identity] = info.st_nlink
                hashed = hashlib.sha256()
                with path.open('rb') as stream:
                    while True:
                        need(time.monotonic() < deadline,'inventory-deadline')
                        raw = stream.read(1048576)
                        if not raw: break
                        hashed.update(raw)
                after = path.lstat()
                need((info.st_dev,info.st_ino,info.st_size,info.st_mtime_ns) ==
                     (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns),'payload-changed-during-inventory')
                leaf.update(type='file',bytes=info.st_size,sha256=hashed.hexdigest())
            else:
                need(stat.S_ISLNK(info.st_mode) and path.resolve(strict=True).is_relative_to(root)
                     and not path.resolve(strict=True).is_dir(),'unsafe-payload-leaf')
                leaf.update(type='symlink',target=os.readlink(path))
            leaves.append(leaf)
    need(all(observed[key] == count for key,count in inode_counts.items()),'external-hardlink-alias')
    leaves.sort(key=lambda value:value['path']); validate_leaves(leaves,caps)
    return leaves


def user_context(env):
    need(env.get('RUNNER_ENVIRONMENT') == 'github-hosted' and 'CAGENTS_AGENT_AUTHORITY' not in env,
         'Genuine hosted User required; inherited authority is never removed')
    return dict(repository=env['GITHUB_REPOSITORY'],event=env['GITHUB_EVENT_NAME'],branch=env['GITHUB_HEAD_REF'],
        ref=env['GITHUB_REF'],pull_request=int(env['PRODUCER_PULL_REQUEST']),run=int(env['GITHUB_RUN_ID']),
        attempt=int(env['GITHUB_RUN_ATTEMPT']),job_name=env['GITHUB_JOB'],controller_sha=env['PRODUCER_CONTROL_SHA'])


def main():
    context = user_context(dict(os.environ))
    need(len(sys.argv) == 3 and sys.argv[1] in ('--admission-only','--produce','--post-save'),'producer-mode')
    control = Path(sys.argv[2])
    plan = json.loads((control/'.github/qualification/native-profile-producer-plan.json').read_bytes())
    # CURRENT hard refusal occurs before any metadata child, target/cache
    # inspection, tool lookup, disk/quota query, mkdir or resource creation.
    admit_references(plan)
    documents = admitted_contract(plan,context,load_ref)
    if sys.argv[1] == '--admission-only': return
    layout = proposed_layout(os.environ['GITHUB_WORKSPACE'],os.environ['RUNNER_TEMP'])
    if sys.argv[1] == '--post-save':
        # Save exposes no ID/version/hash outputs. Without separate original
        # official readback, preserve UNKNOWN instead of inventing publication.
        proof = Path(layout['proof']); raw = Path(layout['manifest']).read_bytes()
        report = dict(kind='native-cache-publication-pending-v1',M_sha256=digest(raw),key=cache_key(raw),
            context=context,save_outcome=os.environ.get('PRODUCER_SAVE_OUTCOME'),official_metadata=None,
            service_archive_sha256=None,external_entry_wait=None,external_acceptance=None,qualified=False)
        (proof/'P-pending.json').write_bytes(encoded(report)+b'\n')
        raise ValueError('Publication UNKNOWN: original official readback and independent acceptance required')
    prep_path = control/'.github/qualification/final-main-prepare.py'
    need(digest(prep_path.read_bytes()) == plan['preparation_module_sha256'],'preparation-source')
    spec = importlib.util.spec_from_file_location('reviewed_preparation',prep_path)
    prep = importlib.util.module_from_spec(spec); spec.loader.exec_module(prep)
    cancel = prep.PreparationCancellation(); cancel.install()
    owner = owner_type(prep)(cancel,subprocess.Popen,prep.original_identity,os.killpg)
    began = time.monotonic(); proof = None; manifest = None; records = []; graphs = {}
    try:
        with cancel.work():
            libc = ctypes.CDLL(None,use_errno=True)
            need(libc.prctl(36,1,0,0,0) == 0,'subreaper-unavailable')
            flag = ctypes.c_int(); need(libc.prctl(37,ctypes.byref(flag),0,0,0) == 0 and flag.value == 1,'subreaper-not-established')
            root = Path(layout['manifest']).parent; proof = Path(layout['proof'])
            need(not root.exists() and not proof.exists(),'producer-path-already-exists-no-fallback')
            for name in ('GITHUB_WORKSPACE','RUNNER_TEMP'):
                path = Path(os.environ[name]); info = path.stat()
                need(path.resolve(strict=True) == path and path.is_dir() and info.st_uid == os.geteuid(),'owned-canonical-job-root')
            need(control.resolve(strict=True) == control,'controller-alias')
            need(os.statvfs(root.parent).f_bavail*os.statvfs(root.parent).f_frsize >=
                 plan['caps']['payload_bytes']+plan['caps']['proof_bytes']+plan['caps']['disk_reserve_bytes'],'actual-disk-insufficient')
            root.mkdir(mode=0o700); proof.mkdir(mode=0o700); (proof/'logs').mkdir(mode=0o700)
            initial_free = os.statvfs(root.parent).f_bavail*os.statvfs(root.parent).f_frsize
            def disk_check():
                value = os.statvfs(root.parent); free = value.f_bavail*value.f_frsize
                need(free >= plan['caps']['disk_reserve_bytes'] and initial_free-free <=
                     plan['caps']['payload_bytes']+plan['caps']['proof_bytes'],'sampled-disk-budget')
            owner.disk_check = disk_check  # Conservative sampled stop, not a filesystem quota guarantee.
            private = Path(tempfile.mkdtemp(prefix='p2018.',dir='/tmp')).resolve(strict=True); private.chmod(0o700)
            need(private.parent == Path('/tmp') and len(str(private)) < 30 and private.stat().st_uid == os.geteuid(),'short-owned-private-root')
            layout['fixture'] = str(control/'.github/qualification/final-main-fixture.rs')
            need(digest(Path(layout['fixture']).read_bytes()) == plan['fixture_sha256'],'fixture-source')
            environments = {}
            for label in ('before','after'):
                target = Path(layout['targets'][label]); target.mkdir(mode=0o700); (target/'.qualification').mkdir(mode=0o700)
                arm_root = private/label; arm_root.mkdir(mode=0o700)
                for suffix in prep.PRIVATE.values(): (arm_root/suffix).mkdir(mode=0o700)
                environments[label] = prep.environment(dict(os.environ),str(private),label,
                    dict(target=str(target),tools=documents['preflight']['tools']))
                need(environments[label].get('CARGO_BUILD_JOBS') == '2' and environments[label].get('RUSTUP_TOOLCHAIN') == '1.93.0','toolchain-configuration')
            for ref in documents['preflight']['tools'].values(): need(prep.file_binding(ref['path']) == ref,'actual-tool-bytes')
            for label,env in environments.items():
                for name in ('cargo','rustc','zig'):
                    need(str(Path(shutil.which(name,path=env['PATH'])).resolve(strict=True)) == documents['preflight']['tools'][name]['path'],'selected-tool-path')
                if env.get('CC'):
                    need(str(Path(env['CC']).resolve(strict=True)) == documents['preflight']['tools']['clang']['path'],'selected-clang-path')
                if env.get('CAGENTS_ZIG'):
                    need(str(Path(env['CAGENTS_ZIG']).resolve(strict=True)) == documents['preflight']['tools']['zig']['path'],'selected-zig-override')
            def execute(label, graph, argv, cwd, env, timeout):
                prefix = proof/'logs'/(label+'-'+graph+'-'+str(len(records)))
                owner.logged(argv,cwd,env,timeout,prefix,plan['caps'],began+plan['caps']['compile_seconds'])
                row = dict(arm=label,graph=graph,argv=list(argv),cwd=cwd,exit_code=0,
                    stdout=file_ref(Path(str(prefix)+'.stdout')),stderr=file_ref(Path(str(prefix)+'.stderr')),
                    stdout_member='logs/'+prefix.name+'.stdout',stderr_member='logs/'+prefix.name+'.stderr',storage='proof-only')
                records.append(row)
                return Path(row['stdout']['path']).read_bytes()
            for name,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]:
                raw = execute('controller','metadata',['git','rev-parse',ref],str(control),None,30)
                expected = context['controller_sha'] if name == 'sha' else documents['grant']['controller_tree']
                need(raw.decode().strip() == expected,'actual-controller-source')
            need(not execute('controller','metadata',['git','status','--porcelain=v1'],str(control),None,30),'dirty-controller')
            for label,checkout in layout['checkouts'].items():
                for name,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]:
                    raw = execute(label,'metadata',['git','rev-parse',ref],checkout,environments[label],plan['caps']['metadata_seconds'])
                    need(raw.decode().strip() == plan['products'][label][name],'checkout-source')
                need(not execute(label,'metadata',['git','status','--porcelain=v1'],checkout,environments[label],plan['caps']['metadata_seconds']),'dirty-source')
            configurations = {label:prep.configuration(checkout,environments[label]) for label,checkout in layout['checkouts'].items()}
            inputs = {label:{name:prep.file_binding(Path(checkout)/name) for name in prep.SOURCES}
                      for label,checkout in layout['checkouts'].items()}
            for label in ('before','after'):
                for argv in (['cargo','--version','--verbose'],['rustc','--version','--verbose'],['zig','env']):
                    raw = execute(label,'metadata',argv,layout['checkouts'][label],environments[label],30)
                    if argv[0] in ('cargo','rustc'): need(b'1.93.0' in raw,'actual-rust-version')
                    else:
                        versions = re.findall(r'^\s*(?:"version"\s*:|\.version\s*=)\s*"([^"]+)"',raw.decode(),re.M)
                        need(versions == ['0.15.2'],'actual-zig-version')
            def run(label, graph, argv, cwd, env, timeout):
                raw = execute(label,graph,argv,cwd,env,timeout)
                rows = [json.loads(line) for line in raw.decode().splitlines() if line.startswith('{')]
                graphs[label,graph] = rows
                return rows
            arms = run_matrix(plan,layout,environments,prep,run)
            for label,checkout in layout['checkouts'].items():
                need(prep.configuration(checkout,environments[label]) == configurations[label],'configuration-changed')
                for name,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]:
                    need(execute(label,'metadata',['git','rev-parse',ref],checkout,environments[label],30).decode().strip() == plan['products'][label][name],'source-changed')
                need(not execute(label,'metadata',['git','status','--porcelain=v1'],checkout,environments[label],30),'source-changed')
                need({name:prep.file_binding(Path(checkout)/name) for name in prep.SOURCES} == inputs[label],'compile-input-changed')
                arms[label]['compile_inputs'] = inputs[label]
                arms[label]['environment_hashes'] = prep.environment_binding(environments[label])
                arms[label]['configuration'] = configurations[label]
            manifest = dict(kind='native-payload-manifest-v2',products=plan['products'],context={**context,'job_id':documents['grant']['job_id']},
                layout=layout,arms=arms,commands=records,fixture_sha256=plan['fixture_sha256'],
                controller_sha=context['controller_sha'],controller_tree=documents['grant']['controller_tree'],
                tool_inputs=documents['preflight']['tools'],admission_refs=plan['admission_inputs'],plan_sha256=digest((control/'.github/qualification/native-profile-producer-plan.json').read_bytes()),
                external_entry_wait=None,native_validation=False,future_freshness=False)
    except BaseException as error:
        cancel.latch(error)
    finally:
        try:
            settlement = settle_writers(owner,os.waitpid,time.monotonic,time.sleep,
                min(began+plan['caps']['entry_seconds'],time.monotonic()+plan['caps']['adopted_wait_seconds']),os.WNOHANG)
        except BaseException as error:
            cancel.latch(error); settlement = dict(errors=['Writer settlement UNKNOWN: '+repr(error)],kernel_echild=False,no_more_spawns=True)
        if proof is not None:
            try: (proof/'compiler-settlement.json').write_bytes(encoded(settlement)+b'\n')
            except BaseException as error: cancel.latch(error)
    cancel.finish(settlement['errors'])
    need(manifest is not None,'no-complete-manifest')
    inode_roles = {}; inventory_deadline = min(began+plan['caps']['entry_seconds'],time.monotonic()+plan['caps']['inventory_seconds'])
    for label,arm in manifest['arms'].items():
        arm['leaves'] = inventory(Path(arm['target']),plan['caps'],inventory_deadline,inode_roles,label)
        validate_coverage(arm,arm['leaves'],{graph:graphs[label,graph] for graph in ('cli','host','ui')})
    manifest['compiler_settlement'] = settlement
    validate_manifest(manifest,plan)
    retained_graphs = compiler_json(manifest,lambda member:(proof/member).read_bytes())
    need(retained_graphs == {key:value for key,value in graphs.items() if key[1] in FEATURES},'retained-cargo-json')
    verify_compiler_evidence(manifest,retained_graphs,prep)
    need(sum(path.stat().st_size for path in proof.rglob('*') if path.is_file())+len(encoded(manifest)) <= plan['caps']['proof_bytes'],'proof-size-cap')
    need(time.monotonic() < began+plan['caps']['entry_seconds'],'producer-entry-deadline')
    raw = encoded(manifest)+b'\n'
    with Path(layout['manifest']).open('xb') as stream:
        stream.write(raw); stream.flush(); os.fsync(stream.fileno())
    Path(layout['manifest']).chmod(0o444)
    (proof/'M.json').write_bytes(raw)  # Evidence bytes only, never Cargo output relocation.
    cancel.finish(settlement['errors'])
    with Path(os.environ['GITHUB_OUTPUT']).open('a') as stream:
        stream.write('key='+cache_key(raw)+'\nmanifest_sha256='+digest(raw)+'\n')
    # This is pre-exit compiler/payload evidence, never a later-exit witness.


if __name__ == '__main__':
    main()
