#!/usr/bin/env python3
"""Startup-only heap attribution hooks for the reviewed private native domain.

This module never launches an application or attaches to a PID. Copy it into a
case's pinned helpers, then call prepare_environment/observe_exec from the direct
UI supervisor and monitor from its admitted holder. Analyze only after graceful
UI exit, using the holder's run_owned_command. Normal RSS/latency runs must not
set heap_profile. No global kernel settings or production processes are changed.
"""
import fcntl
import json
import os
from pathlib import Path
import resource
import stat
import subprocess
import sys
import time

EVIDENCE = Path('/home/marvin/.cache/cagents-issue-owner/evidence')
PRELOAD = Path('/usr/lib64/heaptrack/libheaptrack_preload.so')
INTERPRETER = Path('/usr/lib64/heaptrack/libexec/heaptrack_interpret')
PRINTER = Path('/usr/bin/heaptrack_print')
COMPRESSOR = Path('/usr/bin/zstd')
RAW_LIMIT = 256 * 1024 * 1024  # Diagnostic artifact limit, not a product RAM budget.


def common():
    # The reviewed case supplies identity, source, lock and lifecycle authority.
    # A standalone import is inert; an unadmitted caller cannot prepare a run.
    import common as native
    return native


def require_lifetime_guard(cfg):
    guard = getattr(common(), 'require_profile_lifetime_admission', None)
    if not callable(guard):
        raise RuntimeError('profiling requires the separately reviewed lifetime-held admission guard; stock r5 is insufficient')
    guard(cfg)


def system_tool(path):
    path = path.resolve(strict=True)
    metadata = path.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
        raise RuntimeError('profiling requires an immutable installed system tool')
    return {'path': str(path), 'sha256': common().sha256(path), 'inode': metadata.st_ino,
        'device_major': os.major(metadata.st_dev), 'device_minor': os.minor(metadata.st_dev)}


def configuration(cfg, manifest):
    options = cfg.get('heap_profile')
    if not isinstance(options, dict) or options.get('purpose') != 'heap-attribution':
        raise RuntimeError('an explicit separate heap-attribution case is required')
    seconds = options.get('maximum_seconds', 90)
    if not isinstance(seconds, int) or not 1 <= seconds <= 120:
        raise RuntimeError('profiling interval must be bounded to at most120 seconds')
    if manifest['source_sha'] != cfg['source_sha'] or manifest['profile'] != 'dev-runtime':
        raise RuntimeError('profiling must use the source-bound matched dev-runtime bundle')
    case = Path(cfg['_case']).resolve(strict=True)
    metadata = case.stat()
    if not case.is_relative_to(EVIDENCE) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
        raise RuntimeError('profiling output must stay in the private disk-backed evidence case')
    executable = Path(manifest['products']['cagents-ui']['path']).resolve(strict=True)
    if executable != Path(cfg['_bundle']).resolve(strict=True) / 'bin/cagents-ui':
        raise RuntimeError('profiling requires the immutable bundle UI')
    return case / 'heap-profile', seconds


def prepare_environment(cfg, manifest, environment):
    native = common()
    native.require_build_admission(cfg)
    require_lifetime_guard(cfg)
    native.private_environment(cfg)
    directory, seconds = configuration(cfg, manifest)
    if environment.get('LD_PRELOAD') or environment.get('DUMP_HEAPTRACK_OUTPUT'):
        raise RuntimeError('profiling refuses an inherited injector or output destination')
    tools = {name: system_tool(path) for name, path in (
        ('preload', PRELOAD), ('interpreter', INTERPRETER), ('printer', PRINTER), ('compressor', COMPRESSOR))}
    directory.mkdir(mode=0o700)
    raw = directory / 'heaptrack.raw'
    descriptor = os.open(raw, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_CLOEXEC, 0o600)
    os.close(descriptor)
    prepared = {'schema': 1, 'status': 'prepared_not_executed', 'native_validation': False,
        'source_sha': cfg['source_sha'], 'ui_sha256': manifest['products']['cagents-ui']['sha256'],
        'helper_sha256': native.sha256(__file__), 'tools': tools, 'raw': str(raw),
        'maximum_seconds': seconds, 'raw_artifact_soft_limit_bytes': RAW_LIMIT,
        'deadline_monotonic_ns': time.monotonic_ns() + seconds * 1_000_000_000,
        'profiling_overhead_excludes_this_run_from_normal_rss_latency_comparison': True}
    native.atomic_json(directory / 'prepared.json', prepared)
    result = dict(environment)
    # The installed preloader removes these variables before application code.
    # Actual owned-child maps are nevertheless checked in observe_children.
    result['LD_PRELOAD'] = tools['preload']['path']
    result['DUMP_HEAPTRACK_OUTPUT'] = str(raw)
    return result


