"""PR1310 per-arm matched-build producer SOURCE; no supervisor or admission.

The safe original-command binder is deliberately absent. Standalone invocation
and produce_arm refuse before effects until a separately reviewed source binding
exists. Expected pins, UUIDs and callback fields never authenticate User/C.
"""
if not __debug__:
    raise RuntimeError('Nonoptimized source execution is required')

import hashlib
import inspect
import json
import os
from pathlib import Path
import re
import stat
import struct
import time
import uuid


BASE = ('72fdc5a062e9e1f4d206ace4b3dbddb8e38921c2',
        '38e71776882697fd141201c23e6e55ae0442d8f9')
ARMS = {
    'before': ('4ad36779cf830b36a5248cf76d4150d122884dda', 'f68427879b413b261ad8c5da27faff042cccef20'),
    'after': ('02802c8a563f48758f346fa9a0c633bbcee5abf9', 'f92373909001e423c999a57f8ceaaaff7b7869c5'),
}
TARGET = Path('/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-target')
LOCK = TARGET.parent / 'lane-c.lock'
TOOL = Path('/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin')
ZIG_VERSION = '0.15.2'
# NULL is a source gap, not a runtime flag. No caller can supply a positive
# binding via inputs. A future actual owner/binder must be reviewed and composed.
ORIGINAL_COMMAND_BINDING = None
MAX_LOG = 128 * 1024 * 1024
MAX_ELF = 2 * 1024 ** 3
MAX_CONFIG = 1024 * 1024
GRAPHS = (
    ('cli', 'cagents', 'src/main.rs', ('development',), 600),
    ('host', 'cagentsd', 'src/bin/cagentsd.rs', ('development', 'host'), 600),
    ('ui', 'cagents-ui', 'src/bin/cagents-ui.rs', ('development', 'ui'), 900),
)


class Hold(RuntimeError):
    pass


def require(value, reason):
    if not value:
        raise Hold(reason)


def unique_json(raw):
    def pairs(items):
        value = {}
        for key, item in items:
            require(key not in value, 'Duplicate JSON key')
            value[key] = item
        return value
    def nonfinite(_value):
        raise Hold('Nonfinite JSON')
    return json.loads(raw, object_pairs_hook=pairs, parse_constant=nonfinite)


def commands():
    """Exact browserless lines191-196; JSON only changes output presentation."""
    return [dict(role=role, name=name, source=source, features=list(features), seconds=seconds,
                 argv=[str(TOOL / 'cargo'), 'build', '--locked', '--profile', 'dev-runtime',
                       '--no-default-features', '--features', ','.join(features), '--bin', name,
                       '--message-format=json'])
            for role, name, source, features, seconds in GRAPHS]


def zig_environment(raw, command):
    """Actual bounded Zig-env bytes, matching the runner's JSON/ZON fields."""
    require(type(raw) is bytes and 0 < len(raw) <= MAX_CONFIG, 'Unbounded/missing actual Zig output')
    text = raw.decode('utf-8', errors='strict')
    fields = {}
    for key in ('zig_exe', 'version'):
        values = re.findall(r'(?m)^\s*(?:"' + key + r'":|\.' + key + r'\s*=)\s*"([^"\n]+)"\s*,?\s*$', text)
        require(len(values) == 1, 'Missing/ambiguous Zig env field: ' + key)
        fields[key] = values[0]
    executable = Path(fields['zig_exe'])
    require(executable.is_absolute() and executable.name == 'zig'
            and str(executable) == str(command) and fields['version'] == ZIG_VERSION,
            'Actual Zig installation/version differs from frozen runner contract')
    return {'reported_executable': str(executable), 'reported_version': fields['version'],
            'original_stream_sha256': hashlib.sha256(raw).hexdigest(),
            'format': 'runner-compatible JSON/ZON fields, not an execution-chain certificate'}


