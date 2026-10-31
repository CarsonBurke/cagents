"""Ten isolated UI records for the separate final-main integration packet.

This runner does not execute the original 52-parent/full100 packet or reinterpret
its historical registry. Matched native development builds are recorded separately.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import tempfile
import subprocess
import sys
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require_user():
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise RuntimeError('Genuine hosted User required; inherited authority is never removed')


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True).strip()


def main():
    require_user()
    if len(sys.argv) != 4 or sys.argv[3] not in ('--focused-only', '--ui-suites-only'):
        raise RuntimeError('Pass product control and exactly one stage')
    product, control = [Path(value).resolve(strict=True) for value in sys.argv[1:3]]
    plan = json.loads((control / '.github/qualification/final-main-plan.json').read_text())
    assert len(plan['selectors']) == 4 and len(plan['repository_selectors']) == 3
    assert len(set(plan['selectors'] + plan['repository_selectors'])) == 7
    assert plan['fresh_rust_records'] == ['ui-check', 'ui-all-targets-compile',
        *[selector.rsplit('::', 1)[-1] for selector in plan['selectors'] + plan['repository_selectors']], 'ui-all-targets-suite']
    out = Path(os.environ['QUALIFICATION_EVIDENCE'])
    out.mkdir(parents=True, exist_ok=True)
    private_tmp = Path(tempfile.mkdtemp(prefix='h2018.', dir='/tmp')).resolve(strict=True)
    os.chmod(private_tmp, 0o700)
    assert len(str(private_tmp)) < 30 and all(not (parent / '.git').exists() for parent in [private_tmp, *private_tmp.parents])
    os.environ['TMPDIR'] = str(private_tmp)
    stage = 'focused' if sys.argv[3] == '--focused-only' else 'ui-broad'
    (out / (stage + '-private-tmp.json')).write_text(json.dumps(dict(path=str(private_tmp), mode=0o700,
        uid=private_tmp.stat().st_uid, authority_presence=False,
        scope='Fresh short test-only TMPDIR; genuine parent authority guard ran first and no authority variable was removed.'), indent=2) + '\n')
    records_path = out / 'commands.json'
    records = json.loads(records_path.read_text()) if records_path.exists() else []
    lane = ['--locked', '--no-default-features', '--features', 'ui']

    def unchanged():
        require_user()
        assert git(product, 'rev-parse', 'HEAD') == plan['product']['sha'] == os.environ['PRODUCT_SHA']
        assert git(product, 'rev-parse', 'HEAD^{tree}') == plan['product']['tree'] == os.environ['PRODUCT_TREE']
        assert not git(product, 'status', '--porcelain=v1')
        assert not git(control, 'status', '--porcelain=v1')

    def run(name, argv, json_lines=False):
        unchanged()
        assert name not in {row['name'] for row in records}
        stdout = out / (name + ('.jsonl' if json_lines else '.log'))
        stderr = out / (name + '.stderr.log')
        started = time.time_ns()
        with stdout.open('xb') as output, stderr.open('xb') as errors:
            process = subprocess.Popen(argv, cwd=product, stdout=output, stderr=errors, start_new_session=True)
            try:
                code = process.wait(timeout=4200)
            except BaseException:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=10)
                raise
        row = dict(name=name, argv=argv, exit_code=code, started_unix_ns=started,
                   completed_unix_ns=time.time_ns(), product=plan['product'],
                   stdout=dict(path=stdout.name, sha256=digest(stdout), bytes=stdout.stat().st_size),
                   stderr=dict(path=stderr.name, sha256=digest(stderr), bytes=stderr.stat().st_size))
        records.append(row)
        records_path.write_text(json.dumps(records, indent=2) + '\n')
        unchanged()
        if code:
            raise RuntimeError(name + ' failed; no later build is permitted')
        return stdout

    unchanged()
    source = dict(product=plan['product'], control_sha=git(control, 'rev-parse', 'HEAD'),
                  control_tree=git(control, 'rev-parse', 'HEAD^{tree}'),
                  workflow_sha256=digest(control / '.github/workflows/qualify-final-main-integration.yml'),
                  plan_sha256=digest(control / '.github/qualification/final-main-plan.json'),
                  run=os.environ['GITHUB_RUN_ID'], attempt=os.environ['GITHUB_RUN_ATTEMPT'],
                  rust_records=10,
                  product_directory=str(product),
                  cargo_target_directory=str(Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True)),
                  scope='Separate integration; retained22/100 stay original b3 evidence')
    if sys.argv[3] == '--focused-only':
        assert not records
        (out / 'source.json').write_text(json.dumps(source, indent=2) + '\n')
        run('ui-check', ['cargo', 'check', *lane, '--bin', 'cagents-ui'])
        compiled = run('ui-all-targets-compile', ['cargo', 'test', *lane, '--all-targets', '--no-run', '--message-format=json'], True)
        rows = [json.loads(line) for line in compiled.read_text().splitlines() if line.startswith('{')]
        def select(name, kind, relative_source):
            candidates = [row for row in rows if row.get('reason') == 'compiler-artifact'
                          and row.get('target', {}).get('name') == name
                          and row.get('target', {}).get('kind') == [kind]
                          and row.get('target', {}).get('src_path') == str(product / relative_source)
                          and row.get('profile', {}).get('test') is True and row.get('executable')]
            assert len(candidates) == 1
            artifact = candidates[0]
            assert artifact.get('package_id') and artifact.get('manifest_path', str(product / 'Cargo.toml')) == str(product / 'Cargo.toml')
            assert 'ui' in artifact['features'] and not {'host', 'integrated-browser', 'full'} & set(artifact['features'])
            binary = Path(artifact['executable']).resolve(strict=True)
            assert str(binary) == artifact['executable'] and binary.is_relative_to(Path(source['cargo_target_directory']))
            return artifact, binary

        library, library_binary = select('cagents', 'lib', 'src/lib.rs')
        repository, repository_binary = select('repository_git_contract', 'test', 'tests/repository_git_contract.rs')
        assert repository['package_id'] == library['package_id']
        for filename, artifact, binary in [('selected-test.json', library, library_binary),
                                            ('selected-repository-test.json', repository, repository_binary)]:
            (out / filename).write_text(json.dumps(dict(artifact=artifact, executable=str(binary),
                observed_binary_sha256=digest(binary), compile_stdout_sha256=digest(compiled),
                scope='CI-observed selected executable; binary bytes are not retained in the artifact'), indent=2) + '\n')
        for selectors, names, binary in [(plan['selectors'], plan['fresh_rust_records'][2:6], library_binary),
                                         (plan['repository_selectors'], plan['fresh_rust_records'][6:9], repository_binary)]:
            for selector, name in zip(selectors, names, strict=True):
                log = run(name, [str(binary), selector, '--exact', '--test-threads=1', '--nocapture', '--color=never'])
                text = log.read_text()
                assert text.splitlines().count('test ' + selector + ' ... ok') == 1
                summaries = re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', text, re.M)
                assert len(summaries) == 1 and summaries[0][:5] == ('ok', '1', '0', '0', '0')
    else:
        assert [row['name'] for row in records] == plan['fresh_rust_records'][:9]
        assert json.loads((out / 'source.json').read_text()) == source
        cleanup = json.loads((out / 'native/cleanup.json').read_text())
        assert cleanup['errors'] == [] and cleanup['owned_descendants_remaining'] == []
        assert (out / 'native/cleanup-complete').is_file()
        log = run('ui-all-targets-suite', ['cargo', 'test', *lane, '--all-targets', '--', '--test-threads=1', '--color=never'])
        text = log.read_text()
        summaries = re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', text, re.M)
        assert summaries and all(status == 'ok' and failed == '0' and filtered == '0' for status, _, failed, _, _, filtered in summaries)
        assert any(int(passed) > 0 for _, passed, *_ in summaries)
        for selector in plan['selectors'] + plan['repository_selectors']:
            assert text.splitlines().count('test ' + selector + ' ... ok') == 1
    unchanged()


if __name__ == '__main__':
    main()
