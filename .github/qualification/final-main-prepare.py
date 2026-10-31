"""Stdlib-only, globally gated preparation before the first private desktop.

No current producer roles are admitted. Changing the pins below requires a new
exact source review and execution grant; data supplied at runtime cannot do it.
Normal run-development remains mandatory and may perform real Cargo rechecks.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

PINNED_ROLES = {'before': None, 'after': None}  # Both genuine inventories UNESTABLISHED.
FEATURES = {'cli': ['development'], 'host': ['development','host'], 'ui': ['development','ui']}
SOURCES = ['Cargo.toml','Cargo.lock','.cargo/config.toml','build.rs','mise.toml','scripts/run-development']
PRIVATE = {'HOME':'home','XDG_DATA_HOME':'data','XDG_CONFIG_HOME':'config','XDG_CACHE_HOME':'cache',
           'XDG_STATE_HOME':'state','TMPDIR':'tmp','XDG_RUNTIME_DIR':'runtime'}
RENDERER = {'ICED_BACKEND':'wgpu','WGPU_BACKEND':'gl','LIBGL_ALWAYS_SOFTWARE':'1'}
HEX = re.compile('[0-9a-f]{64}')
LATE = {'DISPLAY','XAUTHORITY','DBUS_SESSION_BUS_ADDRESS','AT_SPI_BUS_ADDRESS'}
ENV_KEYS = {'PATH','HOME','CARGO_HOME','RUSTUP_HOME','LIBCLANG_PATH','CLANG_PATH','LD_LIBRARY_PATH',
            'CC','CXX','AR','CFLAGS','CXXFLAGS','CPPFLAGS','LDFLAGS','SOURCE_DATE_EPOCH','HOST','TARGET',
            'TMPDIR','ICED_BACKEND','WGPU_BACKEND','LIBGL_ALWAYS_SOFTWARE','ACCESSIBILITY_ENABLED','NO_AT_BRIDGE','WINIT_UNIX_BACKEND'}
ENV_PREFIXES = ('CARGO_','RUST','CAGENTS_','ZIG_','PKG_CONFIG','CMAKE_','BINDGEN_','DEP_','XDG_',
                'CC_','CXX_','AR_','CFLAGS_','CXXFLAGS_','LDFLAGS_')


def need(value, reason):
    if not value:
        raise ValueError(reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def path_text(value):
    need(isinstance(value,str), 'path')
    path = Path(value)
    need(path.is_absolute() and '..' not in path.parts and str(path) == value, 'path')
    return path


def reference(ref):
    need(isinstance(ref,dict) and set(ref) == {'path','sha256','bytes'}, 'producer-reference')
    path_text(ref['path'])
    need(type(ref['bytes']) is int and ref['bytes'] > 0 and HEX.fullmatch(ref['sha256']), 'producer-reference')


def admit_references(roles, pinned):
    # Pure metadata MUST precede every inventory read, cache/path inspection,
    # directory creation, subprocess, Cargo command or native resource factory.
    need(set(roles) == set(pinned) == {'before','after'}
         and all(roles[label] is not None and pinned[label] is not None for label in ('before','after')),
         'Native roles UNESTABLISHED; no Cargo/cache inspection/resource creation permitted')
    for label in ('before','after'):
        reference(roles[label]); reference(pinned[label])
        need(roles[label] == pinned[label], 'unpinned-producer')


def validate_inventories(plan, refs, inventories, debug_target):
    need(set(inventories) == {'before','after'}, 'both-inventories')
    targets = []
    for label in ('before','after'):
        value = inventories[label]
        need(set(value) == {'kind','role','source','checkout','target','producer','cache','profiles','tools','inputs'}, 'inventory-shape')
        need(value['kind'] == 'native-role-producer-inventory-v1' and value['role'] == label, 'producer-role')
        need(value['source'] == plan['before' if label == 'before' else 'product'], 'producer-source')
        path_text(value['checkout']); target = path_text(value['target']); targets.append(target)
        producer = value['producer']
        need(set(producer) == {'run','attempt','job','head','workflow_sha256','custody'}, 'producer-identity')
        need(all(type(producer[key]) is int and producer[key] > 0 for key in ('run','attempt','job'))
             and re.fullmatch('[0-9a-f]{40}',producer['head']) and HEX.fullmatch(producer['workflow_sha256']), 'producer-identity')
        reference(producer['custody'])
        cache = value['cache']
        # Publication is a genuine LOCAL receipt, never a service archive hash.
        # actions/cache save exposes no archive digest; unsupported fields stay unknown.
        need(set(cache) == {'key','version','publication','service_archive_sha256'}
             and isinstance(cache['key'],str) and cache['key']
             and (cache['version'] is None or isinstance(cache['version'],str) and cache['version'])
             and cache['service_archive_sha256'] is None, 'producer-cache')
        reference(cache['publication'])
        need(value['profiles'] == {'profile':'dev-runtime','graphs':FEATURES}, 'producer-profiles')
        need(set(value['tools']) == {'cargo','rustc','zig','clang','libclang'}, 'producer-tools')
        for ref in value['tools'].values(): reference(ref)
        need(set(value['inputs']) == set(SOURCES) and all(HEX.fullmatch(v) for v in value['inputs'].values()), 'producer-inputs')
        reference(refs[label])
    debug = path_text(debug_target)
    need(targets[0] != targets[1] and all(target != debug and not target.is_relative_to(debug)
         and not debug.is_relative_to(target) for target in targets)
         and not targets[0].is_relative_to(targets[1]) and not targets[1].is_relative_to(targets[0]), 'distinct-native-roots')
    return inventories


def admitted(plan, pinned, read_inventory, inspect_roles, debug_target):
    refs = plan['native_preparation']['roles']
    admit_references(refs,pinned)
    values = {label:read_inventory(refs[label]) for label in ('before','after')}
    validate_inventories(plan,refs,values,debug_target)
    inspect_roles(values,debug_target)  # Only after BOTH complete source-bound inventories.
    return values


def environment(parent, base, label, inventory):
    root = path_text(base) / label
    env = dict(parent)  # Never remove inherited caller/tracking authority.
    env.update({key:str(root / suffix) for key,suffix in PRIVATE.items()})
    env.update(RENDERER,ACCESSIBILITY_ENABLED='1',NO_AT_BRIDGE='0',WINIT_UNIX_BACKEND='x11',XDG_CURRENT_DESKTOP='GNOME',
               CARGO_TARGET_DIR=inventory['target'],CAGENTS_LOCAL_DEBUG='1',
               RUST_BACKTRACE=parent.get('RUST_BACKTRACE','1'),RUST_LOG=parent.get('RUST_LOG','cagents=debug,cagentsd=debug,warn'),
               CARGO_HOME=parent.get('CARGO_HOME',str(path_text(parent['HOME']) / '.cargo')),
               RUSTUP_HOME=parent.get('RUSTUP_HOME',str(path_text(parent['HOME']) / '.rustup')),
               XDG_DESKTOP_PORTAL_DIR=str(root / 'portal-descriptors'))
    env['PATH'] = str(path_text(inventory['tools']['zig']['path']).parent) + os.pathsep + parent['PATH']
    return env


def environment_binding(env):
    # Values are not dumped: only whitelisted build-affecting names/digests.
    return {key:digest(env[key].encode()) for key in sorted(env)
            if key not in LATE and (key in ENV_KEYS or key.startswith(ENV_PREFIXES))}


def cargo_argv(graph):
    targets = {'cli':['--bin','cagents'],'host':['--bin','cagentsd'],'ui':['--lib','--bin','cagents-ui']}
    return ['cargo','build','--locked','--profile','dev-runtime','--no-default-features',
            '--features',','.join(FEATURES[graph]),*targets[graph],'--message-format=json']


def select_binary(rows, name, graph, checkout, target):
    candidates = [row for row in rows if row.get('reason') == 'compiler-artifact'
        and row.get('target',{}).get('name') == name and row['target'].get('kind') == ['bin']
        and row.get('profile',{}).get('test') is False and row.get('executable')]
    need(len(candidates) == 1, 'prepared-binary-artifact')
    value = candidates[0]
    need(set(value['features']) == set(FEATURES[graph]), 'prepared-features')
    source = 'src/main.rs' if name == 'cagents' else 'src/bin/' + name + '.rs'
    need(value['target'].get('src_path') == str(path_text(checkout) / source),'prepared-binary-source')
    need(value.get('manifest_path',str(path_text(checkout) / 'Cargo.toml')) == str(path_text(checkout) / 'Cargo.toml')
         and value['package_id'], 'prepared-package')
    executable = path_text(value['executable'])
    need(executable == path_text(target) / 'dev-runtime' / name, 'prepared-binary-target')
    return value


def select_libraries(rows, checkout, target):
    own = [row for row in rows if row.get('reason') == 'compiler-artifact'
        and row.get('target',{}).get('name') == 'cagents' and row['target'].get('kind') == ['lib']
        and row['target'].get('src_path') == str(path_text(checkout) / 'src/lib.rs') and row.get('profile',{}).get('test') is False]
    need(len(own) == 1 and set(own[0]['features']) == set(FEATURES['ui']), 'prepared-ui-library')
    serde = [row for row in rows if row.get('reason') == 'compiler-artifact'
        and row.get('target',{}).get('name') == 'serde_json' and row['target'].get('kind') == ['lib']]
    need(len(serde) == 1, 'prepared-serde-library')
    libraries = {}
    for name, row in [('cagents',own[0]),('serde_json',serde[0])]:
        paths = [value for value in row['filenames'] if value.endswith('.rlib')]
        need(len(paths) == 1 and path_text(paths[0]).is_relative_to(path_text(target) / 'dev-runtime/deps'), 'prepared-library-target')
        libraries[name] = paths[0]
    linked = [path for row in rows if row.get('reason') == 'build-script-executed' for path in row.get('linked_paths',[])]
    return own[0], libraries, linked


def fixture_argv(source, target, libraries, linked, helper):
    argv = ['rustc','--edition=2024',source,'--extern','cagents=' + libraries['cagents'],
            '--extern','serde_json=' + libraries['serde_json'],'-L','dependency=' + str(path_text(target) / 'dev-runtime/deps'),'-o',helper]
    for path in linked: argv += ['-L',path]
    return argv


def compile_matrix(inventories, envs, fixture_source, base, run):
    result = {}
    for label in ('before','after'):
        inventory = inventories[label]; checkout, target = inventory['checkout'], inventory['target']
        graphs = {graph:run(label,graph,cargo_argv(graph),checkout,envs[label],4200) for graph in ('cli','host','ui')}
        triplet = {name:select_binary(graphs[graph],name,graph,checkout,target)
                   for graph,name in [('cli','cagents'),('host','cagentsd'),('ui','cagents-ui')]}
        library, libraries, linked = select_libraries(graphs['ui'],checkout,target)
        need(library['package_id'] == triplet['cagents-ui']['package_id'], 'prepared-ui-package')
        helper = str(path_text(base) / (label + '-fixture'))
        argv = fixture_argv(fixture_source,target,libraries,linked,helper)
        run(label,'fixture',argv,checkout,envs[label],180)
        result[label] = dict(inventory=inventory,triplet=triplet,libraries=libraries,helper=helper,
                             environment_sha256=digest(encoded(environment_binding(envs[label]))))
    return result


def file_binding(path):
    path = path_text(str(path)); need(path.resolve(strict=True) == path, 'prepared-file-alias')
    info = path.stat(); need(stat.S_ISREG(info.st_mode), 'prepared-file')
    return dict(path=str(path),sha256=digest(path.read_bytes()),bytes=info.st_size)


def read_inventory(ref):
    reference(ref); actual = file_binding(ref['path']); need(actual == ref,'producer-bytes')
    return json.loads(Path(ref['path']).read_bytes())


def inspect_roles(values, debug_target):
    roots = [path_text(value['target']) for value in values.values()] + [path_text(debug_target)]
    identities = [(path.stat().st_dev,path.stat().st_ino) for path in roots]
    need(len(set(identities)) == 3,'native-root-alias')
    for inventory in values.values():
        for key in ('checkout','target'):
            path = path_text(inventory[key]); info = path.stat()
            need(path.resolve(strict=True) == path and stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid(), 'owned-canonical-role')
        for ref in (inventory['producer']['custody'],inventory['cache']['publication']):
            need(file_binding(ref['path']) == ref,'producer-local-receipt-bytes')
        for name, ref in inventory['tools'].items(): need(file_binding(ref['path']) == ref,'producer-tool-bytes')
        for relative, expected in inventory['inputs'].items():
            need(file_binding(path_text(inventory['checkout']) / relative)['sha256'] == expected,'producer-input-bytes')


def configuration(checkout, env):
    # Bind both present and absent configuration inputs, never dump contents.
    root = path_text(checkout)
    homes = [root,*root.parents,path_text(env['CARGO_HOME'])]
    candidates = {str(home / '.cargo' / name) for home in homes[:-1] for name in ('config','config.toml')}
    candidates |= {str(homes[-1] / name) for name in ('config','config.toml')}
    result = {}
    for value in sorted(candidates):
        path = Path(value)
        result[value] = file_binding(path.resolve(strict=True)) if path.exists() else None
    for key in ('CC','CXX'):
        if env.get(key): result[key] = file_binding(Path(env[key]).resolve(strict=True))
    return result


def require_user():
    need(os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted' and 'CAGENTS_AGENT_AUTHORITY' not in os.environ,'Genuine hosted User required; no authority removal')


def command(argv, cwd, env, stdout, stderr, timeout):
    with stdout.open('xb') as output, stderr.open('xb') as errors:
        process = subprocess.Popen(argv,cwd=cwd,env=env,stdout=output,stderr=errors,start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except BaseException:
            # Only the still-unreaped original root may authorize this group.
            if process.poll() is None:
                os.killpg(process.pid,signal.SIGTERM)
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                if process.poll() is None: os.killpg(process.pid,signal.SIGKILL)
                process.wait(timeout=10)
            raise
    need(code == 0,'preparation-command-failed')


def verify_data(record, plan, control, module_sha, fixture_sha, pinned=PINNED_ROLES):
    need(record['kind'] == 'both-heads-prepared-v1' and record['product'] == plan['product'] and record['before'] == plan['before'],'prepared-source')
    need(record['control'] == control and record['module_sha256'] == module_sha and record['fixture_sha256'] == fixture_sha,'prepared-controller')
    need(record['roles'] == plan['native_preparation']['roles'],'prepared-roles')
    admit_references(record['roles'],pinned)
    need(list(record['arms']) == ['before','after'] and len(record['commands']) == 8,'prepared-both-heads')
    expected = [(label,graph) for label in ('before','after') for graph in ('cli','host','ui','fixture')]
    need([(row['arm'],row['graph']) for row in record['commands']] == expected
         and all(row['exit_code'] == 0 for row in record['commands']),'prepared-command-order')


def verify_artifacts(record, compiled, fixture_source):
    for label in ('before','after'):
        arm = record['arms'][label]; inventory = arm['inventory']
        checkout,target = inventory['checkout'],inventory['target']
        for graph,name in [('cli','cagents'),('host','cagentsd'),('ui','cagents-ui')]:
            selected = select_binary(compiled[label,graph],name,graph,checkout,target)
            need(arm['triplet'][name] == selected,'prepared-selected-binary')
            row = next(row for row in record['commands'] if (row['arm'],row['graph']) == (label,graph))
            need(row['argv'] == cargo_argv(graph),'prepared-command-argv')
        library,libraries,linked = select_libraries(compiled[label,'ui'],checkout,target)
        need(library['package_id'] == arm['triplet']['cagents-ui']['package_id'] and libraries == arm['libraries'],'prepared-selected-library')
        helper = str(path_text(record['base']) / (label + '-fixture'))
        row = next(row for row in record['commands'] if (row['arm'],row['graph']) == (label,'fixture'))
        need(arm['helper'] == helper and row['argv'] == fixture_argv(fixture_source,target,libraries,linked,helper),'prepared-helper')
        paths = {name:value['executable'] for name,value in arm['triplet'].items()}
        paths.update({'library-' + name:path for name,path in libraries.items()}); paths['fixture'] = helper
        need(set(arm['files']) == set(paths) and all(arm['files'][name]['path'] == path for name,path in paths.items()),'prepared-file-set')
        for ref in arm['files'].values(): reference(ref)


def verify_environment(arm, env):
    need(digest(encoded(environment_binding(env))) == arm['environment_sha256'],'prepared-runtime-environment')


def check_arm_files(arm):
    for binding in arm['files'].values(): need(file_binding(binding['path']) == binding,'prepared-runtime-bytes')


def load_prepared(control, plan, checkouts, parent, control_identity):
    record_path = Path(parent['QUALIFICATION_EVIDENCE']) / 'preparation/prepared.json'
    raw = record_path.read_bytes(); record = json.loads(raw)
    verify_data(record,plan,control_identity,digest(Path(__file__).read_bytes()),digest((control / '.github/qualification/final-main-fixture.rs').read_bytes()))
    validate_inventories(plan,record['roles'],{label:record['arms'][label]['inventory'] for label in ('before','after')},parent['CARGO_TARGET_DIR'])
    inspect_roles({label:record['arms'][label]['inventory'] for label in ('before','after')},parent['CARGO_TARGET_DIR'])
    compiled = {}
    for row in record['commands']:
        need(file_binding(row['stdout']['path']) == row['stdout'] and file_binding(row['stderr']['path']) == row['stderr'],'prepared-command-bytes')
        compiled[row['arm'],row['graph']] = [json.loads(line) for line in Path(row['stdout']['path']).read_text().splitlines() if line.startswith('{')]
    verify_artifacts(record,compiled,str(control / '.github/qualification/final-main-fixture.rs'))
    root = path_text(record['base']); info = root.stat()
    need(root.resolve(strict=True) == root and info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o700
         and root.parent == Path('/tmp') and root.name.startswith('h2018.') and len(str(root)) < 30,'prepared-private-root')
    for label in ('before','after'):
        arm = record['arms'][label]
        need(arm['inventory']['checkout'] == str(checkouts[label]),'prepared-checkout')
        env = environment(parent,record['base'],label,arm['inventory'])
        verify_environment(arm,env)
        need(configuration(arm['inventory']['checkout'],env) == arm['configuration'],'prepared-runtime-configuration')
        check_arm_files(arm)
    return record,digest(raw)


def main():
    require_user()
    need(len(sys.argv) in (3,5),'Pass --admission-only CONTROL or --prepare/--verify CONTROL BEFORE AFTER')
    mode, control_arg = sys.argv[1:3]
    control = Path(control_arg)
    plan = json.loads((control / '.github/qualification/final-main-plan.json').read_bytes())
    # This is deliberately BEFORE resolve/stat/cache reads/mkdir/subprocess.
    admit_references(plan['native_preparation']['roles'],PINNED_ROLES)
    if mode == '--admission-only': return
    need(mode in ('--prepare','--verify') and len(sys.argv) == 5,'preparation-mode')
    control = control.resolve(strict=True)
    checkouts = dict(zip(('before','after'),[Path(value).resolve(strict=True) for value in sys.argv[3:]],strict=True))
    identity = {key:subprocess.check_output(['git','-C',str(control),'rev-parse',ref],text=True).strip()
                for key,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]}
    if mode == '--verify': load_prepared(control,plan,checkouts,dict(os.environ),identity); return
    values = admitted(plan,PINNED_ROLES,read_inventory,inspect_roles,os.environ['CARGO_TARGET_DIR'])
    for label in ('before','after'):
        need(values[label]['checkout'] == str(checkouts[label]),'producer-checkout')
        for key,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]:
            need(subprocess.check_output(['git','-C',str(checkouts[label]),'rev-parse',ref],text=True).strip() == plan['before' if label == 'before' else 'product'][key],'clean-source')
        need(not subprocess.check_output(['git','-C',str(checkouts[label]),'status','--porcelain=v1']),'clean-source')
    out = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'preparation'; out.mkdir(parents=True,exist_ok=True)
    base = Path(tempfile.mkdtemp(prefix='h2018.',dir='/tmp')).resolve(strict=True); base.chmod(0o700)
    need(len(str(base)) < 30,'short-private-root')
    envs = {label:environment(dict(os.environ),str(base),label,values[label]) for label in ('before','after')}
    for label in ('before','after'):
        root = base / label; root.mkdir(mode=0o700)
        for suffix in PRIVATE.values(): (root / suffix).mkdir(mode=0o700)
        if envs[label].get('CAGENTS_ZIG'):
            need(str(Path(envs[label]['CAGENTS_ZIG']).resolve(strict=True)) == values[label]['tools']['zig']['path'],'prepared-zig-override')
        need(re.search(r'zig\s*=\s*"0\.15\.2"',(checkouts[label] / 'mise.toml').read_text()),'prepared-zig-pin')
        for name in ('cargo','rustc','zig'):
            need(str(Path(shutil.which(name,path=envs[label]['PATH'])).resolve(strict=True)) == values[label]['tools'][name]['path'],'prepared-tool-selection')
        need(envs[label].get('RUSTUP_TOOLCHAIN') == '1.93.0' and envs[label].get('CARGO_BUILD_JOBS') == '2','prepared-toolchain')
    configurations = {label:configuration(values[label]['checkout'],envs[label]) for label in ('before','after')}
    records = []
    def run(label, graph, argv, cwd, env, timeout):
        stdout,stderr = out / (label + '-' + graph + '.jsonl'),out / (label + '-' + graph + '.stderr.log')
        command(argv,cwd,env,stdout,stderr,timeout)
        records.append(dict(arm=label,graph=graph,argv=argv,exit_code=0,stdout=file_binding(stdout),stderr=file_binding(stderr)))
        return [json.loads(line) for line in stdout.read_text().splitlines() if line.startswith('{')]
    arms = compile_matrix(values,envs,str(control / '.github/qualification/final-main-fixture.rs'),str(base),run)
    for label,arm in arms.items():
        paths = {name:artifact['executable'] for name,artifact in arm['triplet'].items()}
        paths.update({'library-' + name:path for name,path in arm['libraries'].items()}); paths['fixture'] = arm['helper']
        arm['files'] = {name:file_binding(path) for name,path in paths.items()}
        need(configuration(arm['inventory']['checkout'],envs[label]) == configurations[label],'prepared-configuration-changed')
        arm['configuration'] = configurations[label]
        for key,ref in [('sha','HEAD'),('tree','HEAD^{tree}')]:
            need(subprocess.check_output(['git','-C',str(checkouts[label]),'rev-parse',ref],text=True).strip() == plan['before' if label == 'before' else 'product'][key],'prepared-source-changed')
        need(not subprocess.check_output(['git','-C',str(checkouts[label]),'status','--porcelain=v1']),'prepared-source-changed')
    record = dict(kind='both-heads-prepared-v1',product=plan['product'],before=plan['before'],control=identity,
        module_sha256=digest(Path(__file__).read_bytes()),fixture_sha256=digest((control / '.github/qualification/final-main-fixture.rs').read_bytes()),
        roles=plan['native_preparation']['roles'],base=str(base),arms=arms,commands=records,
        limits='Preparation is not zero-rebuild evidence, native validation, complete writer settlement or external entry closure.')
    (out / 'prepared.json').write_bytes(json.dumps(record,indent=2).encode() + b'\n')
    load_prepared(control,plan,checkouts,dict(os.environ),identity)


if __name__ == '__main__':
    main()