def cargo_artifact(raw, checkout, graph):
    """Select ONE actual root bin from original COMBINED Cargo output.

    Human progress/warning lines remain raw/unparsed evidence. No fabricated
    split stdout/stderr or success from text. Malformed JSON, error diagnostics,
    duplicate/missing completion and duplicate matching artifacts refuse.
    """
    require(type(raw) is bytes and 0 < len(raw) <= MAX_LOG, 'Actual combined output missing/too large')
    require(raw.endswith(b'\n'), 'Incomplete final Cargo output line')
    checkout = Path(checkout)
    require(checkout.is_absolute(), 'Checkout must be absolute')
    selected, finished, human = [], [], []
    for number, line in enumerate(raw.decode('utf-8', errors='strict').splitlines(), 1):
        if not line.strip():
            continue
        if not line.lstrip().startswith('{'):
            require(not re.match(r'^\s*error(?:\[|:)', line), 'Contrary Cargo error text')
            human.append(number)
            continue
        row = unique_json(line)
        require(type(row) is dict and type(row.get('reason')) is str, 'Invalid Cargo JSON message')
        reason = row['reason']
        if reason == 'build-finished':
            require(type(row.get('success')) is bool, 'Malformed completion')
            finished.append((number, row['success']))
        elif reason == 'compiler-message':
            require(row.get('message', {}).get('level') != 'error', 'Contrary compiler error diagnostic')
        elif reason == 'compiler-artifact':
            if row.get('manifest_path') != str(checkout / 'Cargo.toml'):
                continue
            target = row.get('target', {})
            # The root lib and CLI bin legitimately share the name cagents.
            # Only actual bin-kind artifacts participate in this bin selector.
            if target.get('kind') != ['bin'] or target.get('name') != graph['name']:
                continue
            require(target.get('kind') == ['bin'] and target.get('crate_types') == ['bin']
                    and target.get('src_path') == str(checkout / graph['source']), 'Wrong actual bin/source kind')
            require(row.get('features') == sorted(graph['features']), 'Wrong root feature graph; no union/default')
            package = row.get('package_id')
            require(type(package) is str and package == checkout.as_uri().replace('file:', 'path+file:', 1)
                    + '#cagents@0.1.3', 'Package identity does not bind exact checkout')
            profile = row.get('profile')
            require(type(profile) is dict and profile.get('test') is False
                    and type(profile.get('opt_level')) is str
                    and type(profile.get('debug_assertions')) is bool
                    and type(profile.get('overflow_checks')) is bool, 'Missing actual non-test profile metadata')
            executable = row.get('executable')
            require(executable == str(TARGET / 'dev-runtime' / graph['name'])
                    and type(row.get('filenames')) is list and executable in row['filenames']
                    and type(row.get('fresh')) is bool, 'Wrong actual reported executable/freshness')
            selected.append({'line': number, 'package_id': package, 'manifest_path': row['manifest_path'],
                             'target': target, 'features': row['features'], 'profile': profile,
                             'executable': executable, 'filenames': row['filenames'], 'fresh': row['fresh']})
    require(len(finished) == 1 and finished[0][1] is True, 'No unique successful actual Cargo completion')
    require(len(selected) == 1 and selected[0]['line'] < finished[0][0], 'No unique completed matched bin artifact')
    return {**selected[0], 'requested_profile_from_argv': 'dev-runtime',
            'profile_name_not_present_in_Cargo_JSON': True, 'build_finished_line': finished[0][0],
            'unparsed_human_line_numbers': human, 'stream': 'ORIGINAL_COMBINED_STDOUT_STDERR',
            'raw_sha256': hashlib.sha256(raw).hexdigest(), 'raw_bytes': len(raw),
            'source_limit': 'Actual Cargo report plus exact command/source/context; fresh=false is observed cached reuse, not a new compile claim'}


def identity(info):
    return dict(device=info.st_dev, inode=info.st_ino, uid=info.st_uid,
                mode=stat.S_IMODE(info.st_mode), links=info.st_nlink, bytes=info.st_size,
                mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns)


