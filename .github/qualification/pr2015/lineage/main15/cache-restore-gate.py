"""Fail before compilation unless the intended hosted debug UI cache was restored.

Read-only target observations establish cache presence, not Cargo freshness or
warm dev-runtime/host/all-target suites. No missing target is created here.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time


class GateFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise GateFailure(message)


def identity(info):
    return dict(device=info.st_dev, inode=info.st_ino, bytes=info.st_size,
                mode=stat.S_IMODE(info.st_mode), uid=info.st_uid,
                mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns)


def directory(path):
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and not path.is_symlink(), 'Expected existing nonsymlink directory: ' + str(path))
    require(info.st_uid == os.getuid(), 'Cache directory owner changed: ' + str(path))
    return identity(info)


def observe(path, target, limit=None):
    require(path.is_relative_to(target) and path != target, 'Cache marker escaped target')
    parent = path.parent
    while parent != target:
        directory(parent)
        parent = parent.parent
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and not path.is_symlink() and info.st_uid == os.getuid(),
            'Expected owned regular cache artifact: ' + str(path))
    require(info.st_size > 0 and (limit is None or info.st_size <= limit), 'Empty or oversized cache marker: ' + str(path))
    value = hashlib.sha256()
    data = bytearray() if limit is not None else None
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        require(identity(os.fstat(stream.fileno())) == identity(info), 'Cache artifact changed before read')
        header = stream.read(16)
        value.update(header)
        if data is not None:
            data.extend(header)
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
            if data is not None:
                data.extend(block)
        require(identity(os.fstat(stream.fileno())) == identity(info), 'Cache artifact changed during read')
    require(identity(path.lstat()) == identity(info), 'Cache artifact path changed during read')
    return {'path': str(path), 'identity': identity(info), 'sha256': value.hexdigest(),
            'header_hex': header.hex()}, bytes(data) if data is not None else header


def cache_metadata(env, lock):
    require(env.get('CACHE_RESTORE_OUTCOME') == 'success', 'Cache restore step did not succeed')
    require(env.get('RUNNER_OS') == 'Linux' and env.get('RUNNER_ARCH') == 'X64', 'Unexpected hosted cache platform')
    lock_sha = hashlib.sha256(lock).digest()
    # The workflow hashes this one exact file. GitHub hashFiles hashes the
    # concatenated per-file binary SHA256 digests, including for one file.
    lock_hash = hashlib.sha256(lock_sha).hexdigest()
    require(env.get('CACHE_LOCK_HASH') == lock_hash, 'Workflow Cargo.lock hashFiles binding changed')
    control = env.get('CACHE_CONTROL_SHA', '')
    require(re.fullmatch(r'[0-9a-f]{40}', control) is not None, 'Missing immutable control SHA')
    prefix = 'held1922-Linux-X64-rust193-' + lock_hash + '-'
    primary, matched = env.get('CACHE_PRIMARY_KEY', ''), env.get('CACHE_MATCHED_KEY', '')
    require(primary == prefix + control, 'Cache primary-key output is not the intended exact key')
    require(matched.startswith(prefix) and re.fullmatch(r'[0-9a-f]{40}', matched[len(prefix):]) is not None,
            'Cache miss or untrusted matched-key output')
    require(env.get('CACHE_HIT') == ('true' if matched == primary else 'false'), 'Cache hit output disagrees with the actual matched key')
    return {'restore_outcome': env['CACHE_RESTORE_OUTCOME'], 'primary_key': primary, 'matched_key': matched,
            'cache_hit': env['CACHE_HIT'], 'match_kind': 'primary' if matched == primary else 'trusted-prefix-fallback',
            'trusted_prefix': prefix, 'cargo_lock_sha256': lock_sha.hex(), 'cargo_lock_hash_files': lock_hash}


def toolchains(rustc, cargo):
    result = {}
    for name, text in [('rustc', rustc), ('cargo', cargo)]:
        require(text.startswith(name + ' 1.93.0 ('), 'Active ' + name + ' is not Rust 1.93.0')
        fields = dict(line.split(': ', 1) for line in text.splitlines()[1:] if ': ' in line)
        require(fields.get('release') == '1.93.0' and fields.get('host') == 'x86_64-unknown-linux-gnu',
                'Active ' + name + ' release/host does not match the hosted lane')
        require(re.fullmatch(r'[0-9a-f]{40}', fields.get('commit-hash', '')) is not None,
                'Active ' + name + ' commit identity is missing')
        result[name] = {'stdout': text, 'stdout_sha256': hashlib.sha256(text.encode()).hexdigest(), 'fields': fields}
    return result


def warm_target(target, runner_temp, rustc):
    require(target.is_absolute() and runner_temp.is_absolute(), 'Absolute target/runtime paths required')
    require(runner_temp.resolve(strict=True) == runner_temp, 'Runner temp path is an alias')
    require(target == runner_temp / 'held-scope-target' and target.resolve(strict=True) == target,
            'Expected existing isolated RUNNER_TEMP target; no cold target will be created')
    directories = {str(path): directory(path) for path in (target, target / 'debug', target / 'debug/deps', target / 'debug/.fingerprint')}
    compiler_marker, data = observe(target / '.rustc_info.json', target, 1024 * 1024)
    marker = json.loads(data)
    require(type(marker.get('rustc_fingerprint')) is int and isinstance(marker.get('outputs'), dict), 'Unsupported normal Cargo rustc marker')
    versions = [(key, value) for key, value in marker['outputs'].items()
                if isinstance(value, dict) and isinstance(value.get('stdout'), str) and value['stdout'].startswith('rustc ')]
    require(versions and all(value.get('success') is True and value.get('code') == 0 and value['stdout'] == rustc
                             for _, value in versions), 'Restored Cargo rustc version output does not match the active compiler')
    candidates = {}
    for package, crate in [('libcosmic', 'cosmic'), ('iced_wgpu', 'iced_wgpu'), ('wgpu-core', 'wgpu_core')]:
        paths = sorted((target / 'debug/deps').glob('lib' + crate + '-*.rmeta'))
        require(0 < len(paths) <= 128, 'Missing or excessive warm debug UI metadata: ' + crate)
        accepted, rejected = [], []
        for artifact in paths:
            try:
                suffix = artifact.name.removeprefix('lib' + crate + '-').removesuffix('.rmeta')
                require(re.fullmatch(r'[0-9a-f]{16}', suffix) is not None, 'Unexpected Cargo artifact suffix')
                artifact_receipt, header = observe(artifact, target)
                require(header.startswith(b'rust\x00\x00\x00\x0a') and artifact_receipt['identity']['bytes'] >= 128,
                        'Invalid or empty Rust 1.93 metadata header')
                path = target / 'debug/.fingerprint' / (package + '-' + suffix)
                fingerprint_receipt, data = observe(path / ('lib-' + crate + '.json'), target, 1024 * 1024)
                fingerprint = json.loads(data)
                require(type(fingerprint.get('rustc')) is int and type(fingerprint.get('profile')) is int
                        and fingerprint.get('compile_kind') == 0, 'Missing native compiler/profile fingerprint')
                features = json.loads(fingerprint['features'])
                require(isinstance(features, list) and all(isinstance(value, str) for value in features), 'Unsupported Cargo feature marker')
                if crate == 'cosmic':
                    require({'a11y', 'iced_wgpu', 'wgpu', 'x11'}.issubset(features), 'Restored cosmic metadata lacks this native UI graph')
                hash_receipt, hash_data = observe(path / ('lib-' + crate), target, 128)
                require(re.fullmatch(rb'[0-9a-f]{16}', hash_data) is not None, 'Invalid normal Cargo fingerprint hash')
                dep_receipt, _ = observe(path / ('dep-lib-' + crate), target, 1024 * 1024)
                accepted.append({'artifact': artifact_receipt, 'fingerprint': fingerprint_receipt,
                                 'fingerprint_hash': hash_receipt, 'dep_info': dep_receipt,
                                 'compiler_fingerprint': fingerprint['rustc'], 'profile': fingerprint['profile'],
                                 'features': features, 'compile_kind': fingerprint['compile_kind']})
            except (GateFailure, OSError, ValueError, KeyError, TypeError) as error:
                rejected.append({'path': str(artifact), 'reason': str(error)})
        require(accepted, 'No complete warm native UI metadata for ' + crate + ': ' + json.dumps(rejected))
        candidates[crate] = accepted
    common = set.intersection(*({row['compiler_fingerprint'] for row in values} for values in candidates.values()))
    require(common, 'Warm UI metadata compiler fingerprints disagree')
    compiler = min(common)
    selected = {crate: next(row for row in values if row['compiler_fingerprint'] == compiler)
                for crate, values in candidates.items()}
    return {'path': str(target), 'directories': directories, 'rustc_info': compiler_marker,
            'matched_rustc_output_keys': [key for key, _ in versions], 'ui_dependencies': selected,
            'scope': 'Existing debug UI metadata and normal Cargo fingerprints only; Cargo decides freshness. No dev-runtime/host/full-suite warmth claim.'}


def version(name):
    executable = shutil.which(name)
    require(executable is not None, 'Active ' + name + ' is missing')
    command = [name, '--version', '--verbose']
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
    return result.stdout, {'argv': command, 'exit_code': result.returncode, 'stderr': result.stderr,
                           'stdout': result.stdout,
                           'launcher': executable, 'resolved_launcher': str(Path(executable).resolve(strict=True))}


def main():
    receipt = {'outcome': 'failed', 'started_unix_ms': time.time_ns() // 1_000_000,
               'source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               'target_write_operations': 0, 'cargo_compile_commands': 0}
    output = None
    try:
        require(len(sys.argv) == 2, 'Pass exactly the immutable product checkout')
        require(os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted' and 'CAGENTS_AGENT_AUTHORITY' not in os.environ,
                'A genuine hosted User context is required; inherited authority is unchanged')
        require(os.environ.get('RUSTUP_TOOLCHAIN') == '1.93.0', 'Pinned Rust toolchain environment changed')
        temporary = Path(os.environ['RUNNER_TEMP'])
        evidence = Path(os.environ['QUALIFICATION_EVIDENCE'])
        require(evidence == temporary / 'held-scope-evidence' and temporary.resolve(strict=True) == temporary,
                'Unexpected evidence location')
        evidence.mkdir(mode=0o700, exist_ok=True)
        directory(evidence)
        output = evidence / 'cache-restore-gate.json'
        receipt['cache_outputs'] = {name: os.environ.get(name) for name in
                                    ('CACHE_RESTORE_OUTCOME', 'CACHE_PRIMARY_KEY', 'CACHE_MATCHED_KEY', 'CACHE_HIT')}
        receipt['cache'] = cache_metadata(os.environ, (Path(sys.argv[1]) / 'Cargo.lock').read_bytes())
        rustc, rustc_command = version('rustc')
        receipt['version_commands'] = [rustc_command]
        cargo, cargo_command = version('cargo')
        receipt['version_commands'] = [rustc_command, cargo_command]
        require(rustc_command['exit_code'] == 0 and cargo_command['exit_code'] == 0, 'Version-only toolchain command failed')
        receipt['toolchains'] = toolchains(rustc, cargo)
        receipt['target'] = warm_target(Path(os.environ['CARGO_TARGET_DIR']), temporary, rustc)
        receipt['outcome'] = 'success'
    except (GateFailure, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        receipt['error'] = {'type': type(error).__name__, 'message': str(error)}
    receipt['finished_unix_ms'] = time.time_ns() // 1_000_000
    if output is not None:
        with output.open('x') as stream:
            json.dump(receipt, stream, indent=2)
            stream.write('\n')
    print(json.dumps(receipt, indent=2))
    return 0 if receipt['outcome'] == 'success' else 1


if __name__ == '__main__':
    sys.exit(main())
