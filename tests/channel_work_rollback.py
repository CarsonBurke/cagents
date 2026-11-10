#!/usr/bin/env python3
"""Qualify schema 24 rollback with real matched daemon/CLI pairs.

Run on fw16 from a plain SSH shell, after building schema 22, schema 23 and
current development binaries through scripts/cargo-gate. This uses private
stores and the real installer; no UI, provider or terminal is started. Example:

  python3 tests/channel_work_rollback.py --schema22-bin DIR --schema23-bin DIR \
      --current-bin DIR --evidence-dir /absolute/private/directory

The installer coordinator remains the existing process fixture, while its
store inspection, schema-changing startup and downgrade use the real current
daemon. The previous real daemon must then move an existing pane, not just open
the converted store. Evidence and daemon logs are retained at --evidence-dir.
"""

import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]


def run(binary, *arguments, env, cwd, check=True):
    completed = subprocess.run(
        [str(binary), *map(str, arguments)], env=env, cwd=cwd,
        capture_output=True, text=True, timeout=30,
    )
    if check and completed.returncode:
        raise AssertionError(
            f"{binary.name} {' '.join(map(str, arguments))}: {completed.returncode}\n"
            f"{completed.stdout}{completed.stderr}"
        )
    return completed


def store_path(environment):
    paths = list(Path(environment['XDG_DATA_HOME']).rglob('workspace-model.sqlite3'))
    assert len(paths) == 1, paths
    return paths[0]


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def schema(path):
    with sqlite3.connect(path) as database:
        return database.execute('PRAGMA user_version').fetchone()[0]