def copy_elf(source, destination, fence, ledger, failure):
    """Actual byte copy before another shared-target build; no ELF execution.

    Each returned FD is retained before metadata, with one close attempt. Calls
    are cooperatively fenced; kernel/metadata calls are not preemptible. Failure
    leaves partial output plus complete known acquisition/close history for the
    ORIGINAL outer owner. No unlink, repair, replacement, retry or C disposition.
    """
    source, destination = Path(source), Path(destination)
    require(source.is_absolute() and destination.is_absolute(), 'Copy requires absolute paths')
    opened, original_error = [], None
    try:
        fence()
        fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        item = {'role': 'source-ELF', 'path': str(source), 'fd': fd, 'open_returned': True,
                'close_attempted': False, 'close_returned': None}
        ledger.append(item); opened.append(item)
        fence(); before = os.fstat(fd); item['identity'] = identity(before)
        require(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= MAX_ELF
                and before.st_uid == os.getuid() and before.st_nlink >= 1,
                'Actual Cargo executable is not an owned bounded ordinary file')
        fence(); named = source.lstat()
        require(identity(named) == identity(before) and stat.S_ISREG(named.st_mode), 'Source name changed')
        fence()
        output = os.open(destination, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o700)
        target = {'role': 'immutable-ELF-copy', 'path': str(destination), 'fd': output,
                  'open_returned': True, 'close_attempted': False, 'close_returned': None}
        ledger.append(target); opened.append(target)
        fence(); target['identity_at_open'] = identity(os.fstat(output))
        copied, digest, header = 0, hashlib.sha256(), b''
        while True:
            fence(); block = os.read(fd, min(1024 * 1024, MAX_ELF - copied + 1))
            if not block:
                break
            copied += len(block); require(copied <= before.st_size, 'ELF grew during copy')
            if not header:
                header = block[:64]
            digest.update(block); view = memoryview(block)
            while view:
                fence(); count = os.write(output, view)
                require(count > 0, 'Short immutable-copy write'); view = view[count:]
        require(copied == before.st_size and len(header) == 64 and header[:6] == b'\x7fELF\x02\x01'
                and struct.unpack_from('<H', header, 16)[0] in (2, 3)
                and struct.unpack_from('<H', header, 18)[0] == 62, 'Not a complete Linux x86_64 ELF')
        fence(); require(identity(os.fstat(fd)) == identity(before), 'Original ELF changed while copied')
        fence(); require(identity(source.lstat()) == identity(before), 'Original ELF path replaced')
        fence(); os.fsync(output)
        fence(); final = os.fstat(output)
        require(stat.S_ISREG(final.st_mode) and final.st_uid == os.getuid() and final.st_nlink == 1
                and stat.S_IMODE(final.st_mode) == 0o700 and final.st_size == copied,
                'Immutable copy identity/mode differs')
        fence(); os.lseek(output, 0, os.SEEK_SET)
        readback = hashlib.sha256(); checked = 0
        while True:
            fence(); block = os.read(output, min(1024 * 1024, copied - checked + 1))
            if not block:
                break
            checked += len(block); require(checked <= copied, 'Copy grew during readback')
            readback.update(block)
        fence(); require(checked == copied and readback.digest() == digest.digest()
                         and identity(os.fstat(output)) == identity(final), 'Actual copy bytes changed')
        fence(); require(identity(destination.lstat()) == identity(final), 'Copied ELF path replaced')
        target.update(identity_after_write=identity(final), sha256=readback.hexdigest(), bytes_copied=copied)
        return {'source': str(source), 'source_identity': identity(before), 'path': str(destination),
                'identity': identity(final), 'sha256': readback.hexdigest(), 'bytes': copied,
                'ELF': 'actual-header Linux x86_64 ET_EXEC/ET_DYN; NOT runtime/dependency closure',
                'copy': 'original bounded byte reads/writes and fsync returned; caller must retain close ledger'}
    except BaseException as error:
        original_error = error; failure(error); raise
    finally:
        for item in reversed(opened):
            if item['close_attempted']:
                continue
            try:
                fence(); item['close_attempted'] = True
                os.close(item['fd']); item['close_returned'] = True
            except BaseException as error:
                item['close_error'] = {'type': type(error).__name__, 'errno': getattr(error, 'errno', None)}
                # If the fence failed, close was never attempted; don't claim an
                # ambiguous syscall. If close was called, no numeric retry.
                item['close_returned'] = False if item['close_attempted'] else None
                failure(error)
                if original_error is None:
                    original_error = error
        if original_error is not None and not all(item['close_returned'] is True for item in opened):
            raise Hold('Copy/one-shot closure incomplete; original owner must retain HOLD') from original_error


