#!/usr/bin/env python3
"""One admitted lane-C HOST check, compilation and same-ELF exact-parent run.

This maintained successor follows the PR1239 R3 direct-ELF driver. A reviewed
plan and coordinator-supplied prior-return statement are inputs, not grants.
Execution needs a fresh external allocation and the actual existing C flock.
See docs/development.md; importing this file does not acquire or launch anything.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import shutil
import signal
import stat
import sys
import time
import tomllib


LANE_RELATIVE = Path('.worktrees/validation-three-lanes')
OWNER = 'validation-lane-c'
TOOL_ROLES = {'python', 'git', 'env', 'cargo', 'rustc', 'rustdoc', 'zig'}
IDENTITY_ROLES = {'workspace_root', 'control_repository', 'common_git_dir',
                  'lane_directory', 'lane_lock', 'lane_target'}
REQUIRED_PINS = {'Cargo.toml', 'Cargo.lock', '.cargo/config.toml', 'build.rs',
                 'src/lib.rs', 'src/bin/cagentsd.rs'}
FORBIDDEN_NAMES = {
    'CAGENTS_CHAT_STAGE_OUTPUT', 'CAGENTS_UI_LAUNCH_HANDSHAKE',
    'CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS', 'CAGENTS_UI_UPDATE_GUARD_FD',
    'CAGENTS_DAEMON_LAUNCH_FENCE_FD', 'CAGENTS_LATENCY_OUTPUT',
    'CAGENTS_INSTALL_RESULT_FD', 'CAGENTS_INSTALL_OWNER_CLI',
    'CAGENTS_ARCHIVE', 'CAGENTS_CHECKSUMS', 'CAGENTS_REPOSITORY',
    'CAGENTS_ROOTED_SAVE_TEST_CHILD', 'CAGENTS_ROOTED_SAVE_TEST_RECEIPT',
    'CAGENTS_RETAINED_CLOSE_RETRY_TEST', 'CAGENTS_NATIVE_HOLDER_STDERR_TEST',
    'CAGENTS_NATIVE_HOLDER_TRANSPORT_TEST', 'CAGENTS_NATIVE_HOLDER_FAILED_SPAWN_TEST',
    'FW16_TEST_1757_PASSIVE_PEER', 'XDG_ACTIVATION_TOKEN', 'DESKTOP_STARTUP_ID',
    'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_INDEX_FILE', 'GIT_CONFIG',
    'GIT_CONFIG_COUNT', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_SYSTEM',
    'GIT_CONFIG_GLOBAL', 'GIT_CONFIG_NOSYSTEM', 'GIT_ATTR_SOURCE',
    'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES',
    'GIT_CEILING_DIRECTORIES', 'GIT_DISCOVERY_ACROSS_FILESYSTEM',
    'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
}
FORBIDDEN_PREFIXES = ('CARGO_PROFILE_', 'GIT_CONFIG_KEY_', 'GIT_CONFIG_VALUE_', 'CAGENTS_DEVELOPMENT_')
RETURN_STATE = 'own family positively settled; fixed packet ended'
RETURN_LEASE = 'closed only after positive owned-family settlement and pidfd closure'
STOP_REQUESTED = False
STOP_SIGNALS = []


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def json_bytes(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            check(key not in result, 'Duplicate JSON field')
            result[key] = value
        return result
    return json.loads(raw.decode('utf-8', errors='strict'), object_pairs_hook=unique)


def absolute(value):
    check(isinstance(value, str), 'Expected an absolute path')
    path = Path(value)
    check(path.is_absolute() and str(path) == value and path == path.resolve(), 'Path contains a symlink or lexical alias')
    return path


def digest_string(value):
    return isinstance(value, str) and re.fullmatch(r'[0-9a-f]{64}', value) is not None


def commit_string(value):
    return isinstance(value, str) and re.fullmatch(r'[0-9a-f]{40}', value) is not None


def device_inode(value):
    return isinstance(value, list) and len(value) == 2 and all(type(part) is int and part >= 0 for part in value) and value[1] > 0


class HostPaths:
    """Reviewed absolute bindings for this one fixed lane-C workload."""
    def __init__(self, plan):
        self.workspace = absolute(plan['workspace_root'])
        self.repository = absolute(plan['control_repository'])
        self.common = absolute(plan['common_git_dir'])
        self.lanes = self.workspace / LANE_RELATIVE
        self.checkout = self.lanes / 'lane-c-checkout'
        self.lock = self.lanes / 'lane-c.lock'
        self.target = self.lanes / 'lane-c-target'
        check(self.repository.is_relative_to(self.workspace) and not self.repository.is_relative_to(self.lanes),
              'Control repository must belong to the explicit workspace outside its C lane directory')
        check(not self.common.is_relative_to(self.checkout) and not self.checkout.is_relative_to(self.common)
              and self.common != self.repository, 'Common Git anchor overlaps the canonical checkout/source root')
        self.identities = plan['identities']
        check(set(self.identities) == IDENTITY_ROLES and all(device_inode(value) for value in self.identities.values()),
              'All existing workspace/control/common/C directory and lock identities must be reviewed')
        self.tools = {}
        check(set(plan['tools']) == TOOL_ROLES, 'Exactly the seven physical HOST tool roles must be bound')
        for role, binding in plan['tools'].items():
            check(set(binding) == {'path', 'sha256', 'device_inode'} and digest_string(binding['sha256'])
                  and device_inode(binding['device_inode']), 'Physical tool requires absolute path, digest and existing identity')
            self.tools[role] = str(absolute(binding['path']))
        self.overlays = {'CARGO_TARGET_DIR': str(self.target), 'CARGO_BUILD_JOBS': '2',
                         'RUSTC': self.tools['rustc'], 'RUSTDOC': self.tools['rustdoc'], 'CAGENTS_ZIG': self.tools['zig']}

    def role_paths(self):
        return {'workspace_root': self.workspace, 'control_repository': self.repository,
                'common_git_dir': self.common, 'lane_directory': self.lanes,
                'lane_lock': self.lock, 'lane_target': self.target}


def context_guard():
    # Refuse by NAME only; never clear or select caller credentials/authority.
    check(not any(name in FORBIDDEN_NAMES or name.startswith(FORBIDDEN_PREFIXES)
                  for name in os.environ), 'Inherited startup/output/fixture/build routing conflicts with HOST admission')
    check(signal.getsignal(signal.SIGCHLD) == signal.SIG_DFL,
          'Inherited SIGCHLD prevents original-child reaping; it is not reset')


def stopped():
    return STOP_REQUESTED


def stop(signum, _frame):
    global STOP_REQUESTED
    STOP_REQUESTED = True
    if len(STOP_SIGNALS) < 64:
        STOP_SIGNALS.append({'signal': signum, 'utc': now()})


def private_write(path, data, exclusive=False):
    flags = os.O_WRONLY | os.O_CREAT | os.O_CLOEXEC | os.O_NOFOLLOW
    descriptor = os.open(path, flags | (os.O_EXCL if exclusive else os.O_TRUNC), 0o600)
    with os.fdopen(descriptor, 'wb') as stream:
        info = os.fstat(stream.fileno())
        check(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
              and info.st_nlink == 1 and stat.S_IMODE(info.st_mode) == 0o600,
              'Evidence file is not an owned private regular file')
        stream.write(data)
        stream.flush()


def prior_return_observation(value):
    """Validate the pinned R3-compatible coordinator statement, not a capability.

    Failed tests may have a positive family return. Preserve their outcomes and
    signal dispositions; no cleanup exit becomes a successful original result.
    """
    check(isinstance(value, dict) and value.get('state') == RETURN_STATE
          and value.get('lock_acquired') is True and value.get('lease') == RETURN_LEASE
          and value.get('all_owned_pidfds_closed') is True
          and type(value.get('exit_code')) is int and bool(value.get('finished_utc')),
          'Prior statement does not report positive C return and pidfd closure')
    family = value.get('custody_final')
    check(isinstance(family, dict) and family.get('subreaper') is True
          and family.get('initial_children_empty') is True and bool(family.get('boot_id'))
          and isinstance(family.get('wrapper'), dict)
          and type(family.get('launch_count')) is int and family['launch_count'] >= 0
          and isinstance(family.get('nodes'), list), 'Prior custody statement is incomplete')
    wrapper = family['wrapper']
    check(all(type(wrapper.get(key)) is int for key in ('pid', 'start_ticks', 'uid')),
          'Prior wrapper identity is missing')
    identities = set()
    for node in family['nodes']:
        check(isinstance(node, dict) and node.get('settled') is True
              and all(type(node.get(key)) is int for key in ('pid', 'start_ticks', 'uid'))
              and bool(node.get('exit_status_source')) and isinstance(node.get('signals'), list),
              'Prior owned node has unknown settlement')
        identity = (node['pid'], node['start_ticks'])
        check(identity not in identities, 'Duplicate prior owned-node identity')
        identities.add(identity)
    return {'coordinator_statement_only': True, 'state': value['state'],
            'original_exit_code': value['exit_code'], 'custody_final': family,
            'finished_utc': value['finished_utc'], 'lease': value['lease']}


def prepared_observation(raw, original_exit, state, source, paths):
    check(state == 'finished' and original_exit == 0, 'Checkout helper refused, failed or has unknown original result')
    value = json_bytes(raw)
    previous = None if source['expect_current'] == 'absent' else source['expect_current']
    operation = 'created' if previous is None else 'reused' if previous == source['head'] else 'updated'
    expected = {'version': 1, 'status': 'prepared', 'operation': operation,
                'repository': str(paths.repository), 'common_git_dir': str(paths.common),
                'checkout': str(paths.checkout), 'owner': OWNER, 'head': source['head'],
                'tree': source['tree'], 'previous_head': previous,
                'registry': str(paths.common / 'validation-checkouts-v1' /
                                (hashlib.sha256(os.fsencode(paths.checkout)).hexdigest() + '.json')),
                'detached': True, 'clean': True, 'source_only': True,
                'external_requirements': ['exclusive_build_lane_ownership', 'prior_owned_family_settled']}
    check(json.dumps(value, sort_keys=True) == json.dumps(expected, sort_keys=True),
          'Prepared checkout receipt differs from the exact admitted slot/source/previous head')
    return value


def context_snapshot(paths):
    candidates = {base / '.cargo' / name for base in (paths.checkout, *paths.checkout.parents)
                  for name in ('config', 'config.toml')}
    cargo_home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')))
    candidates.update(cargo_home / name for name in ('config', 'config.toml'))
    configs = {}
    for path in sorted(candidates):
        try:
            info = path.lstat()
        except FileNotFoundError:
            configs[str(path)] = {'present': False}
            continue
        check(path.is_file(), 'Cargo configuration is not an ordinary readable file')
        configs[str(path)] = {'present': True, 'mode': info.st_mode, 'sha256': sha(path),
                             'symlink_target': os.readlink(path) if stat.S_ISLNK(info.st_mode) else None}
    keys = {'PATH', 'HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR',
            'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'RUSTUP_AUTO_INSTALL', 'RUSTC', 'RUSTDOC',
            'RUSTDOCFLAGS', 'CARGO_ENCODED_RUSTDOCFLAGS', 'CC', 'CXX', 'AR', 'LD', 'LD_LIBRARY_PATH',
            'PKG_CONFIG_PATH', 'PKG_CONFIG_LIBDIR', 'PKG_CONFIG_SYSROOT_DIR'}
    hashed = {name: hashlib.sha256(value.encode()).hexdigest() for name, value in os.environ.items()
              if name in keys or name.startswith(('CARGO_BUILD_', 'CARGO_TARGET_', 'CARGO_PROFILE_', 'CC_', 'CXX_', 'AR_'))}
    tools = {}
    for name, spelling in paths.tools.items():
        physical = Path(spelling).resolve(strict=True)
        check(str(physical) == spelling, 'Tool binding must name the physical file without aliases')
        before = physical.stat()
        check(stat.S_ISREG(before.st_mode) and before.st_mode & 0o111, 'Physical tool is not an ordinary executable')
        digest = sha(physical)
        after = physical.stat()
        identity = lambda value: [value.st_dev, value.st_ino, value.st_mode, value.st_size, value.st_mtime_ns, value.st_ctime_ns]
        check(identity(before) == identity(after), 'Physical tool changed while hashing')
        tools[name] = {'spelling': spelling, 'physical': str(physical), 'sha256': digest,
                       'device_inode': [after.st_dev, after.st_ino], 'identity': identity(after)}
    found_git = shutil.which('git')
    check(found_git is not None and Path(found_git).resolve() == Path(paths.tools['git']),
          'Checkout helper PATH does not select the pinned physical Git; PATH is not rewritten')
    return {'configs': configs, 'inherited_build_value_hashes': hashed, 'physical_tools': tools,
            'inherited_cagents_names': sorted(name for name in os.environ if name.startswith('CAGENTS_')),
            'intended_Cargo_overlays': paths.overlays,
            'direct_ELF_environment': 'original environment; authority/HOME/XDG preserved'}


def select_host_artifact(raw, original_exit, paths):
    check(original_exit == 0, 'Original Cargo compilation was unsuccessful')
    messages, diagnostics = [], []
    for number, line in enumerate(raw.decode('utf-8', errors='strict').splitlines(), 1):
        if not line.strip():
            continue
        if not line.lstrip().startswith('{'):
            diagnostics.append({'line': number, 'text': line})
            continue
        message = json_bytes(line.encode())
        check(isinstance(message, dict) and isinstance(message.get('reason'), str), 'Invalid Cargo JSON message')
        messages.append(message)
    builds = [message for message in messages if message['reason'] == 'build-finished']
    check(len(builds) == 1 and builds[0].get('success') is True, 'Unique successful build-finished record missing')
    artifacts = [message for message in messages if message['reason'] == 'compiler-artifact']
    forbidden = {'cosmic', 'libcosmic', 'servo', 'webrender', 'mozjs', 'mozjs_sys', 'spidermonkey'}
    check(not any(message.get('target', {}).get('name') in forbidden for message in artifacts),
          'HOST compilation emitted a forbidden UI/browser artifact')
    roots = [message for message in artifacts if message.get('manifest_path') == str(paths.checkout / 'Cargo.toml')
             and message.get('target', {}).get('name') == 'cagents'
             and message.get('target', {}).get('kind') == ['lib']
             and message.get('target', {}).get('src_path') == str(paths.checkout / 'src/lib.rs')
             and message.get('profile', {}).get('test') is True]
    check(len(roots) == 1, 'Fresh compilation did not report exactly one canonical root lib test artifact')
    artifact = roots[0]
    features = artifact.get('features')
    check(isinstance(features, list) and 'host' in features
          and not set(features).intersection({'ui', 'integrated-browser', 'full'}), 'Artifact violates isolated HOST features')
    check(isinstance(artifact.get('executable'), str) and artifact['executable'], 'Root test artifact lacks an executable')
    return {'compiler_artifact': artifact, 'build_finished': builds[0],
            'all_compiler_artifact_count': len(artifacts), 'non_JSON_combined_diagnostics': diagnostics,
            'stream': 'COMBINED stdout+stderr'}


def observe_elf(spelling, paths, expected=None):
    path = Path(spelling)
    check(path.is_absolute() and path.parent == paths.target / 'debug/deps'
          and re.fullmatch(r'cagents-[0-9a-f]+', path.name), 'Cargo-reported ELF is outside the existing C target')
    check(path.resolve(strict=True) == path and paths.target.resolve(strict=True) == paths.target, 'ELF/target contains an alias')
    named = path.lstat()
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        check(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
              and before.st_nlink == 1 and before.st_mode & 0o111, 'Reported ELF is not an owned ordinary executable')
        magic = os.read(descriptor, 4)
        check(magic == b'\x7fELF', 'Reported executable lacks ELF magic')
        digest = hashlib.sha256(magic)
        while chunk := os.read(descriptor, 1024 * 1024):
            digest.update(chunk)
        after = os.fstat(descriptor)
        identity = lambda info: [info.st_dev, info.st_ino, info.st_uid, info.st_mode, info.st_nlink,
                                 info.st_size, info.st_mtime_ns, info.st_ctime_ns]
        check(identity(named) == identity(before) == identity(after) == identity(path.lstat()), 'ELF changed during binding')
        observation = {'path': str(path), 'sha256': digest.hexdigest(), 'device_inode': [after.st_dev, after.st_ino],
                       'file_identity': identity(after), 'ELF_magic': True}
    finally:
        os.close(descriptor)
    check(expected is None or observation == expected, 'Pinned ELF changed across phases')
    return observation


def inventory_observation(raw, original_exit, selectors):
    check(original_exit == 0, 'Original direct ELF inventory failed')
    lines = [line for line in raw.decode('utf-8', errors='strict').splitlines() if line]
    totals = re.fullmatch(r'(\d+) tests?, (\d+) benchmarks?', lines[-1]) if lines else None
    check(totals is not None, 'Complete original inventory totals missing')
    rows = [re.fullmatch(r'([^\s]+): (test|benchmark)', line) for line in lines[:-1]]
    check(all(row is not None for row in rows), 'Unparsed inventory line')
    names = [row[1] for row in rows]
    check(len(names) == len(set(names)), 'Duplicate complete inventory name')
    tests = [row[1] for row in rows if row[2] == 'test']
    benchmarks = [row[1] for row in rows if row[2] == 'benchmark']
    check([len(tests), len(benchmarks)] == [int(totals[1]), int(totals[2])], 'Inventory totals differ from rows')
    check(all(tests.count(name) == 1 for name in selectors), 'Same ELF lacks a selected exact parent')
    return {'test_names': tests, 'benchmark_names': benchmarks, 'test_count': len(tests),
            'benchmark_count': len(benchmarks), 'selected_in_plan_order': selectors, 'parent_passes': 0}


def test_observation(raw, original_exit, selector, inventory_count):
    text = raw.decode('utf-8', errors='strict')
    summary_pattern = (r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; '
                       r'(\d+) measured; (\d+) filtered out; finished in (\d+(?:\.\d+)?s)$')
    summaries = re.findall(summary_pattern, text, re.M)
    rows = [line for line in re.findall(r'^test .+$', text, re.M) if re.fullmatch(summary_pattern, line) is None]
    running = re.findall(r'^running .+$', text, re.M)
    valid = (original_exit == 0 and running == ['running 1 test'] and rows == ['test ' + selector + ' ... ok']
             and len(summaries) == 1 and inventory_count >= 1
             and summaries[0][:6] == ('ok', '1', '0', '0', '0', str(inventory_count - 1))
             and '\x1b' not in text and 'error: test failed' not in text)
    return {'qualification': valid, 'selected_parent': selector, 'original_exit': original_exit,
            'all_running_lines': running, 'all_terminal_rows': rows, 'all_summaries': summaries,
            'inventory_test_count': inventory_count, 'outer_parent_contract_count': 1,
            'nested_selfchild_is_nonadditive': True, 'independent_raw_audit_required': True}


def phases(selectors, paths):
    prefix = [paths.tools['env'], *[name + '=' + value for name, value in paths.overlays.items()], paths.tools['cargo']]
    return [
        {'label': 'host-binary-check', 'kind': 'check', 'budget': 600,
         'argv': prefix + ['check', '--locked', '--no-default-features', '--features', 'host', '--bin', 'cagentsd']},
        {'label': 'host-lib-test-compilation', 'kind': 'compile', 'budget': 1200,
         'argv': prefix + ['test', '--locked', '--no-default-features', '--features', 'host', '--lib', '--no-run', '--message-format=json']},
        {'label': 'same-ELF-complete-inventory', 'kind': 'inventory', 'budget': 30,
         'argv': ['<CARGO_REPORTED_HOST_ELF>', '--list', '--color=never']},
        *[{'label': f'parent-{index:02d}', 'kind': 'parent', 'budget': 120, 'selector': selector,
           'argv': ['<CARGO_REPORTED_HOST_ELF>', selector, '--exact', '--nocapture', '--test-threads=1', '--color=never']}
          for index, selector in enumerate(selectors, 1)],
    ]


def validate_plan(plan):
    check(set(plan) == {'version', 'workspace_root', 'control_repository', 'common_git_dir', 'identities',
                        'tools', 'source', 'artifacts', 'selectors', 'evidence_directory', 'total_workload_seconds'}
          and type(plan['version']) is int and plan['version'] == 1, 'Unsupported HOST plan fields/version')
    paths = HostPaths(plan)
    source = plan['source']
    check(set(source) == {'original_checkout', 'head', 'tree', 'parent', 'expect_current', 'sha256'}, 'Invalid source plan fields')
    original = absolute(source['original_checkout'])
    check(original != paths.checkout and not original.is_relative_to(paths.checkout), 'Original witness must be separate from the staged slot')
    check(all(commit_string(source[key]) for key in ('head', 'tree', 'parent'))
          and (source['expect_current'] == 'absent' or commit_string(source['expect_current'])), 'Full exact commit/tree/previous-head pins required')
    pins = source['sha256']
    check(isinstance(pins, dict) and REQUIRED_PINS <= pins.keys(), 'Required source-fidelity pins missing')
    for name, digest in pins.items():
        relative = Path(name)
        check(name and not relative.is_absolute() and str(relative) == name and '..' not in relative.parts
              and '.git' not in relative.parts and digest_string(digest), 'Invalid relative source pin')
    check(set(plan['artifacts']) == {'checkout_helper', 'custody'}, 'Exactly the reviewed checkout helper and custody artifact required')
    for artifact in plan['artifacts'].values():
        check(set(artifact) == {'path', 'sha256'} and digest_string(artifact['sha256']), 'Invalid artifact pin')
        check(not absolute(artifact['path']).is_relative_to(paths.checkout), 'Executing helper/custody must be outside the mutable slot')
    selectors = plan['selectors']
    check(isinstance(selectors, list) and 1 <= len(selectors) <= 32
          and all(isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)+', name) for name in selectors)
          and len(selectors) == len(set(selectors)), 'One to32 distinct ordered exact HOST parent selectors required')
    # R3 budgets retained; staging gets60s in addition to the original90s overhead.
    check(type(plan['total_workload_seconds']) is int and plan['total_workload_seconds'] == 1980 + 120 * len(selectors),
          'Whole budget must reserve all fixed phases, helper60s and overhead90s')
    evidence = absolute(plan['evidence_directory'])
    check(not any(evidence == path or evidence.is_relative_to(path) for path in (paths.checkout, original, paths.common, paths.target)),
          'Evidence must remain outside source, Git administration and the existing target')
    return paths


class Runner:
    def __init__(self, arguments):
        context_guard()
        self.plan_path, self.prior_path = absolute(arguments.plan), absolute(arguments.prior_return)
        self.driver_path = absolute(str(Path(__file__).absolute()))
        self.bindings = {str(self.plan_path): arguments.plan_sha256, str(self.driver_path): arguments.driver_sha256,
                         str(self.prior_path): arguments.prior_return_sha256}
        check(all(digest_string(value) for value in self.bindings.values()), 'Full reviewed artifact digests required')
        self.check_bindings()
        self.plan = json_bytes(self.plan_path.read_bytes())
        self.paths = validate_plan(self.plan)
        check(not any(path.is_relative_to(self.paths.checkout) for path in (self.plan_path, self.prior_path, self.driver_path)),
              'Driver, plan and handback evidence must be outside the mutable slot')
        check(re.fullmatch(r'[A-Za-z0-9_.:/-]{1,256}', arguments.allocation) is not None
              and 'PENDING' not in arguments.allocation.upper(), 'A fresh external allocation reference is required')
        self.prior_return = prior_return_observation(json_bytes(self.prior_path.read_bytes()))
        self.bindings.update({artifact['path']: artifact['sha256'] for artifact in self.plan['artifacts'].values()})
        self.check_bindings()
        self.helper = runpy.run_path(self.plan['artifacts']['custody']['path'])
        self.work = Path(self.plan['evidence_directory'])
        self.work.mkdir(mode=0o700)  # Exclusive new execution evidence; never creates the canonical slot.
        check(stat.S_IMODE(self.work.stat().st_mode) == 0o700, 'Evidence directory is not private')
        self.receipt = self.work / 'receipt.json'
        self.owned, self.lease, self.lock_acquired = None, None, False
        self.total_deadline, self.initial_context, self.serial, self.outer_exit = None, None, 0, 1
        self.target_identity = None
        self.entries = phases(self.plan['selectors'], self.paths)
        self.value = {'kind': 'maintained isolated HOST lane-C direct-ELF receipt', 'version': 1,
                      'started_utc': now(), 'allocation_reference_only': arguments.allocation,
                      'prior_return': {'path': str(self.prior_path), 'sha256': arguments.prior_return_sha256,
                                       'observation': self.prior_return},
                      'head': self.plan['source']['head'], 'tree': self.plan['source']['tree'],
                      'workspace_root': str(self.paths.workspace), 'control_repository': str(self.paths.repository),
                      'canonical_checkout': str(self.paths.checkout), 'common_git_anchor': str(self.paths.common),
                      'existing_target': str(self.paths.target), 'lane_lock': str(self.paths.lock), 'jobs': 2,
                      'reviewed_existing_identities': self.paths.identities,
                      'artifact_bindings': self.bindings, 'commands': [], 'metadata_commands': [],
                      'phase_states': [{'label': entry['label'], 'state': 'UNRUN'} for entry in self.entries],
                      'errors': [], 'stop_signals': STOP_SIGNALS, 'lock_acquired': False, 'qualification': False,
                      'independent_raw_audit_required': True, 'capture_stream': 'COMBINED original stdout+stderr',
                      'limits': 'Handled exits only; SIGKILL/power loss cannot run cleanup. No provider/native/cache-performance claim. Capture has no byte cap.'}
        self.save()

    def check_bindings(self):
        check(all(sha(path) == digest for path, digest in self.bindings.items()), 'Pinned driver/plan/helper/custody/prior-return bytes changed')

    def save(self):
        private_write(self.receipt, (json.dumps(self.value, indent=2) + '\n').encode())

    def report_error(self, label, error):
        if len(self.value['errors']) < 256:
            self.value['errors'].append({'label': label, 'type': type(error).__name__, 'message': str(error)[:2048], 'utc': now()})
        try:
            self.save()
        except BaseException:
            self.value['receipt_write_failed'] = True

    def family_settled(self):
        self.owned.scan()
        check(not self.helper['child_ids'](self.owned.pid), 'Owned descendants remain; next launch forbidden')
        check(all(self.owned.reap(node) for node in self.owned.nodes.values()), 'Owned original process lacks positive settlement')
        snapshot = self.owned.snapshot()
        check(not self.helper['child_ids'](self.owned.pid)
              and all(node.get('settled') for node in self.owned.nodes.values()), 'Owned settlement changed during snapshot')
        return snapshot

    def budget(self, seconds, fixed):
        check(not stopped(), 'Stop closes command admission')
        remaining = self.total_deadline - time.monotonic()
        check(remaining >= seconds + 90 if fixed else remaining > 0, 'Full phase budget plus90s overhead cannot fit; no truncation/retry')
        return seconds if fixed else min(seconds, remaining)

    def captured(self, argv, label, seconds, cwd=None, metadata=False, fixed=False):
        check(self.lock_acquired and self.prior_return is not None, 'No command before actual C and prior-positive-return admission')
        cwd = self.paths.checkout if cwd is None else cwd
        self.layout_observation(label + '-admission')
        self.family_settled()
        context_guard()
        self.check_bindings()
        timeout = self.budget(seconds, fixed)
        self.serial += 1
        path = self.work / f'{self.serial:03d}-{label}.log'
        record = {'label': label, 'argv': argv, 'cwd': str(cwd), 'started_utc': now(), 'log': str(path),
                  'timeout_seconds': timeout, 'fixed_budget_reserved': fixed,
                  'state': 'admitted; not yet launched', 'original_exit': None, 'cleanup_exit': None}
        self.value['metadata_commands' if metadata else 'commands'].append(record)
        self.save()
        def incomplete(result):
            partial = Path(str(path) + '.original-partial')
            private_write(partial, result.stdout, exclusive=True)
            record['incomplete_before_cleanup'] = {'path': str(partial), 'sha256': sha(partial),
                                                  'bytes': len(result.stdout), 'state': result.state, 'original_exit': None}
            self.save()
        self.budget(seconds, fixed)
        result = self.owned.capture(argv, str(cwd), label, timeout, on_incomplete=incomplete)
        private_write(path, result.stdout, exclusive=True)
        record.update(state=result.state, original_exit=result.original_exit, cleanup_exit=result.cleanup_exit,
                      finished_utc=now(), log_sha256=sha(path), stdout_bytes=len(result.stdout),
                      stdout_complete=result.state == 'finished', retention_error=getattr(result, 'retention_error', None))
        self.save()  # Original output/result retained BEFORE any descendant-settlement refusal.
        record['family_after_original_result'] = self.family_settled()
        record['unforced_own_family'] = not any(node.get('signals') for node in record['family_after_original_result']['nodes'])
        self.save()
        return result, record

    def source_observation(self, stage, original=False):
        source = self.plan['source']
        cwd = Path(source['original_checkout']) if original else self.paths.checkout
        observation = {'stage': stage, 'checkout': str(cwd), 'original_source_witness': original, 'source_hashes': {}}
        self.value.setdefault('source_observations', []).append(observation)
        for name, expected in source['sha256'].items():
            actual = sha(cwd / name)
            observation['source_hashes'][name] = actual
            check(actual == expected, 'Reviewed source bytes changed')
        config = tomllib.loads((cwd / '.cargo/config.toml').read_text())
        check(not (cwd / '.cargo/config').exists() and config.get('build', {}).get('jobs') == 2
              and config.get('env', {}).get('CAGENTS_CHECKOUT_ROOT') == {'value': '.', 'relative': True, 'force': True},
              'Checkout-fidelity anchor or existing jobs2 changed')
        build = (cwd / 'build.rs').read_text()
        check('const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");' in build
              and 'println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");' in build,
              'Compile-time/runtime checkout-fidelity anchor missing')
        suffixes = [('rev-parse', 'HEAD'), ('rev-parse', 'HEAD^{tree}'), ('rev-parse', 'HEAD^'),
                    ('status', '--porcelain=v1', '--untracked-files=all', '--ignored=matching'),
                    ('rev-parse', '--show-toplevel'), ('rev-parse', '--path-format=absolute', '--git-common-dir')]
        if not original:
            suffixes.append(('rev-parse', '--abbrev-ref', 'HEAD'))
        values = []
        for suffix in suffixes:
            argv = [self.paths.tools['git'], '--no-pager', '--no-optional-locks', '--no-replace-objects',
                    '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false', '-C', str(cwd), *suffix]
            result, _ = self.captured(argv, stage + '-git-' + str(len(values)), 10, cwd=cwd, metadata=True)
            check(result.state == 'finished' and result.original_exit == 0, 'Original source metadata failed/incomplete')
            values.append(result.stdout.decode('utf-8', errors='strict').strip())
        check(values == [source['head'], source['tree'], source['parent'], '', str(cwd), str(self.paths.common)]
              + ([] if original else ['HEAD']), 'Exact clean source/head/tree/parent/common-Git/detachment mismatch')
        observation.update(values=values, exact_source=True)
        self.save()

    def context_observation(self, stage):
        context_guard()
        current = context_snapshot(self.paths)
        self.verify_tools(current)
        check(current['physical_tools'] == self.preparation_tools, 'Physical tools changed since helper admission')
        if self.initial_context is None:
            self.initial_context = current
        matches = current == self.initial_context
        self.value.setdefault('context_observations', []).append({'stage': stage, 'utc': now(), 'current': current, 'matches_initial': matches})
        self.save()
        check(matches, 'Cargo ancestor/config/tool/inherited build context changed')

    def acquire_lane(self):
        self.directory_observation('workspace_root', self.paths.workspace)
        self.directory_observation('lane_directory', self.paths.lanes)
        check(self.paths.lock.resolve(strict=True) == self.paths.lock, 'Existing C lock route contains an alias')
        self.lease = os.open(self.paths.lock, os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW)
        info = os.fstat(self.lease)
        check(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1
              and not info.st_mode & 0o022, 'Existing C lock is not an owned protected regular file')
        named = self.paths.lock.lstat()
        check([info.st_dev, info.st_ino] == self.paths.identities['lane_lock']
              and (named.st_dev, named.st_ino) == (info.st_dev, info.st_ino), 'Existing C lock identity changed')
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        named, held = self.paths.lock.lstat(), os.fstat(self.lease)
        check((named.st_dev, named.st_ino) == (held.st_dev, held.st_ino) == (info.st_dev, info.st_ino)
              and held.st_nlink == 1, 'C lock was replaced while acquiring its flock')
        self.lock_acquired = True
        self.value.update(lock_acquired=True, lease='fixed existing C fd held', lock_identity=[info.st_dev, info.st_ino])

    def directory_observation(self, role, path):
        check(path.resolve(strict=True) == path, 'Reviewed directory route is absent or aliased')
        info = path.lstat()
        check(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o022,
              'Reviewed directory is not owned and protected')
        identity = [info.st_dev, info.st_ino]
        check(identity == self.paths.identities[role], 'Reviewed ' + role + ' directory identity changed')
        # Within the workspace, also protect each intermediate directory rather
        # than accepting a writable/aliased .worktrees or control parent route.
        if path.is_relative_to(self.paths.workspace):
            parent = path.parent
            while parent.is_relative_to(self.paths.workspace):
                value = parent.lstat()
                check(stat.S_ISDIR(value.st_mode) and value.st_uid == os.getuid() and not value.st_mode & 0o022,
                      'Workspace ancestor directory is not owned and protected')
                if parent == self.paths.workspace:
                    break
                parent = parent.parent
        return {'role': role, 'path': str(path), 'device_inode': identity}

    def layout_observation(self, stage):
        observations = [self.directory_observation(role, path) for role, path in self.paths.role_paths().items()
                        if role != 'lane_lock']
        check(self.lease is not None, 'C admission has no held descriptor')
        named, held = self.paths.lock.lstat(), os.fstat(self.lease)
        check([named.st_dev, named.st_ino] == [held.st_dev, held.st_ino] == self.paths.identities['lane_lock']
              and held.st_nlink == 1, 'Held C no longer matches the reviewed existing lock')
        self.value.setdefault('layout_observations', []).append({'stage': stage, 'directories': observations})
        self.save()

    def control_observation(self):
        values = []
        for suffix in [('rev-parse', '--show-toplevel'), ('rev-parse', '--path-format=absolute', '--git-common-dir')]:
            argv = [self.paths.tools['git'], '--no-pager', '--no-optional-locks', '--no-replace-objects',
                    '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false', '-C', str(self.paths.repository), *suffix]
            result, _ = self.captured(argv, 'control-git-' + str(len(values)), 10, cwd=self.paths.repository, metadata=True)
            check(result.state == 'finished' and result.original_exit == 0, 'Control repository provenance is incomplete')
            values.append(result.stdout.decode('utf-8', errors='strict').strip())
        check(values == [str(self.paths.repository), str(self.paths.common)], 'Control repository reports a different common Git binding')
        self.value['control_provenance'] = {'repository': values[0], 'common_git_dir': values[1]}
        self.save()

    def verify_tools(self, snapshot):
        observed = snapshot['physical_tools']
        check(set(observed) == TOOL_ROLES and all(
            observed[role]['sha256'] == binding['sha256'] and observed[role]['device_inode'] == binding['device_inode']
            and observed[role]['physical'] == binding['path'] for role, binding in self.plan['tools'].items()),
            'Physical HOST tool differs from reviewed path/bytes/identity')

    def target_observation(self, stage):
        observation = self.directory_observation('lane_target', self.paths.target)
        identity = observation['device_inode']
        if self.target_identity is None:
            self.target_identity = identity
        check(identity == self.target_identity, 'Existing C target directory identity changed')
        self.value.setdefault('target_observations', []).append({'stage': stage, **observation})
        self.save()

    def prepare_checkout(self):
        check(self.lock_acquired and self.prior_return is not None, 'Checkout staging lacks actual C/prior positive return')
        check(self.value.get('control_provenance') == {'repository': str(self.paths.repository), 'common_git_dir': str(self.paths.common)},
              'Checkout staging lacks exact control/common Git provenance')
        source = self.plan['source']
        argv = [self.paths.tools['python'], '-I', '-B', self.plan['artifacts']['checkout_helper']['path'],
                '--repository', str(self.paths.repository), '--checkout', str(self.paths.checkout), '--owner', OWNER,
                '--head', source['head'], '--tree', source['tree'], '--expect-current', source['expect_current']]
        result, record = self.captured(argv, 'prepare-canonical-checkout', 60, cwd=self.paths.repository, metadata=True, fixed=True)
        # captured() has already required positive helper/Git-descendant settlement.
        value = prepared_observation(result.stdout, result.original_exit, result.state, source, self.paths)
        check(record['unforced_own_family'], 'Forced helper cleanup cannot admit Cargo')
        self.value['prepared_checkout'] = value
        self.save()

    def run(self):
        check(not stopped(), 'Stop before custody admission')
        self.check_bindings()
        self.prior_return = prior_return_observation(json_bytes(self.prior_path.read_bytes()))
        self.owned = self.helper['Custody'](stopped)
        self.acquire_lane()
        self.total_deadline = time.monotonic() + self.plan['total_workload_seconds']
        self.target_observation('before-staging')
        # Check physical tools/PATH before the helper or any metadata child. The
        # canonical Cargo config may be created by staging, so its baseline is
        # deliberately established after the exact prepared/source receipt.
        preparation = context_snapshot(self.paths)
        self.preparation_tools = preparation['physical_tools']
        self.verify_tools(preparation)
        self.value['context_before_staging'] = preparation
        self.save()
        print(str(self.receipt), flush=True)
        self.control_observation()
        self.source_observation('original-before-staging', original=True)
        self.prepare_checkout()
        self.run_workload()

    def run_workload(self):
        elf = inventory = None
        for index, entry in enumerate(self.entries):
            stage = f'phase-{index + 1}'
            self.source_observation(stage + '-before')
            self.context_observation(stage + '-before')
            self.target_observation(stage + '-before')
            argv = list(entry['argv'])
            before_elf = None
            if entry['kind'] in ('inventory', 'parent'):
                check(elf is not None, 'Direct phase lacks just-completed Cargo artifact')
                before_elf = observe_elf(elf['path'], self.paths, elf)
                argv[0] = elf['path']
            state = self.value['phase_states'][index]
            state.update(state='ADMITTED', admitted_utc=now())
            result, record = self.captured(argv, entry['label'], entry['budget'], fixed=True)
            state.update(state='ATTEMPTED', original_exit=result.original_exit, original_state=result.state, log=record['log'])
            if before_elf is not None:
                record.update(ELF_before=before_elf, ELF_after=observe_elf(elf['path'], self.paths, elf))
                nodes = [node for node in record['family_after_original_result']['nodes']
                         if node.get('role') == entry['label'] and node.get('provenance') == 'direct Popen child'
                         and node.get('executable') == elf['path'] and node.get('executable_identity') == elf['device_inode']]
                record['actual_ELF_PID_binding'] = {'retained_original_nodes': nodes,
                    'status': 'AVAILABLE' if len(nodes) == 1 else 'MISSING; no PID qualification',
                    'limit': 'Fast exit may preclude live executable metadata; no lookup/retarget/inference.'}
            self.source_observation(stage + '-after')
            self.context_observation(stage + '-after')
            self.target_observation(stage + '-after')
            record['source_context_unchanged'] = True
            if entry['kind'] == 'parent':
                record['test_observation'] = test_observation(result.stdout, result.original_exit, entry['selector'], inventory['test_count'])
            if result.state != 'finished' or result.original_exit != 0:
                code = result.original_exit
                self.outer_exit = code if code is not None and code > 0 else 128 - code if code is not None and code < 0 else 1
                record['qualification'] = False
                self.save()
                return
            check(record['unforced_own_family'], 'Forced family cleanup cannot qualify original phase')
            if entry['kind'] == 'check':
                text = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', result.stdout.decode('utf-8', errors='strict'))
                check(re.search(r'Finished `dev` profile', text), 'HOST check lacks original Finished dev marker')
            elif entry['kind'] == 'compile':
                record['Cargo_artifact_observation'] = select_host_artifact(result.stdout, result.original_exit, self.paths)
                elf = observe_elf(record['Cargo_artifact_observation']['compiler_artifact']['executable'], self.paths)
                self.value['owning_ELF'] = elf
            elif entry['kind'] == 'inventory':
                inventory = inventory_observation(result.stdout, result.original_exit, self.plan['selectors'])
                self.value['same_ELF_complete_inventory'] = inventory
            else:
                check(record['test_observation']['qualification'], 'Strict original one-parent raw oracle failed')
            record['qualification'] = 'raw success candidate only; independent audit required; inventory/compile add zero parents'
            state.update(state='RAW_SUCCESS_CANDIDATE', finished_utc=now())
            self.save()
        self.source_observation('original-final', original=True)
        self.source_observation('canonical-final')
        self.context_observation('workload-final')
        self.target_observation('workload-final')
        observe_elf(elf['path'], self.paths, elf)
        self.value['family_at_workload_end'] = self.family_settled()
        check(not any(node.get('signals') for node in self.value['family_at_workload_end']['nodes']), 'Forced cleanup prevents whole raw success')
        self.outer_exit = 0
        self.value['qualification'] = 'raw success candidates only; independent original result/context/custody audit required'

    def finish(self):
        global STOP_REQUESTED
        STOP_REQUESTED = True
        self.value.update(state='admission closed; mandatory positive own-family settlement; C retained', workload_finished_utc=now())
        try:
            self.save()
        except BaseException as error:
            self.report_error('settlement-initial-save', error)
        if self.owned is not None:
            while True:
                try:
                    self.value['custody_final'] = self.owned.settle_all(lambda: None, self.report_error)
                    self.owned.close_pidfds()
                    self.value['all_owned_pidfds_closed'] = True
                    break
                except BaseException as error:
                    self.report_error('settlement-blocked-C-retained', error)
                    time.sleep(0.1)
        if self.lease is not None:
            check(self.value.get('all_owned_pidfds_closed'), 'Unknown own-family settlement forbids C fd close')
            descriptor, self.lease = self.lease, None
            os.close(descriptor)
            self.value['lease'] = RETURN_LEASE
        if STOP_SIGNALS:
            self.outer_exit = self.outer_exit or 128 + STOP_SIGNALS[0]['signal']
        try:
            self.check_bindings()
        except BaseException as error:
            self.outer_exit = self.outer_exit or 1
            self.value['qualification'] = False
            self.report_error('artifact-changed-after-execution', error)
        self.value.update(finished_utc=now(), exit_code=self.outer_exit, state=RETURN_STATE)
        self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('plan', 'plan-sha256', 'driver-sha256', 'allocation', 'prior-return', 'prior-return-sha256'):
        parser.add_argument('--' + name, required=True)
    arguments = parser.parse_args()
    context_guard()
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    runner = None
    try:
        runner = Runner(arguments)
        runner.run()
    except BaseException as error:
        if runner is None:
            print(json.dumps({'startup_refused': type(error).__name__, 'message': str(error)}), file=sys.stderr)
        else:
            runner.outer_exit = runner.outer_exit or 1
            runner.report_error('original-wrapper-failure', error)
    finally:
        if runner is not None:
            runner.finish()  # Cannot abandon positive family settlement or release C early.
    return 1 if runner is None else runner.outer_exit


if __name__ == '__main__':
    raise SystemExit(main())