def observe_exec(cfg, manifest, ui):
    native = common()
    native.require_build_admission(cfg)
    require_lifetime_guard(cfg)
    directory, _ = configuration(cfg, manifest)
    prepared = json.loads((directory / 'prepared.json').read_text())
    if not native.current(ui):
        raise RuntimeError('profile UI identity ended before observation')
    process = Path('/proc') / str(ui['pid'])
    executable = process.joinpath('exe').resolve(strict=True)
    expected = manifest['products']['cagents-ui']
    if executable != Path(expected['path']) or native.sha256(executable) != expected['sha256']:
        raise RuntimeError('profile target is not the immutable owned UI')
    environment = dict(part.split(b'=', 1) for part in process.joinpath('environ').read_bytes().split(b'\0') if b'=' in part)
    if environment.get(b'XDG_DATA_HOME') != os.fsencode(os.environ['XDG_DATA_HOME']):
        raise RuntimeError('profile UI left the admitted private data root')
    library = prepared['tools']['preload']
    loaded = False
    deadline = min(time.monotonic_ns() + 3_000_000_000, prepared['deadline_monotonic_ns'])
    # /proc/exe switches before ld.so necessarily maps LD_PRELOAD. Wait for the
    # exact library inode without weakening the executable or PID/start fence.
    while native.current(ui) and time.monotonic_ns() < deadline:
        for line in process.joinpath('maps').read_text().splitlines():
            fields = line.split(maxsplit=5)
            if len(fields) == 6 and fields[5] == library['path']:
                major, minor = (int(part, 16) for part in fields[3].split(':'))
                loaded |= (int(fields[4]), major, minor) == (library['inode'], library['device_major'], library['device_minor'])
        if loaded:
            break
        time.sleep(.01)
    if not loaded or not native.current(ui):
        raise RuntimeError('heaptrack preload or UI identity was not observed')
    native.atomic_json(directory / 'ui.json', {'ui': ui, 'source_sha': cfg['source_sha'],
        'ui_sha256': expected['sha256'], 'preload_sha256': prepared['tools']['preload']['sha256'],
        'preload_mapping_observed': True, 'at_monotonic_ns': time.monotonic_ns(), 'native_validation': False})


def monitor(cfg, ui):
    native = common()
    require_lifetime_guard(cfg)
    if not native.current(cfg['native_owner']) or not native.current(ui):
        raise RuntimeError('profile lost its exact native owner or UI')
    directory = Path(cfg['_case']) / 'heap-profile'
    prepared = json.loads((directory / 'prepared.json').read_text())
    if time.monotonic_ns() >= prepared['deadline_monotonic_ns']:
        raise RuntimeError('bounded profile interval expired; case is incomplete')
    raw = Path(prepared['raw'])
    if raw.stat().st_size > RAW_LIMIT:
        raise RuntimeError('profile exceeded its diagnostic artifact soft limit; case is incomplete')
    return {'raw_bytes': raw.stat().st_size, 'at_monotonic_ns': time.monotonic_ns()}


def owned_children(cfg, manifest, ui):
    """Discover only frozen private daemon roots and their data-fenced children."""
    native = common()
    native.require_build_admission(cfg)
    data_home = os.fsencode(os.environ['XDG_DATA_HOME'])
    daemon = manifest['products']['cagentsd']
    roots = []
    for process in Path('/proc').iterdir():
        if not process.name.isdecimal():
            continue
        try:
            if process.stat().st_uid != os.getuid() or process.joinpath('exe').readlink() != Path(daemon['path']):
                continue
            environment = dict(part.split(b'=', 1) for part in process.joinpath('environ').read_bytes().split(b'\0') if b'=' in part)
            if environment.get(b'XDG_DATA_HOME') != data_home:
                continue
            owner = native.identity(int(process.name))
            if native.sha256(process / 'exe') != daemon['sha256'] or not native.current(owner):
                raise RuntimeError('private daemon identity changed')
            roots.append(owner)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
    if not roots:
        raise RuntimeError('profile workload has no observed matched private daemon')
    pending, result, seen = roots[:], [], set()
    while pending:
        owner = pending.pop()
        if owner['pid'] in seen:
            continue
        if len(seen) >= 1024:
            raise RuntimeError('profile child census exceeds its bounded fixture scope')
        seen.add(owner['pid'])
        if not native.current(owner):
            raise RuntimeError('owned process changed during profile census')
        process = Path('/proc') / str(owner['pid'])
        environment = dict(part.split(b'=', 1) for part in process.joinpath('environ').read_bytes().split(b'\0') if b'=' in part)
        if process.stat().st_uid != os.getuid() or environment.get(b'XDG_DATA_HOME') != data_home:
            raise RuntimeError('child escaped the private profile data root')
        result.append(owner)
        children = process.joinpath('task', str(owner['pid']), 'children').read_text().split()
        pending.extend(native.identity(int(pid)) for pid in children)
        if not native.current(owner):
            raise RuntimeError('owned process changed while reading descendants')
    if not native.current(ui):
        raise RuntimeError('profile UI ended during owned child census')
    return result