def observe_context(checkout, environment, fence):
    """Actual metadata/digests ONLY; NOT Cargo's complete effective resolution."""
    checkout = Path(checkout)
    cargo_home = Path(environment.get('CARGO_HOME', str(Path(environment['HOME']) / '.cargo')))
    candidates = {base / '.cargo' / name for base in [checkout, *checkout.parents]
                  for name in ('config', 'config.toml')}
    candidates.update(cargo_home / name for name in ('config', 'config.toml'))
    configs = {}
    for path in sorted(candidates):
        fence()
        try:
            before = path.lstat()
        except FileNotFoundError:
            configs[str(path)] = {'present': False}; continue
        require(stat.S_ISREG(before.st_mode) and before.st_size <= MAX_CONFIG,
                'Config is aliased/nonregular/unbounded; no repair')
        fence()
        with path.open('rb') as stream:
            fence(); raw = stream.read(MAX_CONFIG + 1)
        require(len(raw) <= MAX_CONFIG, 'Configuration exceeded read bound')
        fence(); require(identity(path.lstat()) == identity(before), 'Configuration changed during read')
        configs[str(path)] = {'present': True, 'identity': identity(before),
                              'sha256': hashlib.sha256(raw).hexdigest()}
    keys = {'PATH', 'HOME', 'CODEX_HOME', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'RUSTC', 'RUSTDOC',
            'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
            'RUSTDOCFLAGS', 'CARGO_ENCODED_RUSTDOCFLAGS', 'CC', 'CXX', 'AR', 'LD', 'LD_LIBRARY_PATH',
            'PKG_CONFIG_PATH', 'PKG_CONFIG_LIBDIR', 'PKG_CONFIG_SYSROOT_DIR', 'CAGENTS_ZIG'}
    hashed = {key: hashlib.sha256(value.encode()).hexdigest() for key, value in environment.items()
              if key in keys or key.startswith(('CARGO_BUILD_', 'CARGO_TARGET_', 'CARGO_PROFILE_', 'CC_', 'CXX_', 'AR_'))}
    fence()
    return {'configs': configs, 'opaque_input_hashes': hashed,
            'context_scope': 'Declared ancestor/CARGO_HOME files and selected input hashes; includes/wrappers/shims/target runner/effective semantic resolution remain UNKNOWN',
            'authority_values': 'PRESERVED_NOT_DISCLOSED_OR_AUTHENTICATED', 'full_chain_config_import_qualification': None}



def executable_observation(path, expected_sha256, fence):
    path = Path(path)
    fence(); before = path.lstat()
    require(path.is_absolute() and path.resolve(strict=True) == path and stat.S_ISREG(before.st_mode)
            and before.st_mode & 0o111 and 0 < before.st_size <= 256 * 1024 * 1024,
            'Actual executable is aliased/unbounded/nonexecutable')
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while True:
            fence(); block = stream.read(1024 * 1024)
            if not block:
                break
            digest.update(block)
    fence(); require(identity(path.lstat()) == identity(before), 'Actual executable changed')
    require(digest.hexdigest() == expected_sha256, 'Actual executable hash differs from source-bound expectation')
    return {'path': str(path), 'identity': identity(before), 'actual_sha256': digest.hexdigest(),
            'version_process': None, 'version_source': 'physical1.93 path/source-bound hash expectation only'}


def source_observation(owner, arm, checkout, stage, fence):
    # These are actual maintained Runner names, not a new source-check API.
    # Their future safe native-task binding is still NULL; no current HOST
    # plan/canonical slot/old prior return is silently adopted.
    fence(); owner.check_bindings()
    require(owner.paths.checkout == checkout and owner.plan['source']['head'] == ARMS[arm][0]
            and owner.plan['source']['tree'] == ARMS[arm][1], 'Original source plan belongs to another arm')
    owner.source_observation(stage)
    fence()


def write_copy_receipt(path, value, fence, ledger, failure):
    payload = (json.dumps(value, sort_keys=True, allow_nan=False) + '\n').encode()
    require(len(payload) <= MAX_CONFIG, 'Per-bin immutable receipt exceeded bound')
    item = None
    try:
        fence(); fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        item = {'role': 'immutable-bin-receipt', 'path': str(path), 'fd': fd,
                'open_returned': True, 'close_attempted': False, 'close_returned': None}
        ledger.append(item)
        view = memoryview(payload)
        while view:
            fence(); count = os.write(fd, view)
            require(count > 0, 'Short receipt write'); view = view[count:]
        fence(); os.fsync(fd)
        fence(); info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_size == len(payload), 'Wrong receipt identity')
        item.update(identity=identity(info), sha256=hashlib.sha256(payload).hexdigest())
    except BaseException as error:
        failure(error); raise
    finally:
        if item is not None:
            try:
                fence(); item['close_attempted'] = True
                os.close(item['fd']); item['close_returned'] = True
            except BaseException as error:
                item['close_returned'] = False if item['close_attempted'] else None
                item['close_error'] = {'type': type(error).__name__, 'errno': getattr(error, 'errno', None)}
                failure(error); raise
    return {'path': str(path), 'identity': item['identity'], 'sha256': item['sha256']}