@contextlib.contextmanager
def daemon(binaries, environment, cwd, label):
    log_path = Path(environment['HOME']).parent / f'{label}-daemon.log'
    with log_path.open('wb') as log:
        process = subprocess.Popen(
            [str(binaries / 'cagentsd')], env=environment, cwd=cwd,
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    raise AssertionError(f'{label} exited: {log_path.read_text()}')
                result = run(binaries / 'cagents', '--json', 'daemon', 'status',
                             env=environment, cwd=cwd, check=False)
                if result.returncode == 0 and json.loads(result.stdout)['data']['running']:
                    break
                assert time.monotonic() < deadline, f'{label} did not start: {result.stderr}'
                time.sleep(0.1)
            yield
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise AssertionError(f'{label} did not stop')


def seed(path, repository):
    """A long-lived workspace shape: 18 projects, 76 channels, 1856 views."""
    channels = [str(uuid.uuid4()) for _ in range(76)]
    panes = []
    now = int(time.time() * 1000)
    with sqlite3.connect(path) as database:
        database.execute('PRAGMA foreign_keys=ON')
        for index in range(18):
            project = repository / str(index)
            project.mkdir(parents=True)
            database.execute('INSERT INTO projects(repository,name,ordinal,kind) VALUES(?,?,?,?)',
                             (str(project), f'project {index}', index + 1, 'folder'))
        for index, channel in enumerate(channels):
            project = str(repository / str(index % 18))
            database.execute('INSERT INTO channels(id,repository,title,ordinal,title_source) VALUES(?,?,?,?,2)',
                             (channel, project, f'channel {index}', index + 1))
            database.execute('INSERT INTO channel_work_homes(channel,incarnation,repository,title,lifecycle,revision) VALUES(?,?,?,?,?,?)',
                             (channel, index + 1, project, f'channel {index}', 'active', '7'))
            database.execute('INSERT INTO channel_work_entries(channel,incarnation,target,entry,bucket,sequence) VALUES(?,?,?,?,?,?)',
                             (channel, index + 1, f'history-{index}', '{}', 'history', index + 1))
            database.execute('INSERT INTO channel_work_operations(operation,channel,incarnation,record) VALUES(?,?,?,?)',
                             (str(uuid.uuid4()), channel, index + 1, '{}'))
            database.execute('INSERT INTO view_scopes(id,channel,revision) VALUES(?,?,?)',
                             (index + 1, channel, index + 1))
        for index in range(76, 1856):
            project = str(repository / str(index % 18))
            database.execute('INSERT INTO view_scopes(id,repository,worktree,revision) VALUES(?,?,?,?)',
                             (index + 1, project, str(repository / f'worktree-{index}'), index + 1))
        for index in range(96):
            pane, resource = str(uuid.uuid4()), str(uuid.uuid4())
            descriptor = json.dumps(dict(kind='browser', id=resource,
                current_url=f'https://example.invalid/{index}', history=[],
                history_index=0, scroll_x=0.0, scroll_y=0.0, fidelity='url_only'))
            scope = index % 76 + 1
            database.execute('''INSERT INTO panes(id,scope,position,title,user_title,width_fraction,
                pending_terminal_start,filing,kind,resource,descriptor_format,descriptor)
                VALUES(?,?,?,?,?,1.0,0,?,'browser',?,1,?)''',
                (pane, scope, (index // 76 + 1) * (1 << 20), f'pane {index}',
                 f'custom {index}', str(repository / str(index % 18)), resource, descriptor))
            database.execute('INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?,?)',
                             (pane, index + 1))
            panes.append(pane)
        for index in range(1856):
            scope = index + 1
            database.execute('INSERT INTO retired_panes(scope,pane,ordinal,retired_at_unix_ms) VALUES(?,?,0,?)',
                             (scope, str(uuid.uuid4()), now))
            database.execute('INSERT INTO view_operations(operation,scope,applied_at_unix_ms) VALUES(?,?,?)',
                             (str(uuid.uuid4()), scope, now))
        database.execute('UPDATE view_revision_clock SET last=1856 WHERE id=1')
        database.execute('UPDATE channel_work_placement_clock SET last_epoch=96 WHERE id=1')
        assert database.execute('PRAGMA foreign_key_check').fetchall() == []
    # Channels 0, 18 and 36 belong to the same project.
    return panes[0], [channels[index] for index in (0, 18, 36)]


def assert_placement(path, pane, channel, version):
    with sqlite3.connect(path) as database:
        assert schema(path) == version
        if version >= 23:
            location = database.execute('SELECT s.channel FROM panes p JOIN view_scopes s ON s.id=p.scope WHERE p.id=?', (pane,)).fetchone()
        else:
            value = database.execute('SELECT scope FROM shared_view_panes WHERE id=?', (pane,)).fetchone()
            location = (json.loads(value[0])['Channel'],)
        assert location == (channel,), location
        assert database.execute('PRAGMA foreign_key_check').fetchall() == []
        if version <= 23:
            assert database.execute('SELECT count(*) FROM channel_work_placement_epochs').fetchone()[0] == 96
            clock = database.execute('SELECT last_epoch FROM channel_work_placement_clock').fetchone()[0]
            maximum = database.execute('SELECT max(epoch) FROM channel_work_placement_epochs').fetchone()[0]
            assert clock >= maximum > 0


def move(binaries, environment, directory, pane, destination):
    result = run(binaries / 'cagents', '--json', 'channel', 'move', pane, '--to', destination,
                 env=environment, cwd=directory)
    assert json.loads(result.stdout)['data']['placement'] == {'Channel': destination}, result.stdout


def qualify(args):
    spec = importlib.util.spec_from_file_location('installer_coordinator', ROOT / 'tests/installer_coordinator.py')
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    fixture = installer.InstallerCoordinator('runTest')
    fixture.setUp()
    # Retain the fixture under the requested evidence directory, including on
    # failure, instead of allowing TemporaryDirectory to erase its diagnostics.
    original = fixture.directory
    import shutil
    shutil.copytree(original, args.evidence_dir)
    fixture.doCleanups()
    fixture.setUp = lambda: None
    for name, value in list(vars(fixture).items()):
        if isinstance(value, Path) and value.is_relative_to(original):
            setattr(fixture, name, args.evidence_dir / value.relative_to(original))
    fixture.environment = {key: value.replace(str(original), str(args.evidence_dir))
                           for key, value in fixture.environment.items()}
    environment = fixture.environment
    repository = args.evidence_dir / 'projects'
    with daemon(args.schema23_bin, environment, args.evidence_dir, 'initial23'):
        pass
    path = store_path(environment)
    assert schema(path) == 23
    pane, channels = seed(path, repository)
    assert_placement(path, pane, channels[0], 23)

    fixture.installed_cli(guarded=True)
    # The installer's coordinator is still a fixture; only its daemon-start
    # action is replaced with real schema24 admission followed by an induced
    # startup failure. Its store options always run the real converter.
    startup = args.evidence_dir / 'migrate.py'
    fixture_environment = args.evidence_dir / 'fixture-environment.json'
    fixture_environment.write_text(json.dumps(environment))
    startup.write_text('import importlib.util, json, pathlib, sys\n'
        f'spec=importlib.util.spec_from_file_location("rollback",{str(Path(__file__).resolve())!r})\n'
        'module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)\n'
        f'environment=json.loads(pathlib.Path({str(fixture_environment)!r}).read_text())\n'
        f'with module.daemon(pathlib.Path({str(args.current_bin)!r}),environment,pathlib.Path({str(args.evidence_dir)!r}),"installer24"):\n'
        f'    module.move(pathlib.Path({str(args.current_bin)!r}),environment,pathlib.Path({str(args.evidence_dir)!r}),{pane!r},{channels[1]!r})\n'
        f'    assert module.schema(pathlib.Path({str(path)!r})) == 24\n'
        f'pathlib.Path({str(args.evidence_dir / "installer-migrated24")!r}).write_text("24")\n')
    coordinator = installer.COORDINATOR.replace(
        "    if os.environ.get('FIXTURE_DAEMON_START_FAILS'):\n",
        f'    subprocess.run([{sys.executable!r},{str(startup)!r}],check=True)\n'
        "    if os.environ.get('FIXTURE_DAEMON_START_FAILS'):\n")
    fixture.python(fixture.payload / 'bin/cagents', coordinator)
    inspector = ('import os, sys\n'
        'if sys.argv[1:] == ["--stored-schema"] or sys.argv[1:2] == ["--downgrade-store"]:\n'
        f'    os.execv({str(args.current_bin / "cagentsd")!r},[{str(args.current_bin / "cagentsd")!r},*sys.argv[1:]])\n'
        "SOURCE = 'candidate'\n" + installer.INSPECTOR)
    fixture.python(fixture.payload / 'bin/cagentsd', inspector)
    fixture.package()
    environment['FIXTURE_DAEMON_START_FAILS'] = '1'
    result = fixture.install('--restart-daemon')
    (args.evidence_dir / 'installer.stdout').write_text(result.stdout)
    (args.evidence_dir / 'installer.stderr').write_text(result.stderr)
    assert result.returncode != 0, result.stdout + result.stderr
    assert 'replacement daemon startup failed' in result.stderr, result.stderr
    assert os.readlink(fixture.prefix / 'current') == 'versions/old'
    assert not (fixture.prefix / 'versions' / installer.VERSION).exists()
    assert (args.evidence_dir / 'installer-migrated24').read_text() == '24'
    assert_placement(path, pane, channels[1], 23)
    with daemon(args.schema23_bin, environment, args.evidence_dir, 'restored23'):
        move(args.schema23_bin, environment, args.evidence_dir, pane, channels[2])
        assert_placement(path, pane, channels[2], 23)

    # Write through schema24 after upgrading again, then roll back across both
    # converters and require the actual schema22 writer to edit that pane.
    with daemon(args.current_bin, environment, args.evidence_dir, 'upgraded24'):
        assert schema(path) == 24
        move(args.current_bin, environment, args.evidence_dir, pane, channels[1])
        assert_placement(path, pane, channels[1], 24)
    run(args.current_bin / 'cagents', 'daemon', 'downgrade-store', '22',
        env=environment, cwd=args.evidence_dir)
    assert_placement(path, pane, channels[1], 22)
    with daemon(args.schema22_bin, environment, args.evidence_dir, 'restored22'):
        move(args.schema22_bin, environment, args.evidence_dir, pane, channels[0])
        assert_placement(path, pane, channels[0], 22)
    result = dict(schema23_installer_rollback='passed', schema23_edit='passed',
                  schema22_edit='passed', projects=18, channels=76, views=1856, panes=96,
                  binaries={key: {
                      'directory': str(getattr(args, key)),
                      'cli_version': run(getattr(args, key) / 'cagents', '--version',
                                         env=environment, cwd=args.evidence_dir).stdout.strip(),
                      'daemon_sha256': digest(getattr(args, key) / 'cagentsd'),
                  } for key in ('schema22_bin', 'schema23_bin', 'current_bin')})
    (args.evidence_dir / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('schema22-bin', 'schema23-bin', 'current-bin', 'evidence-dir'):
        parser.add_argument('--' + option, type=Path, required=True)
    args = parser.parse_args()
    if any(name in os.environ for name in
           ('CAGENTS_AGENT_AUTHORITY', 'CAGENTS_AGENT_TOKEN', 'CAGENTS_SESSION_TOKEN')):
        parser.error('run this fixture over plain SSH; inherited agent authority is refused')
    for option in ('schema22_bin', 'schema23_bin', 'current_bin'):
        binaries = getattr(args, option).resolve(strict=True)
        for binary in ('cagents', 'cagentsd'):
            assert (binaries / binary).is_file(), binaries / binary
        setattr(args, option, binaries)
    args.evidence_dir = args.evidence_dir.absolute()
    assert not args.evidence_dir.exists(), 'evidence directory must be new'
    qualify(args)


if __name__ == '__main__':
    main()