def observe_children(cfg, children):
    """Only identities supplied by the reviewed domain; never scan/attach globally."""
    native = common()
    native.require_build_admission(cfg)
    directory = Path(cfg['_case']) / 'heap-profile'
    prepared = json.loads((directory / 'prepared.json').read_text())
    observed = []
    for child in children:
        if not native.current(child):
            raise RuntimeError('owned child changed during profile purity observation')
        maps = Path('/proc', str(child['pid']), 'maps').read_text()
        environment = dict(part.split(b'=', 1) for part in Path('/proc', str(child['pid']), 'environ').read_bytes().split(b'\0') if b'=' in part)
        if prepared['tools']['preload']['path'] in maps or environment.get(b'LD_PRELOAD') or environment.get(b'DUMP_HEAPTRACK_OUTPUT'):
            raise RuntimeError('an owned daemon/provider also inherited the UI profiler')
        if not native.current(child):
            raise RuntimeError('owned child changed during profile purity observation')
        observed.append(child)
    native.atomic_json(directory / 'children.json', {'owned_children': observed,
        'preload_mappings_absent': True, 'native_validation': False})


def analyze(cfg):
    """Invoke as an admitted owned command AFTER UI exit; all tools share its PGID."""
    native = common()
    native.require_build_admission(cfg)
    native.verify_source(cfg)
    require_lifetime_guard(cfg)
    manifest = native.verify_manifest(Path(cfg['_bundle']) / 'manifest.json')
    directory, _ = configuration(cfg, manifest)
    prepared = json.loads((directory / 'prepared.json').read_text())
    observed = json.loads((directory / 'ui.json').read_text())
    ended = json.loads((Path(cfg['_case']) / 'snapshot-ui-exit.json').read_text())
    if ended.get('ui') != observed['ui'] or ended.get('returncode') != 0 or native.current(observed['ui']):
        raise RuntimeError('profile analysis requires the exact UI graceful exit receipt')
    for tool in prepared['tools'].values():
        if system_tool(Path(tool['path'])) != tool:
            raise RuntimeError('installed profiling tool changed')
    raw = Path(prepared['raw'])
    if raw != directory / 'heaptrack.raw' or not 0 < raw.stat().st_size <= RAW_LIMIT:
        raise RuntimeError('profile raw output is empty, oversized or outside this case')
    native.private_regular(raw)
    output = directory / 'heaptrack.zst'
    tool_environment = dict(os.environ, DEBUGINFOD_URLS='')
    # Diagnostic analysis limits do not change the application memory policy.
    _, hard = resource.getrlimit(resource.RLIMIT_AS)
    limit = 4 * 1024**3 if hard == resource.RLIM_INFINITY else min(hard, 4 * 1024**3)
    resource.setrlimit(resource.RLIMIT_AS, (limit, limit))
    os.umask(0o077)
    with raw.open('rb') as source, output.open('xb') as destination, (directory / 'analysis.log').open('xb') as log:
        fcntl.flock(source, fcntl.LOCK_EX | fcntl.LOCK_NB)
        interpreter = subprocess.Popen([prepared['tools']['interpreter']['path']], stdin=source,
            stdout=subprocess.PIPE, stderr=log, close_fds=True, env=tool_environment)
        compressor = None
        try:
            compressor = subprocess.Popen([prepared['tools']['compressor']['path'], '-c'],
                stdin=interpreter.stdout, stdout=destination, stderr=log, close_fds=True, env=tool_environment)
            interpreter.stdout.close()
            if compressor.wait() != 0 or interpreter.wait() != 0:
                raise RuntimeError('heaptrack interpretation/compression failed')
        finally:
            # No detached tool process may outlive this admitted command.
            for process in (compressor, interpreter):
                if process is not None and process.poll() is None:
                    process.terminate()
                    try: process.wait(timeout=2)
                    except subprocess.TimeoutExpired: process.kill(); process.wait()
    with (directory / 'allocations.txt').open('xb') as report:
        result = subprocess.run([prepared['tools']['printer']['path'], '-f', str(output),
            '--merge-backtraces=0', '-n', '25', '-s', '5', '-p', '1', '-a', '1', '-l', '1',
            '--print-massif', str(directory / 'timeline.massif')], stdout=report, stderr=subprocess.STDOUT,
            close_fds=True, timeout=90, env=tool_environment)
    if result.returncode:
        raise RuntimeError('heaptrack_print failed')
    native.require_build_admission(cfg)
    native.atomic_json(directory / 'result.json', {'status': 'profile_interpreted', 'native_validation': False,
        'source_sha': cfg['source_sha'], 'ui': observed['ui'], 'raw_bytes': raw.stat().st_size,
        'raw_sha256': native.sha256(raw), 'interpreted_sha256': native.sha256(output),
        'end_live_allocations_are_not_proof_of_leaks': True,
        'malloc_profile_excludes_untracked_mmap_gpu_and_other_process_memory': True})


if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] != 'analyze':
        raise SystemExit('use only: chat_memory_heap.py analyze PRIVATE_CASE_CONFIG')
    analyze(common().load_config(sys.argv[2]))