def require_original_owner(invoke, fence, failure):
    """No currently supported safe binder: refuse, without any discovery/effect."""
    binding = ORIGINAL_COMMAND_BINDING
    require(binding is not None, 'SAFE_ORIGINAL_COMMAND_OWNER_BINDING_UNSUPPLIED; source producer HOLD')
    # This branch is for a future explicit source composition, not caller JSON.
    # Exact source/code object expectations are correlation only; actual outer
    # User/resource instruction and original custody remain external.
    callbacks = {'invoke': invoke, 'fence': fence, 'failure': failure}
    for name, callback in callbacks.items():
        require(inspect.ismethod(callback) and callback.__self__ is invoke.__self__,
                'Callbacks must belong to the SAME retained original owner')
        expected = binding[name]
        require(callback.__func__.__qualname__ == expected['qualname']
                and callback.__func__.__code__.co_filename == expected['path'], 'Wrong actual owner callsite')
        require(hashlib.sha256(Path(expected['path']).read_bytes()).hexdigest() == expected['sha256'],
                'Original callback source changed')
    return invoke.__self__


def original_output(result, record):
    """Maintained capture shape is combined output, not two invented streams."""
    require(result.state == 'finished' and type(result.original_exit) is int and result.original_exit == 0
            and result.cleanup_exit is None and getattr(result, 'retention_error', None) is None,
            'No unforced original successful command return')
    require(record['state'] == result.state and record['original_exit'] == result.original_exit
            and record['cleanup_exit'] is None and record['stdout_complete'] is True
            and record['unforced_own_family'] is True, 'Original record/result contradiction or unforced family missing')
    require(type(result.stdout) is bytes and 0 < len(result.stdout) <= MAX_LOG
            and record['stdout_bytes'] == len(result.stdout)
            and record['log_sha256'] == hashlib.sha256(result.stdout).hexdigest(),
            'Original combined output/retention differs')
    return result.stdout


def produce_arm(arm, checkout, evidence_root, inputs, retained, invoke, fence, failure):
    """Finite task seam; NEVER owns/spawns/signals/closes a process or Cfd.

    Future safe original owner must support exact per-command environment input,
    retain original Popen/wait/pidfd/family/output facts and clamp this entire
    job to its one original2400 deadline. Current binding NULL refuses first.
    """
    owner = require_original_owner(invoke, fence, failure)
    require(arm in ARMS and type(inputs) is dict, 'Unknown arm/inputs')
    checkout, evidence_root = Path(checkout), Path(evidence_root)
    require(checkout.is_absolute() and evidence_root.is_absolute(), 'Absolute source/evidence roots required')
    require(evidence_root.parent == Path('/tmp') and evidence_root.name.startswith('cagents-history-build-'),
            'Evidence must use a fresh direct private /tmp task root')
    root_uuid = evidence_root.name.removeprefix('cagents-history-build-')
    require(str(uuid.UUID(root_uuid)) == root_uuid and uuid.UUID(root_uuid).int != 0, 'Canonical nonnil evidence-root UUID required')
    require(owner.value.get('matched_build_producer') is retained and retained == {},
            'Original owner must retain the exact producer state BEFORE any effect')
    retained.update(arm=arm, source=ARMS[arm], copies=[], acquisitions=[], tool_observations=[],
                    root_creation=None, status='STARTED_NOT_QUALIFIED')
    ledger, artifacts = retained['acquisitions'], retained['copies']
    try:
        fence()
        # Source/head/tree/clean-context/tool pin admission and original C ownership
        # belong to the exact future binder. A caller tuple is never source proof.
        source_observation(owner, arm, checkout, 'matched-build-before', fence)
        env = os.environ.copy()
        env.update(CARGO_TARGET_DIR=str(TARGET), CARGO_BUILD_JOBS='2', RUSTC=str(TOOL / 'rustc'),
                   RUSTDOC=str(TOOL / 'rustdoc'), RUSTUP_TOOLCHAIN='1.93.0', RUSTUP_AUTO_INSTALL='0')
        zig = Path(inputs['zig_executable'])
        require(zig.is_absolute() and zig.name == 'zig', 'Actual physical Zig path absent')
        require('CAGENTS_ZIG' not in env or env['CAGENTS_ZIG'] == str(zig), 'Inherited Zig selection conflicts; no repair')
        retained['tool_observations'].append(executable_observation(zig, inputs['zig_sha256'], fence))
        for name, expected in inputs['rust193_sha256'].items():
            require(name in ('cargo', 'rustc', 'rustdoc'), 'Unexpected physical compiler tool')
            retained['tool_observations'].append(executable_observation(TOOL / name, expected, fence))
        require(set(inputs['rust193_sha256']) == {'cargo', 'rustc', 'rustdoc'}, 'Complete physical compiler pins absent')
        fence()
        zresult, zrecord = invoke([str(zig), 'env'], 'zig-env', 30, cwd=checkout, environment=env)
        retained['zig_original_command_record'] = zrecord
        zig_proof = zig_environment(original_output(zresult, zrecord), zig)
        env['PATH'] = str(zig.parent) + os.pathsep + str(TOOL) + os.pathsep + env['PATH']
        # The normal script does PATH lookup after Zig prepend; no silent shortcut
        # if that would resolve an unexpected Cargo/shim instead of physical1.93.
        resolved = next((Path(part) / 'cargo' for part in env['PATH'].split(os.pathsep)
                         if part and (Path(part) / 'cargo').is_file() and os.access(Path(part) / 'cargo', os.X_OK)), None)
        require(resolved == TOOL / 'cargo', 'Zig-first PATH does not select exact physical Cargo; no repair')
        before = observe_context(checkout, env, fence)
        fence(); os.mkdir(evidence_root, 0o700)
        retained['root_creation'] = {'path': str(evidence_root), 'mkdir_returned': True}
        fence(); root_info = evidence_root.lstat()
        require(stat.S_ISDIR(root_info.st_mode) and root_info.st_uid == os.getuid()
                and stat.S_IMODE(root_info.st_mode) == 0o700, 'Private exclusive artifact root required')
        retained['root_creation']['identity'] = identity(root_info)
        retained['context_before'] = before
        owner.save()
        for graph in commands():
            source_observation(owner, arm, checkout, 'before-' + graph['role'], fence)
            require(observe_context(checkout, env, fence) == before, 'Build context changed')
            result, raw_record = invoke(graph['argv'], 'build-' + graph['role'], graph['seconds'],
                                        cwd=checkout, environment=env)
            actual = {'role': graph['role'], 'graph': graph, 'original_command_record': raw_record, 'immutable_copy': None}
            artifacts.append(actual)  # Original owner retains partial phase BEFORE parsing/copy.
            artifact = cargo_artifact(original_output(result, raw_record), checkout, graph)
            actual['actual_Cargo'] = artifact
            source_observation(owner, arm, checkout, 'after-' + graph['role'], fence)
            require(observe_context(checkout, env, fence) == before, 'Context changed after build')
            copied = copy_elf(artifact['executable'], evidence_root / graph['name'], fence, ledger, failure)
            require(all(item['close_returned'] is True for item in ledger), 'Known artifact FD close incomplete')
            actual['immutable_copy'] = copied
            actual['copy_receipt'] = write_copy_receipt(evidence_root / (graph['name'] + '.json'), actual, fence, ledger, failure)
            owner.save()
        source_observation(owner, arm, checkout, 'matched-build-final', fence)
        require(observe_context(checkout, env, fence) == before, 'Final build context changed')
        retained['status'] = 'THREE_ACTUAL_ARTIFACT_COPIES_RETURNED_NOT_NATIVE_QUALIFICATION'
        owner.save()
        return {'record': 'pr1310-matched-build-producer-observations-v1', 'arm': arm, 'source': ARMS[arm],
                'actual_artifacts': artifacts, 'Zig_preflight': zig_proof, 'context': before,
                'matched': {row['role']: {'path': row['immutable_copy']['path'], 'sha256': row['immutable_copy']['sha256']}
                            for row in artifacts}, 'renderer_build': None,
                'known_copy_FD_ledger': ledger, 'original_owner_identity': id(owner),
                'requested_profile': 'dev-runtime', 'native_import_chain_config_authority_qualification': None,
                'resource_return': 'EXTERNAL_ORIGINAL_OWNER_ONLY_NO_CFD_HERE'}
    except BaseException as error:
        retained['status'] = 'FAILED_OR_PARTIAL_HOLD'
        retained['error'] = {'type': type(error).__name__, 'message': str(error)[:4096]}
        failure(error)
        raise


if __name__ == '__main__':
    raise Hold('Producer library only; SAFE_ORIGINAL_COMMAND_OWNER_BINDING_UNSUPPLIED, no standalone execution')
