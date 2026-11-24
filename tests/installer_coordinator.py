#!/usr/bin/env python3
"""Run the real installer with private coordinator/host process fixtures.

The fixtures exercise archive extraction, coordinator selection, re-entry and
publication without compiling Rust, opening windows or starting a real daemon.
They do not replace the Rust source-inspection or install-guard contracts.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
VERSION = "9.9.9"
JOB = "00000000-0000-4000-8000-000000000001"
TOP = f"cagents-{VERSION}"
ARCHIVE_NAME = f"{TOP}-x86_64-unknown-linux-gnu.tar.gz"
# Preparation deliberately runs syncfs. Parallel gate compilation can leave
# substantial shared-filesystem writeback ahead of this private fixture tree.
INSTALL_TIMEOUT_SECONDS = 60

COORDINATOR = r'''
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
log = pathlib.Path(os.environ['FIXTURE_CALLS'])
def record(event, **fields):
    with log.open('a') as output:
        output.write(json.dumps(dict(event=event, **fields)) + '\n')
forbidden = os.environ.get('FIXTURE_FORBIDDEN_CLI')
if forbidden and pathlib.Path(sys.argv[0]).resolve() == pathlib.Path(forbidden):
    pathlib.Path(os.environ['FIXTURE_FORBIDDEN_EXECUTION']).write_text('executed\n')
    sys.exit(97)
# Fixture-only readiness is represented by a private marker, never a native
# serving socket. Census/receipt envelopes still use the production schema.
fixture_proof = {'host_session_id': 'fixture-daemon', 'boot_id': 'fixture-boot',
                 'host_incarnation': 'fixture-incarnation'}
commands = ('coordinate-install', 'verify-install-guard', 'record-deferred-install')
if len(args) == 2 and args[0] in commands and args[1] == '--help':
    print('Options:\n  --stage-only  Preserve the running application')
elif args[0] == 'coordinate-install':
    helper = pathlib.Path(sys.argv[0]).resolve().with_name('cagentsd')
    # The real coordinator resolves this exact sibling, never a PATH fallback.
    inspected = subprocess.run([str(helper), '--inspect-upgrade-source', 'fixture'],
                               check=True, capture_output=True, text=True)
    assert json.loads(inspected.stdout) == {'Ok': {'stopped': [], 'stopped_windows': [], 'labels': {}}}
    record('coordinate', executable=str(pathlib.Path(sys.argv[0]).resolve()),
           helper=str(helper), mode=helper.stat().st_mode & 0o777)
    installer = args[args.index('--installer') + 1]
    child_args = args[args.index('--') + 1:]
    environment = dict(os.environ, CAGENTS_UI_UPDATE_GUARD_FD='fixture-only')
    sys.exit(subprocess.run(['/bin/sh', installer, *child_args], env=environment).returncode)
elif args[0] == 'verify-install-guard':
    assert args in (['verify-install-guard'], ['verify-install-guard', '--stage-only'])
    assert os.environ['CAGENTS_UI_UPDATE_GUARD_FD'] == 'fixture-only'
    stage_only = args[1:] == ['--stage-only']
    record('verify', stage_only=stage_only)
    if os.environ.get('FIXTURE_WINDOWS_PREPARED') and not stage_only:
        print('application windows have not finished closing', file=sys.stderr)
        sys.exit(1)
elif args[:2] == ['record-deferred-install', '--stage-only']:
    assert len(args) == 6 and args[2] == '--prefix' and args[4] == '--version-root'
    assert os.environ['CAGENTS_UI_UPDATE_GUARD_FD'] == 'fixture-only'
    version_root = pathlib.Path(args[5])
    assert version_root.parent == pathlib.Path(args[3]) / 'versions'
    assert (version_root / 'bin/cagents').is_file()
    (version_root / '.deferred-owner.json').write_text('{"version":1,"owner_version":null}\n')
elif args[:2] == ['daemon', 'capture-update-census']:
    assert args[2] == '--output' and args[4] == '--candidate-cli' and len(args) == 6
    assert pathlib.Path(args[5]).is_file()
    record('census-capture')
    pathlib.Path(args[3]).write_text(json.dumps({'version': 1, 'transaction': '00000000-0000-4000-8000-000000000001',
                                               'source': None, 'terminals': [], 'chats': [],
                                               'checkpoint_before': os.environ.get('FIXTURE_CHECKPOINT_BEFORE')}, separators=(',', ':')))
elif args[:2] == ['daemon', 'inspect-pending-update-recovery']:
    assert args[2] == '--candidate-cli' and len(args) in (4, 6)
    if len(args) == 6:
        assert args[4] == '--census'
    record('pending-inspect', executable=str(pathlib.Path(sys.argv[0]).resolve()), candidate=args[3])
    if os.environ.get('FIXTURE_PENDING_RECOVERY'):
        print(f"Run '{args[3]}' --no-start daemon recover-update --checkpoint " + 'a' * 64 + ' --yes', file=sys.stderr)
        sys.exit(1)
elif args[:2] in (['daemon', 'verify-update-census'], ['daemon', 'inspect-update-recovery']):
    import hashlib
    assert args[2] == '--census' and args[4] == '--outcome'
    census_path, outcome_path = pathlib.Path(args[3]), pathlib.Path(args[5])
    census = json.loads(census_path.read_text())
    assert census == {'version': 1, 'transaction': '00000000-0000-4000-8000-000000000001', 'source': None, 'terminals': [], 'chats': [],
                      'checkpoint_before': os.environ.get('FIXTURE_CHECKPOINT_BEFORE')}
    assert not census_path.stat().st_mode & 0o077
    if args[1] == 'inspect-update-recovery':
        assert args[6] == '--candidate-cli' and len(args) == 8
        flags = set()
    else:
        flags = set(args[6:])
    assert flags <= {'--allow-absent', '--require-original', '--require-absent', '--validate-only'}
    ready = pathlib.Path(os.environ['FIXTURE_DAEMON_STATE']).exists()
    if '--require-absent' in flags:
        state, accepted = ('no_source', True) if not ready and census['checkpoint_before'] is None else ('unverified', False)
    elif args[1] == 'inspect-update-recovery':
        state, accepted = 'restart_permitted', True
    elif '--validate-only' in flags:
        state, accepted = 'unverified', True
    elif '--require-original' in flags:
        state, accepted = ('no_source', True) if not ready and '--allow-absent' in flags else ('unverified', False)
    elif ready and not os.environ.get('FIXTURE_CENSUS_VERIFICATION_FAILS'):
        state, accepted = 'usable', True
    elif '--allow-absent' in flags:
        state, accepted = 'no_source', True
    else:
        state, accepted = 'unverified', False
    record('census-inspect' if args[1] == 'inspect-update-recovery' else 'census-verify', state=state)
    outcome = {'state': state}
    if state == 'usable':
        outcome.update(serving=fixture_proof, terminals=0, chats=0)
    elif state == 'unverified':
        outcome.update(serving=fixture_proof if ready else None,
                       readiness='ready' if ready else None,
                       surviving_terminals=0, expected_terminals=0,
                       missing=[], unexpected=[], changed=[],
                       detail='fixture-only service authorization or readiness is unverified')
    outcome_path.write_text(json.dumps({'version': 1, 'transaction': census['transaction'],
        'census_digest': hashlib.sha256(census_path.read_bytes()).hexdigest(),
        'outcome': outcome, 'checkpoint_before': census['checkpoint_before']}))
    if not accepted:
        print('Terminal readiness is unverified; inspect --no-start daemon status and --no-start terminal list.', file=sys.stderr)
        sys.exit(1)
elif args == ['--json', 'daemon', 'status']:
    record('status')
    if 'FIXTURE_STATUS_REPLY' in os.environ:
        print(os.environ['FIXTURE_STATUS_REPLY'])
        sys.exit(0)
    rewrite = os.environ.get('FIXTURE_STATUS_REWRITES')
    rewritten = log.with_suffix('.status-rewritten')
    if rewrite and not rewritten.exists():
        # One independent write after the check, not a mutating status probe
        # on every cleanup observation of that same path.
        path = pathlib.Path(rewrite)
        path.unlink()
        path.write_text('written during the installation\n')
        rewritten.write_text('done\n')
    print(json.dumps({'ok': True, 'command': 'daemon.status', 'data': {'running': pathlib.Path(os.environ['FIXTURE_DAEMON_STATE']).exists()}},
                     separators=(',', ':')))
elif args == ['daemon', 'stop', '--require-empty']:
    record('daemon-stop')
    pathlib.Path(os.environ['FIXTURE_DAEMON_STATE']).unlink()
    if os.environ.get('FIXTURE_UPDATE_DURING_STOP'):
        # An in-app update starting now takes its job lock as update.rs does.
        import fcntl
        state = pathlib.Path(os.environ['FIXTURE_UPDATE_DURING_STOP']) / '.update'
        try:
            state.mkdir(mode=0o700)
        except FileExistsError:
            pass
        lock = os.open(state / 'lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            record('update-started')
        except BlockingIOError:
            record('update-busy')
elif args == ['daemon', 'start']:
    record('daemon-start-fixture', executable=str(pathlib.Path(sys.argv[0]).resolve()))
    starts = sum(json.loads(line)['event'] == 'daemon-start-fixture' for line in log.read_text().splitlines())
    if os.environ.get('FIXTURE_DAEMON_START_FAILS') and (not os.environ.get('FIXTURE_FAIL_FIRST_DAEMON_START') or starts == 1):
        sys.exit(1)
    pathlib.Path(os.environ['FIXTURE_DAEMON_STATE']).write_text('ready\n')
    if os.environ.get('FIXTURE_START_NONZERO_AFTER_READY'):
        sys.exit(1)
elif args[:2] in (['daemon', 'store-schema'], ['daemon', 'downgrade-store']) \
        and not os.environ.get('FIXTURE_CLI_PREDATES_STORE_COMMANDS'):
    if args[2:] == ['--help']:
        print('Usage: cagents daemon ' + args[1])
    else:
        # The real command runs its matched sibling daemon's store option.
        helper = pathlib.Path(sys.argv[0]).resolve().with_name('cagentsd')
        option = ['--stored-schema'] if args[1] == 'store-schema' else ['--downgrade-store', *args[2:]]
        completed = subprocess.run([str(helper), *option], capture_output=True, text=True)
        sys.stdout.write(completed.stdout)
        sys.exit(completed.returncode)
else:
    # An older command line rejects a subcommand it does not know.
    raise AssertionError(args)
'''

INSPECTOR = r'''
import json, os, pathlib, sys
args = sys.argv[1:]
def record(event, **fields):
    with pathlib.Path(os.environ['FIXTURE_CALLS']).open('a') as output:
        output.write(json.dumps(dict(event=event, source=SOURCE, **fields)) + '\n')
if args == ['--stored-schema']:
    record('stored-schema')
    if os.environ.get('FIXTURE_STORED_SCHEMA_FAILS'):
        sys.exit(1)
    # Empty when there is no store yet.
    print(os.environ.get('FIXTURE_STORED_SCHEMA', '22'))
elif args[:1] == ['--downgrade-store']:
    record('downgrade-store', target=args[1])
    if os.environ.get('FIXTURE_DOWNGRADE_FAILS'):
        sys.exit(1)
elif args == ['--inspect-upgrade-source', 'fixture']:
    record('inspect')
    print(json.dumps({'Ok': {'stopped': [], 'stopped_windows': [], 'labels': {}}}))
else:
    # A release before the converter would start a daemon here.
    record('unexpected', args=args)
    sys.exit(2)
'''


@unittest.skipUnless(
    platform.system() == "Linux" and platform.machine() == "x86_64" and os.getuid() != 0,
    "the installer requires an ordinary Linux x86_64 user",
)
class InstallerCoordinator(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-installer-coordinator-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.home = self.directory / "home with spaces"
        self.home.mkdir()
        self.runtime = self.directory / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.downloads = self.directory / "downloads"
        self.downloads.mkdir()
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.prefix = self.directory / "prefix with spaces"
        self.calls = self.directory / "calls.jsonl"
        # No credentials, caller authority or live application paths cross
        # into the subprocess. Any accidental network access fails the fixture.
        self.environment = {
            "PATH": str(self.tools) + os.pathsep + os.defpath,
            "HOME": str(self.home),
            "XDG_DATA_HOME": str(self.home / "data"),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_RUNTIME_DIR": str(self.runtime),
            "TMPDIR": str(self.downloads),
            "FIXTURE_CALLS": str(self.calls),
            "FIXTURE_DAEMON_STATE": str(self.directory / "daemon-ready"),
            "FIXTURE_SYNC_CALL": str(self.directory / "sync-call"),
        }
        self.executable(self.tools / "getconf", '#!/bin/sh\nprintf "glibc 2.99\\n"\n')
        self.executable(self.tools / "sync", '#!/bin/sh\nprintf "%s\\n" "$*" > "$FIXTURE_SYNC_CALL"\n'
                        '[ -z "${FIXTURE_SYNC_FAIL:-}" ] || exit 7\nexec /usr/bin/sync "$@"\n')
        for name in ("curl", "wget", "gh"):
            self.executable(self.tools / name, '#!/bin/sh\nexit 97\n')
        self.executable(self.tools / "cagentsd", '#!/bin/sh\nexit 96\n')
        self.payload = self.directory / TOP
        self.python(self.payload / "bin/cagents", COORDINATOR)
        self.python(self.payload / "bin/cagentsd", "SOURCE = 'candidate'\n" + INSPECTOR)
        self.executable(self.payload / "bin/cagents-ui", '#!/bin/sh\nexit 98\n')
        self.executable(self.payload / "bin/cagents-dictate", '#!/bin/sh\nexit 95\n')
        # The installer, not the archive, decides the helper's mode.
        (self.payload / "bin/cagents-dictate").chmod(0o700)
        self.executable(self.payload / "bin/cagents-uninstall", (ROOT / "packaging/cagents-uninstall").read_text())
        self.write(self.payload / "bin/.cagents-ui-lease-v1", "1\n")
        for name in ("applications/dev.cagents.Cagents.desktop",
                     "autostart/dev.cagents.Cagents.Daemon.desktop",
                     "icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
                     "icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
                     "metainfo/dev.cagents.Cagents.metainfo.xml", "doc/cagents/LICENSE",
                     "doc/cagents/THIRD-PARTY-NOTICES"):
            source = "LICENSE" if name == "doc/cagents/LICENSE" else "packaging/" + Path(name).name
            path = self.payload / "share" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / source, path)
        self.archive = self.directory / ARCHIVE_NAME
        self.checksums = self.directory / "SHA256SUMS"
        self.package()
        self.environment.update(CAGENTS_ARCHIVE=str(self.archive), CAGENTS_CHECKSUMS=str(self.checksums))

    def package(self, *, exclude=()):
        excluded = {f"{TOP}/{member}" for member in ((exclude,) if isinstance(exclude, str) else exclude)}
        with tarfile.open(self.archive, "w:gz") as archive:
            archive.add(self.payload, arcname=TOP,
                        filter=lambda entry: None if entry.name in excluded else entry)
        self.checksums.write_text(f"{hashlib.sha256(self.archive.read_bytes()).hexdigest()}  {ARCHIVE_NAME}\n")

    @staticmethod
    def write(path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def executable(self, path, text):
        self.write(path, text)
        path.chmod(0o755)

    def python(self, path, body):
        self.executable(path, f"#!{sys.executable}\n" + body)

    def install(self, *options):
        command = ["/bin/sh", str(ROOT / "install.sh"), "--version", VERSION, "--prefix", str(self.prefix),
                   *options]
        with subprocess.Popen(command, env=self.environment, cwd=self.directory,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, start_new_session=True) as process:
            try:
                stdout, stderr = process.communicate(timeout=INSTALL_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                # The fixture coordinator and inner installer are in this
                # private process group. A deadline must not orphan them.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                stdout, stderr = process.communicate()
                self.fail(f"installer fixture exceeded {INSTALL_TIMEOUT_SECONDS}s: {command!r}\n"
                          f"stdout: {stdout}\nstderr: {stderr}")
            return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)

    def events(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def installed_cli(self, *, guarded):
        previous = self.prefix / "versions/old/bin"
        previous.mkdir(parents=True)
        self.write(self.prefix / ".cagents-install-root", "cagents install root\n")
        (self.prefix / "current").symlink_to("versions/old")
        if guarded:
            self.python(previous / "cagents", COORDINATOR)
            self.python(previous / "cagentsd", "SOURCE = 'installed'\n" + INSPECTOR)
        else:
            self.executable(previous / "cagents", '#!/bin/sh\nexit 64\n')
        return previous

    def assert_installed(self, result, source):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"Installed cagents {VERSION}", result.stdout)
        self.assertEqual(os.readlink(self.prefix / "current"), f"versions/{VERSION}")
        installed = self.prefix / "current/bin"
        for name in ("cagents", "cagentsd", "cagents-ui"):
            self.assertEqual((installed / name).read_bytes(), (self.payload / "bin" / name).read_bytes())
        if (self.payload / "bin/cagents-dictate").exists():
            helper = installed / "cagents-dictate"
            self.assertEqual(helper.read_bytes(), (self.payload / "bin/cagents-dictate").read_bytes())
            self.assertEqual(stat.S_IMODE(helper.stat().st_mode), 0o755)
        self.assertEqual(os.readlink(self.home / ".local/bin/cagents"), str(installed / "cagents"))
        events = self.events()
        self.assertEqual([e['source'] for e in events if e['event'] == 'inspect'], [source])
        self.assertEqual(sum(e['event'] == 'verify' for e in events), 1)
        self.assertEqual(sum(e['event'] == 'daemon-start-fixture' for e in events), 1)
        self.assertEqual(list(self.downloads.iterdir()), [], "downloaded coordinator pair was not cleaned")
        return next(e for e in events if e['event'] == 'coordinate')

    def test_install_prunes_only_unneeded_release_trees(self):
        versions = self.prefix / 'versions'
        names = ('9.9.0', '9.9.1', '9.9.2', '9.9.3', '9.9.4', '9.9.5+local.123', '9.9.6', '9.9.7')
        for name in names:
            shutil.copytree(self.payload, versions / name)
        self.write(self.prefix / '.cagents-install-root', 'cagents install root\n')
        (self.prefix / 'current').symlink_to('versions/9.9.1')
        (versions / '9.9.8').symlink_to('9.9.1')
        self.write(versions / '9.9.3' / 'notes.txt', 'keep my notes\n')
        self.write(versions / '9.9.4' / '.cagents-prepared-candidate-v1.json', '{}\n')
        old_marker = versions / '9.9.6' / '.cagents-retained-candidate-v1.json'
        self.write(old_marker, '{}\n')
        expired = time.time() - 15 * 86400
        os.utime(old_marker, (expired, expired))
        self.write(versions / '9.9.7' / '.deferred-owner.json', '{}\n')

        # Execute the binary itself so the kernel names the installed tree.
        live = versions / '9.9.2/bin/cagentsd'
        shutil.copy2(shutil.which('sleep'), live)
        with subprocess.Popen([live, '60']) as process:
            try:
                self.assertEqual(os.readlink(f'/proc/{process.pid}/exe'), str(live))
                self.assert_installed(self.install(), 'candidate')
            finally:
                process.terminate()

        self.assertEqual({path.name for path in versions.iterdir()},
                         {VERSION, '9.9.1', '9.9.2', '9.9.3', '9.9.4', '9.9.8'})
        self.assertEqual((versions / '9.9.3/notes.txt').read_text(), 'keep my notes\n')

    def test_staging_prunes_obsolete_versions_only_after_publication(self):
        self.installed_cli(guarded=True)
        obsolete = self.prefix / 'versions/9.9.0'
        shutil.copytree(self.payload, obsolete)
        self.environment['FIXTURE_PENDING_RECOVERY'] = '1'
        failed = self.install('--stage-only')
        self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
        self.assertTrue(obsolete.is_dir())

        del self.environment['FIXTURE_PENDING_RECOVERY']
        staged = self.install('--stage-only')
        self.assertEqual(staged.returncode, 0, staged.stdout + staged.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        self.assertTrue((self.prefix / 'versions/old').is_dir())
        self.assertFalse(obsolete.exists())

    def test_install_defers_pruning_when_a_live_cagents_executable_cannot_be_read(self):
        self.installed_cli(guarded=True)
        obsolete = self.prefix / 'versions/9.9.0'
        shutil.copytree(self.payload, obsolete)
        hidden = obsolete / 'bin/cagentsd'
        shutil.copy2(sys.executable, hidden)
        program = '''
import ctypes, time
assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0  # PR_SET_DUMPABLE
print('ready', flush=True)
time.sleep(60)
'''
        with subprocess.Popen([hidden, '-c', program], stdout=subprocess.PIPE, text=True) as process:
            try:
                self.assertEqual(process.stdout.readline(), 'ready\n')
                with self.assertRaises(PermissionError):
                    os.readlink(f'/proc/{process.pid}/exe')
                self.assert_installed(self.install(), 'installed')
                self.assertTrue(obsolete.is_dir())
            finally:
                process.terminate()

    def test_install_defers_pruning_when_version_tree_contains_a_mount(self):
        self.installed_cli(guarded=True)
        obsolete = self.prefix / 'versions/9.9.0'
        shutil.copytree(self.payload, obsolete)
        mountinfo = self.directory / 'mountinfo'
        mounted = str(obsolete / 'share').replace(' ', r'\040')
        mountinfo.write_text(f'42 41 8:1 /other/share {mounted} rw - ext4 /dev/sda1 rw\n')
        self.environment['FIXTURE_MOUNTINFO'] = str(mountinfo)
        self.executable(self.tools / 'awk', '''#!/bin/bash
if [[ ${!#} == /proc/self/mountinfo ]]; then
    set -- "${@:1:$#-1}" "$FIXTURE_MOUNTINFO"
fi
exec /usr/bin/awk "$@"
''')

        self.assert_installed(self.install(), 'installed')
        self.assertTrue(obsolete.is_dir())

    def test_verifying_the_current_candidate_preserves_its_rollback_version(self):
        self.prepare_candidate()
        # Older installers could leave the prepared marker on a published tree.
        current = self.prefix / 'current'
        current.unlink()
        current.symlink_to(f'versions/{VERSION}')
        rollback = self.prefix / 'versions/9.9.0'
        shutil.copytree(self.payload, rollback)
        del self.environment['FIXTURE_WINDOWS_PREPARED']

        result = self.install('--prepared-candidate', JOB)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f'Installed cagents {VERSION}', result.stdout)
        self.assertFalse((current / '.cagents-prepared-candidate-v1.json').exists())
        self.assertTrue(rollback.is_dir())

    def test_fresh_prefix_uses_the_verified_candidate_pair(self):
        self.assertFalse(self.prefix.exists())
        coordinator = self.assert_installed(self.install(), 'candidate')
        self.assertEqual(Path(coordinator['executable']).parent, Path(coordinator['helper']).parent)
        self.assertEqual(Path(coordinator['helper']).name, 'cagentsd')
        self.assertEqual(coordinator['mode'], 0o700)

    def test_old_cli_without_install_guard_uses_the_verified_candidate_pair(self):
        previous = self.installed_cli(guarded=False)
        coordinator = self.assert_installed(self.install(), 'candidate')
        self.assertNotEqual(Path(coordinator['executable']).parent, previous)
        self.assertEqual((previous / "cagents").read_text(), '#!/bin/sh\nexit 64\n')

    def test_installed_coordinator_keeps_its_own_source_inspector(self):
        previous = self.installed_cli(guarded=True)
        coordinator = self.assert_installed(self.install(), 'installed')
        self.assertEqual(Path(coordinator['executable']), previous / "cagents")
        self.assertEqual(Path(coordinator['helper']), previous / "cagentsd")

    def test_source_inspector_extraction_failure_stops_before_publication(self):
        real_tar = shutil.which("tar")
        self.python(self.tools / "tar", f'''
import os, sys
args = sys.argv[1:]
if args[0] == '-xOzf' and args[-1] == '{TOP}/bin/cagentsd':
    sys.exit(23)
os.execv({real_tar!r}, [{real_tar!r}, *args])
''')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('could not extract the verified source inspector', result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertFalse(self.calls.exists())
        self.assertEqual(list(self.downloads.iterdir()), [])

    def test_bad_checksum_rejects_the_pair_before_either_executable_runs(self):
        with self.archive.open('ab') as archive:
            archive.write(b'tampered')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('archive SHA-256 verification failed', result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertFalse(self.calls.exists())
        self.assertEqual(list(self.downloads.iterdir()), [])

    def assert_missing_member_rejected(self, member):
        # Recompute the checksum so this reaches the archive completeness gate.
        self.package(exclude=member)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('archive has an unexpected or incomplete entry structure', result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertFalse(self.calls.exists(), 'an incomplete archive executed its coordinator')
        self.assertEqual(list(self.downloads.iterdir()), [])

    def test_archive_requires_the_ui_lease_marker_independently_of_autostart(self):
        self.assert_missing_member_rejected('bin/.cagents-ui-lease-v1')

    def test_archive_requires_autostart_independently_of_the_ui_lease_marker(self):
        self.assert_missing_member_rejected('share/autostart/dev.cagents.Cagents.Daemon.desktop')

    def test_archive_from_before_dictation_installs_without_the_helper(self):
        for member in ("bin/cagents-dictate", "share/doc/cagents/THIRD-PARTY-NOTICES"):
            (self.payload / member).unlink()
        self.package()
        self.assert_installed(self.install(), 'candidate')
        self.assertFalse((self.prefix / "current/bin/cagents-dictate").exists())

    def test_archive_ships_the_helper_only_with_its_notices(self):
        for member in ("bin/cagents-dictate", "share/doc/cagents/THIRD-PARTY-NOTICES"):
            with self.subTest(missing=member):
                self.assert_missing_member_rejected(member)

    def managed_link(self, member):
        if member == "bin/cagents":
            return self.home / ".local/bin/cagents"
        if member.startswith("share/autostart/"):
            return self.home / "config/autostart" / Path(member).name
        return self.home / "data" / member.removeprefix("share/")

    def place_copy(self, member, content):
        """A regular file where the installer keeps a link, as desktop tools leave it."""
        copy = self.managed_link(member)
        copy.parent.mkdir(parents=True, exist_ok=True)
        copy.write_bytes(content)
        return copy

    def assert_no_backups(self, copy):
        self.assertEqual(sorted(p.name for p in copy.parent.iterdir() if p.name != copy.name), [])

    def assert_adopted(self, result, member):
        copy = self.managed_link(member)
        self.assertEqual(os.readlink(copy), str(self.prefix / "current" / member))
        self.assertEqual(result.stderr.count("was an unchanged copy of the installed file"), 1,
                         result.stderr)
        self.assertIn(f"{copy} was an unchanged copy", result.stderr)
        self.assert_no_backups(copy)

    def test_copy_of_the_candidate_file_becomes_the_managed_link(self):
        member = "share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"
        self.place_copy(member, (self.payload / member).read_bytes())
        result = self.install()
        self.assert_installed(result, 'candidate')
        self.assert_adopted(result, member)

    def test_copy_of_the_current_version_file_becomes_the_managed_link(self):
        # The candidate's file differs: the copy matches the version it was made from.
        previous = self.installed_cli(guarded=True).parent
        member = "share/metainfo/dev.cagents.Cagents.metainfo.xml"
        self.write(previous / member, "<component>previous</component>\n")
        self.place_copy(member, (previous / member).read_bytes())
        result = self.install()
        self.assert_installed(result, 'installed')
        self.assert_adopted(result, member)

    def test_copy_of_the_generated_autostart_entry_becomes_the_managed_link(self):
        # The workstation case: the entry install.sh generated, copied over its link.
        previous = self.installed_cli(guarded=True).parent
        member = "share/autostart/dev.cagents.Cagents.Daemon.desktop"
        self.write(previous / member, (
            "[Desktop Entry]\nType=Application\nName=cagents daemon\n"
            "Comment=Keep cagents sessions available in the system tray\n"
            f'Exec="{self.prefix}/current/bin/cagents" daemon start\n'
            "Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"))
        self.place_copy(member, (previous / member).read_bytes())
        result = self.install()
        self.assert_installed(result, 'installed')
        self.assert_adopted(result, member)
        self.assertEqual(self.managed_link(member).read_bytes(), (previous / member).read_bytes())

    def test_changed_copy_is_refused_before_installation_changes(self):
        previous = self.installed_cli(guarded=True).parent
        for member in ("bin/cagents", "share/applications/dev.cagents.Cagents.desktop",
                       "share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
                       "share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
                       "share/metainfo/dev.cagents.Cagents.metainfo.xml",
                       "share/autostart/dev.cagents.Cagents.Daemon.desktop"):
            with self.subTest(member=member):
                self.write(previous / member, "previous\n")
                copy = self.place_copy(member, b"edited by the user\n")
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"refusing to replace existing file {copy}", result.stderr)
                self.assertEqual(copy.read_bytes(), b"edited by the user\n")
                self.assert_no_backups(copy)
                self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")
                self.assertFalse((self.prefix / "versions" / VERSION).exists())
                self.assertFalse(any(e['event'] == 'daemon-start-fixture' for e in self.events()))
                copy.unlink()

    def test_directory_at_a_link_path_is_still_refused(self):
        member = "share/autostart/dev.cagents.Cagents.Daemon.desktop"
        self.managed_link(member).mkdir(parents=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"refusing to replace existing file {self.managed_link(member)}", result.stderr)
        self.assertTrue(self.managed_link(member).is_dir())

    def test_failed_update_restores_the_adopted_copy(self):
        previous = self.installed_cli(guarded=True).parent
        member = "share/autostart/dev.cagents.Cagents.Daemon.desktop"
        self.write(previous / member, "[Desktop Entry]\nName=previous\n")
        copy = self.place_copy(member, (previous / member).read_bytes())
        before = copy.stat()
        self.environment["FIXTURE_DAEMON_START_FAILS"] = "1"
        self.environment["FIXTURE_CHECKPOINT_BEFORE"] = "a" * 64
        result = self.install("--restart-daemon")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("replacement daemon startup failed", result.stderr)
        self.assertIn(f"{copy} was an unchanged copy", result.stderr, "the copy was never replaced")
        self.assertIn(
            f"rolled back; restored the original file at {copy}",
            result.stderr,
            "the user was not told the earlier replacement was undone",
        )
        self.assertFalse(copy.is_symlink())
        self.assertEqual(copy.read_bytes(), (previous / member).read_bytes())
        self.assertEqual(copy.stat().st_ino, before.st_ino, "the original file was not the one restored")
        self.assert_no_backups(copy)
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")
        self.assertTrue((self.prefix / "versions" / VERSION).exists(),
                        "unverified service availability must retain its recovery target")

    def failed_update(self, **environment):
        self.installed_cli(guarded=True)
        # These store-conversion fixtures retain an already Ready checkpoint.
        # They cannot qualify for the fresh/empty installation exception.
        self.environment.update(FIXTURE_DAEMON_START_FAILS="1", FIXTURE_CHECKPOINT_BEFORE="a" * 64, **environment)
        result = self.install("--restart-daemon")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("replacement daemon startup failed", result.stderr)
        events = self.events()
        self.assertEqual(
            [e for e in events if e['event'] == 'unexpected'], [],
            "the installer ran a daemon with an option it may not know",
        )
        conversion = [(e['event'], e.get('source'), e.get('target')) for e in events
                      if e['event'] in ('stored-schema', 'downgrade-store', 'daemon-start-fixture')]
        return result, conversion

    def test_failed_update_converts_the_store_back_to_the_schema_it_had(self):
        result, conversion = self.failed_update(FIXTURE_STORED_SCHEMA="23")
        self.assertEqual(
            conversion,
            [("stored-schema", "candidate", None), ("daemon-start-fixture", None, None),
             ("downgrade-store", "candidate", "23")],
            "the store was not converted back to the schema recorded before the replacement started",
        )
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")
        self.assertTrue((self.prefix / "versions" / VERSION).exists(),
                        "unverified service availability must retain its recovery target")

    def test_failed_update_converts_a_store_the_replacement_created_back_to_the_oldest_schema(self):
        _, conversion = self.failed_update(FIXTURE_STORED_SCHEMA="")
        self.assertEqual(conversion[-1], ("downgrade-store", "candidate", "22"))
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")

    def test_store_that_cannot_be_converted_back_keeps_the_new_installation(self):
        result, _ = self.failed_update(FIXTURE_DOWNGRADE_FAILS="1")
        self.assertIn("could not convert the shared-model store back to schema 22", result.stderr)
        self.assertEqual(os.readlink(self.prefix / "current"), f"versions/{VERSION}")
        self.assertTrue((self.prefix / "versions" / VERSION / "bin/cagentsd").exists())

    def test_replacement_without_store_commands_is_installed_without_running_its_daemon(self):
        # Releases before the store commands: their daemon would start serving
        # when given an option, so only their command line may be asked.
        self.installed_cli(guarded=True)
        self.environment.update(FIXTURE_CLI_PREDATES_STORE_COMMANDS="1")
        result = self.install("--restart-daemon")
        self.assert_installed(result, "installed")
        events = [e['event'] for e in self.events()]
        self.assertNotIn("unexpected", events)
        self.assertNotIn("stored-schema", events)

    def test_failed_update_to_a_replacement_without_store_commands_converts_nothing_back(self):
        result, conversion = self.failed_update(FIXTURE_CLI_PREDATES_STORE_COMMANDS="1")
        self.assertEqual(conversion, [("daemon-start-fixture", None, None)])
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")
        self.assertTrue((self.prefix / "versions" / VERSION).exists(),
                        "unverified service availability must retain its recovery target")

    def test_unreadable_store_schema_leaves_the_installation_unchanged(self):
        self.installed_cli(guarded=True)
        self.environment.update(FIXTURE_STORED_SCHEMA_FAILS="1")
        result = self.install("--restart-daemon")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot read the shared-model store schema", result.stderr)
        events = self.events()
        self.assertNotIn("daemon-start-fixture", [e['event'] for e in events])
        self.assertNotIn("downgrade-store", [e['event'] for e in events])
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")
        self.assertTrue((self.prefix / "versions" / VERSION).exists(),
                        "unverified service availability must retain its recovery target")

    def test_copy_rewritten_after_the_check_is_kept_and_refused(self):
        previous = self.installed_cli(guarded=True).parent
        member = "share/autostart/dev.cagents.Cagents.Daemon.desktop"
        self.write(previous / member, "[Desktop Entry]\nName=previous\n")
        copy = self.place_copy(member, (previous / member).read_bytes())
        self.environment["FIXTURE_STATUS_REWRITES"] = str(copy)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"refusing to replace existing file {copy}", result.stderr)
        self.assertEqual(copy.read_text(), "written during the installation\n")
        self.assert_no_backups(copy)
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/old")

    def assert_files_installed_for_login(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(os.readlink(self.prefix / "current"), f"versions/{VERSION}")
        self.assertTrue((self.prefix / "versions" / VERSION / "bin/cagents").exists())
        self.assertTrue((self.home / "config/autostart/dev.cagents.Cagents.Daemon.desktop").is_symlink())
        self.assertIn("will start at graphical login", result.stdout)
        self.assertNotIn("your sessions are ready", result.stdout)
        self.assertIn("daemon start command failed", result.stderr)
        self.assertFalse(Path(self.environment["FIXTURE_DAEMON_STATE"]).exists())
        proofs = [e['state'] for e in self.events() if e['event'] == 'census-verify']
        self.assertIn('no_source', proofs)
        self.assertNotIn('usable', proofs, 'failed first startup is not verified readiness')

    def test_fresh_install_keeps_files_and_login_autostart_when_daemon_start_fails(self):
        self.environment["FIXTURE_DAEMON_START_FAILS"] = "1"
        self.assert_files_installed_for_login(self.install())

    def test_absent_source_install_keeps_files_and_login_autostart_when_daemon_start_fails(self):
        self.installed_cli(guarded=True)
        self.environment["FIXTURE_DAEMON_START_FAILS"] = "1"
        self.assert_files_installed_for_login(self.install())

    def test_explicit_restart_without_a_source_keeps_files_and_login_autostart_after_failed_start(self):
        self.environment['FIXTURE_DAEMON_START_FAILS'] = '1'
        self.assert_files_installed_for_login(self.install('--restart-daemon'))

    def test_downgrade_refusal_starts_the_selected_candidate_and_verifies_ready(self):
        result, conversion = self.failed_update(FIXTURE_DOWNGRADE_FAILS='1', FIXTURE_FAIL_FIRST_DAEMON_START='1')
        self.assertEqual(conversion[-2:], [('downgrade-store', 'candidate', '22'), ('daemon-start-fixture', None, None)])
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        self.assertTrue(Path(self.environment['FIXTURE_DAEMON_STATE']).exists())
        receipt_paths = list(self.downloads.glob('*/update-outcome.json'))
        self.assertEqual(len(receipt_paths), 1)
        self.assertEqual(json.loads(receipt_paths[0].read_text())['outcome']['state'], 'usable')

    def test_downgrade_refusal_with_failed_restart_prints_the_exact_selected_start_command(self):
        result, _ = self.failed_update(FIXTURE_DOWNGRADE_FAILS='1')
        command = f"'{self.prefix / 'versions' / VERSION / 'bin/cagents'}' daemon start"
        self.assertIn(command, result.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        self.assertFalse(Path(self.environment['FIXTURE_DAEMON_STATE']).exists())

    def test_downgrade_refusal_with_started_unverified_service_gives_inspection_without_another_start(self):
        result, _ = self.failed_update(FIXTURE_DOWNGRADE_FAILS='1', FIXTURE_FAIL_FIRST_DAEMON_START='1',
                                       FIXTURE_CENSUS_VERIFICATION_FAILS='1')
        cli = self.prefix / 'versions' / VERSION / 'bin/cagents'
        self.assertIn('daemon start command completed, but readiness and the original inventory remain unverified', result.stderr)
        self.assertIn(f"'{cli}' --no-start daemon status", result.stderr)
        self.assertIn(f"'{cli}' --no-start terminal list", result.stderr)
        self.assertNotIn('Start it with', result.stderr)
        self.assertNotIn(f"'{cli}' daemon start", result.stderr)
        starts = [e['executable'] for e in self.events() if e['event'] == 'daemon-start-fixture']
        self.assertEqual(starts, [str(cli), str(cli)], 'a completed target start must not be retried or replaced')
        self.assertTrue(Path(self.environment['FIXTURE_DAEMON_STATE']).exists())
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        paths = list(self.downloads.glob('*/update-outcome.json'))
        self.assertEqual(len(paths), 1)
        receipt = json.loads(paths[0].read_text())['outcome']
        self.assertEqual(receipt['state'], 'unverified')
        self.assertEqual(receipt['serving']['host_session_id'], 'fixture-daemon')
        self.assertEqual(receipt['readiness'], 'ready')

    def test_downgrade_refusal_with_nonzero_start_but_serving_unverified_service_is_not_restarted(self):
        result, _ = self.failed_update(FIXTURE_DOWNGRADE_FAILS='1', FIXTURE_FAIL_FIRST_DAEMON_START='1',
                                       FIXTURE_CENSUS_VERIFICATION_FAILS='1', FIXTURE_START_NONZERO_AFTER_READY='1')
        cli = self.prefix / 'versions' / VERSION / 'bin/cagents'
        self.assertIn('daemon start command failed, but a stopped service was not proven', result.stderr)
        self.assertIn(f"'{cli}' --no-start daemon status", result.stderr)
        self.assertIn(f"'{cli}' --no-start terminal list", result.stderr)
        self.assertNotIn('Start it with', result.stderr)
        self.assertNotIn(f"'{cli}' daemon start", result.stderr)
        self.assertEqual([e['executable'] for e in self.events() if e['event'] == 'daemon-start-fixture'],
                         [str(cli), str(cli)])
        self.assertTrue(Path(self.environment['FIXTURE_DAEMON_STATE']).exists())
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        receipts = list(self.downloads.glob('*/update-outcome.json'))
        self.assertEqual(len(receipts), 1)
        self.assertEqual(json.loads(receipts[0].read_text())['outcome']['state'], 'unverified')

    def test_same_version_pending_recovery_is_reported_before_already_installed_or_capture(self):
        first = self.install()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.calls.unlink()
        self.environment.update(CAGENTS_UI_UPDATE_GUARD_FD='fixture-only', FIXTURE_PENDING_RECOVERY='1')
        retry = self.install('--restart-daemon')
        self.assertNotEqual(retry.returncode, 0)
        self.assertIn(f"'{self.prefix / 'versions' / VERSION / 'bin/cagents'}' --no-start daemon recover-update", retry.stderr)
        self.assertNotIn('already installed', retry.stderr)
        self.assertEqual([e['event'] for e in self.events()], ['verify', 'pending-inspect'])

    def test_retained_failed_candidate_can_resume_the_same_version_after_revalidation(self):
        self.failed_update(FIXTURE_DOWNGRADE_FAILS='1')
        candidate = self.prefix / 'versions' / VERSION
        inode = (candidate / 'bin/cagents').stat().st_ino
        marker = candidate / '.cagents-retained-candidate-v1.json'
        self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(marker.read_text()), {'version': 1, 'archive_sha256': hashlib.sha256(self.archive.read_bytes()).hexdigest()})
        self.calls.unlink()
        self.environment.pop('FIXTURE_DAEMON_START_FAILS')
        self.environment.pop('FIXTURE_DOWNGRADE_FAILS')
        self.environment['CAGENTS_UI_UPDATE_GUARD_FD'] = 'fixture-only'
        retried = self.install('--restart-daemon')
        self.assertEqual(retried.returncode, 0, retried.stdout + retried.stderr)
        self.assertEqual((candidate / 'bin/cagents').stat().st_ino, inode)
        self.assertTrue(Path(self.environment['FIXTURE_DAEMON_STATE']).exists())
        self.assertFalse(marker.exists(), 'a verified completed install clears failure bookkeeping')

    def test_standalone_retry_can_resume_a_verified_prepared_candidate(self):
        candidate = self.prepare_candidate()
        inode = (candidate / 'bin/cagents').stat().st_ino
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        result = self.install('--restart-daemon')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((candidate / 'bin/cagents').stat().st_ino, inode)
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')

    def test_pending_recovery_precedes_repacked_archive_marker_mismatch(self):
        self.failed_update(FIXTURE_DOWNGRADE_FAILS='1')
        # Repack identical bytes with different tar metadata, as install-local
        # may do. The existing executable tree still matches the new archive.
        item = self.payload / 'bin/cagents'
        os.utime(item, (item.stat().st_atime, item.stat().st_mtime + 5))
        self.package()
        self.calls.unlink()
        self.environment.update(CAGENTS_UI_UPDATE_GUARD_FD='fixture-only', FIXTURE_PENDING_RECOVERY='1')
        retry = self.install('--restart-daemon')
        self.assertNotEqual(retry.returncode, 0)
        self.assertIn('daemon recover-update --checkpoint', retry.stderr)
        self.assertNotIn('archive differs', retry.stderr)
        self.assertEqual([e['event'] for e in self.events()], ['verify', 'pending-inspect'])

    def test_pending_recovery_precedes_existing_tree_errors_without_executing_that_tree(self):
        candidate = self.prepare_candidate()
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        extra = candidate / '.older-retained-layout'
        extra.write_text('retained older-layout evidence\n')
        forbidden_execution = self.directory / 'untrusted-candidate-executed'
        self.environment.update(FIXTURE_FORBIDDEN_CLI=str(candidate / 'bin/cagents'),
                                FIXTURE_FORBIDDEN_EXECUTION=str(forbidden_execution),
                                FIXTURE_PENDING_RECOVERY='1')
        before = {str(p.relative_to(candidate)): (p.read_bytes(), p.stat().st_mode & 0o777)
                  for p in candidate.rglob('*') if p.is_file()}
        for selected in ('versions/old', f'versions/{VERSION}'):
            with self.subTest(current=selected):
                (self.prefix / 'current').unlink()
                (self.prefix / 'current').symlink_to(selected)
                pending = self.install('--restart-daemon')
                self.assertNotEqual(pending.returncode, 0)
                self.assertIn(f"'{candidate / 'bin/cagents'}' --no-start daemon recover-update", pending.stderr)
                self.assertNotIn('unexpected entries', pending.stderr)
                self.assertFalse(forbidden_execution.exists(), 'inspection must execute only the authenticated archive CLI')
                events = self.events()
                self.assertEqual([e['event'] for e in events], ['verify', 'pending-inspect'])
                inspector = Path(events[1]['executable'])
                self.assertTrue(inspector.is_relative_to(self.downloads), inspector)
                self.assertEqual(events[1]['candidate'], str(candidate / 'bin/cagents'))
                self.assertEqual(os.readlink(self.prefix / 'current'), selected)
                after = {str(p.relative_to(candidate)): (p.read_bytes(), p.stat().st_mode & 0o777)
                         for p in candidate.rglob('*') if p.is_file()}
                self.assertEqual(after, before, 'pending inspection may not modify or replace retained evidence')
                self.calls.unlink()
        self.environment.pop('FIXTURE_PENDING_RECOVERY')
        rejected = self.install('--restart-daemon')
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('unexpected entries', rejected.stderr, 'no-pending retries still require complete tree validation')
        self.assertEqual([e['event'] for e in self.events()], ['verify', 'pending-inspect'])
        self.assertFalse(forbidden_execution.exists())
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')

    def prepare_candidate(self, *, serving=False):
        self.installed_cli(guarded=True)
        self.environment['CAGENTS_UI_UPDATE_GUARD_FD'] = 'fixture-only'
        self.environment['FIXTURE_WINDOWS_PREPARED'] = '1'
        source_state = Path(self.environment['FIXTURE_DAEMON_STATE'])
        if serving:
            source_state.write_text('original service remains available\n')
        result = self.install('--prepare-candidate', JOB)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Prepared cagents', result.stdout)
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertEqual([e['event'] for e in self.events()], ['verify'])
        self.assertTrue(self.events()[0]['stage_only'], 'preparation must allow prepared live windows')
        self.assertFalse((self.home / '.local/bin/cagents').exists())
        self.assertFalse((self.home / 'config/autostart').exists())
        if serving:
            self.assertEqual(source_state.read_text(), 'original service remains available\n')
        else:
            self.assertFalse(source_state.exists())
        self.assertEqual(list(self.downloads.iterdir()), [])
        self.assertEqual(Path(self.environment['FIXTURE_SYNC_CALL']).read_text(),
                         f'-f {self.prefix / "versions"}\n')
        candidate = self.prefix / 'versions' / VERSION
        marker = candidate / '.cagents-prepared-candidate-v1.json'
        self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(marker.read_text()), {
            'version': 1, 'job': JOB,
            'archive_sha256': hashlib.sha256(self.archive.read_bytes()).hexdigest(),
        })
        self.calls.unlink()
        return candidate

    def test_candidate_capability_probe_has_no_installation_side_effects(self):
        result = subprocess.run(
            ['/bin/sh', str(ROOT / 'install.sh'), '--supports-prepared-candidate'],
            env=self.environment, capture_output=True, text=True, timeout=INSTALL_TIMEOUT_SECONDS,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertFalse(self.calls.exists())
        self.assertEqual(list(self.downloads.iterdir()), [])

    def test_prepared_candidate_is_reused_only_by_its_explicit_job(self):
        candidate = self.prepare_candidate()
        original_inode = (candidate / 'bin/cagents').stat().st_ino
        refused = self.install('--prepared-candidate', JOB)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('application windows have not finished closing', refused.stderr)
        self.assertFalse(self.events()[0]['stage_only'], 'final commit still requires closed windows')
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertTrue(candidate.exists())
        self.calls.unlink()
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        result = self.install('--prepared-candidate', JOB)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        self.assertEqual((candidate / 'bin/cagents').stat().st_ino, original_inode)
        self.assertEqual(sum(e['event'] == 'daemon-start-fixture' for e in self.events()), 1)
        self.assertFalse(
            (candidate / '.cagents-prepared-candidate-v1.json').exists(),
            'a published candidate is an ordinary installed tree',
        )

    def test_prepared_candidate_rejects_wrong_job_and_tree_tampering_before_source_calls(self):
        candidate = self.prepare_candidate()
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        marker = candidate / '.cagents-prepared-candidate-v1.json'
        original_marker = marker.read_bytes()
        executable = candidate / 'bin/cagents'
        original = executable.read_bytes()
        for mutation in ('job', 'contents', 'permissions', 'extra', 'newline-extra', 'marker-mode', 'archive-marker'):
            with self.subTest(mutation=mutation):
                job = '00000000-0000-4000-8000-000000000002' if mutation == 'job' else JOB
                if mutation == 'contents':
                    executable.write_bytes(original + b'\n# changed\n')
                elif mutation == 'permissions':
                    executable.chmod(0o700)
                elif mutation == 'extra':
                    (candidate / 'unexpected').write_text('extra\n')
                elif mutation == 'newline-extra':
                    (candidate / 'bin\nshare').write_text('extra\n')
                elif mutation == 'marker-mode':
                    marker.chmod(0o644)
                elif mutation == 'archive-marker':
                    marker.write_text(json.dumps({'version': 1, 'job': JOB, 'archive_sha256': '0' * 64},
                                                 separators=(',', ':')) + '\n')
                result = self.install('--prepared-candidate', job)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
                self.assertTrue(candidate.exists(), 'a prepared target must survive failure')
                self.assertEqual([e['event'] for e in self.events()], ['verify', 'pending-inspect'])
                self.calls.unlink()
                executable.write_bytes(original)
                executable.chmod(0o755)
                marker.write_bytes(original_marker)
                marker.chmod(0o600)
                (candidate / 'unexpected').unlink(missing_ok=True)
                (candidate / 'bin\nshare').unlink(missing_ok=True)

    def test_unmarked_existing_candidate_cannot_be_claimed_by_an_ordinary_install(self):
        candidate = self.prepare_candidate()
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        (candidate / '.cagents-prepared-candidate-v1.json').unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('already installed', result.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertTrue(candidate.exists())
        unmarked = self.install('--prepare-candidate', JOB)
        self.assertNotEqual(unmarked.returncode, 0)
        self.assertIn('does not match this private job', unmarked.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertTrue(candidate.exists(), 'an unmarked existing tree is never claimed or deleted')

    def test_candidate_preparation_requires_the_worker_guard_before_installation(self):
        result = self.install('--prepare-candidate', JOB)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('requires the worker application update fence', result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertFalse(self.calls.exists())

    def test_candidate_preparation_keeps_a_serving_source_and_prepared_windows_untouched(self):
        self.prepare_candidate(serving=True)

    def test_candidate_publication_flush_failure_does_not_prepare_or_stop_the_source(self):
        self.installed_cli(guarded=True)
        self.environment.update(CAGENTS_UI_UPDATE_GUARD_FD='fixture-only',
                                FIXTURE_WINDOWS_PREPARED='1', FIXTURE_SYNC_FAIL='1')
        state = Path(self.environment['FIXTURE_DAEMON_STATE'])
        state.write_text('original remains available\n')
        result = self.install('--prepare-candidate', JOB)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('could not durably publish', result.stderr)
        self.assertEqual(state.read_text(), 'original remains available\n')
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertFalse((self.prefix / 'versions' / VERSION).exists())
        self.assertEqual([e['event'] for e in self.events()], ['verify'])

    def test_cancelled_preparation_can_be_adopted_by_a_new_guarded_job_for_the_same_release(self):
        candidate = self.prepare_candidate()
        original_inode = (candidate / 'bin/cagents').stat().st_ino
        next_job = '00000000-0000-4000-8000-000000000002'
        retried = self.install('--prepare-candidate', next_job)
        self.assertEqual(retried.returncode, 0, retried.stdout + retried.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.assertEqual((candidate / 'bin/cagents').stat().st_ino, original_inode)
        self.assertEqual([e['event'] for e in self.events()], ['verify'])
        self.assertTrue(self.events()[0]['stage_only'])
        self.assertEqual(json.loads((candidate / '.cagents-prepared-candidate-v1.json').read_text())['job'], next_job)
        self.calls.unlink()
        self.environment.pop('FIXTURE_WINDOWS_PREPARED')
        refused = self.install('--prepared-candidate', JOB)
        self.assertNotEqual(refused.returncode, 0, 'the previous job cannot claim the rebound marker')
        self.assertEqual(os.readlink(self.prefix / 'current'), 'versions/old')
        self.calls.unlink()
        finished = self.install('--prepared-candidate', next_job)
        self.assertEqual(finished.returncode, 0, finished.stdout + finished.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), f'versions/{VERSION}')
        self.assertEqual((candidate / 'bin/cagents').stat().st_ino, original_inode)

    # Uninstall --------------------------------------------------------------
    # Both entry points run here: the packaged helper from the installation it
    # removes, as the documentation tells people to run it, and install.sh.
    UNINSTALLERS = ("packaged helper", "install.sh")
    # The status producer's reply for a daemon process that holds the runtime
    # lock without serving yet, or any longer, in both envelope key orders.
    TRANSITIONING_STATUS = (
        '{"command":"daemon.status","data":{"cli_version":"9.9.9","daemon_present":true,'
        '"daemon_version":null,"inherited_lease":false,"instance":"production","running":false,'
        '"socket":"/run/user/1000/cagents/cagentsd.sock"},"ok":true}',
        '{"ok":true,"command":"daemon.status","data":{"instance":"production","running":false,'
        '"daemon_present":true,"socket":"/run/user/1000/cagents/cagentsd.sock","daemon_version":null}}',
    )

    def uninstall_command(self, uninstaller, *options, prefix=None):
        prefix = prefix or self.prefix
        if uninstaller == "packaged helper":
            command = ["/bin/sh", str(self.prefix / "current/bin/cagents-uninstall"), "--prefix", str(prefix)]
        else:
            command = ["/bin/sh", str(ROOT / "install.sh"), "--prefix", str(prefix),
                       *(() if "--purge" in options else ("--uninstall",))]
        return [*command, *options]

    def uninstall(self, uninstaller, *options, environment=None, prefix=None):
        # A typical login umask, which would expose files the uninstaller creates.
        return subprocess.run(self.uninstall_command(uninstaller, *options, prefix=prefix),
                              env=environment or self.environment, cwd=self.directory,
                              capture_output=True, text=True, timeout=INSTALL_TIMEOUT_SECONDS, umask=0o022)

    def install_serving(self):
        self.assert_installed(self.install(), 'candidate')
        self.assertTrue(Path(self.environment["FIXTURE_DAEMON_STATE"]).exists())
        self.calls.unlink()

    def record_update(self):
        """Leave the updater state an in-app update job leaves in its prefix."""
        state = self.prefix / ".update"
        state.mkdir(mode=0o700, exist_ok=True)
        for name, text in (("lock", ""), ("status.json", '{"phase":"Succeeded"}\n'), (f"{JOB}.log", "updated\n")):
            (state / name).write_text(text)
            (state / name).chmod(0o600)
        return state

    def assert_still_installed(self):
        self.assertEqual(os.readlink(self.prefix / "current"), f"versions/{VERSION}")
        self.assertTrue((self.prefix / "current/bin/cagents").is_file())
        self.assertTrue((self.prefix / ".cagents-install-root").is_file())
        self.assertTrue(self.managed_link("bin/cagents").is_symlink())

    def test_uninstall_entry_points_share_one_removal_procedure(self):
        def shared(path):
            text = path.read_text()
            begin, end = "# BEGIN shared uninstall\n", "# END shared uninstall\n"
            self.assertEqual((text.count(begin), text.count(end)), (1, 1), path)
            return text[text.index(begin):text.index(end) + len(end)]
        self.assertEqual(shared(ROOT / "install.sh"), shared(ROOT / "packaging/cagents-uninstall"))

    def test_uninstall_keeps_an_installation_whose_daemon_is_starting_or_draining(self):
        self.install_serving()
        for uninstaller in self.UNINSTALLERS:
            for reply in self.TRANSITIONING_STATUS:
                with self.subTest(uninstaller=uninstaller, reply=reply):
                    result = self.uninstall(uninstaller, environment=dict(self.environment, FIXTURE_STATUS_REPLY=reply))
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("the daemon is starting or stopping; retry once it has settled", result.stderr)
                    self.assert_still_installed()
                    self.assertNotIn("daemon-stop", [e["event"] for e in self.events()])

    def test_uninstall_after_an_update_lets_the_same_prefix_be_reinstalled(self):
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                self.install_serving()
                self.record_update()
                result = self.uninstall(uninstaller)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual([e["event"] for e in self.events()].count("daemon-stop"), 1)
                self.assertFalse(self.prefix.exists(), result.stderr)
                self.assertFalse(self.managed_link("bin/cagents").is_symlink())
                self.calls.unlink()
        self.assert_installed(self.install(), 'candidate')

    def test_uninstall_refuses_while_an_update_holds_the_updater_lock(self):
        self.install_serving()
        state = self.record_update()
        with (state / "lock").open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            for uninstaller in self.UNINSTALLERS:
                with self.subTest(uninstaller=uninstaller):
                    result = self.uninstall(uninstaller)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("an application update is in progress", result.stderr)
                    self.assert_still_installed()
                    self.assertEqual(sorted(p.name for p in state.iterdir()), sorted(["lock", "status.json", f"{JOB}.log"]))
                    # The daemon is checked only after the update is excluded.
                    self.assertFalse(self.calls.exists())

    def test_an_update_cannot_start_while_an_installation_without_updater_state_is_removed(self):
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                self.install_serving()
                self.assertFalse((self.prefix / ".update").exists())
                result = self.uninstall(uninstaller, environment=dict(
                    self.environment, FIXTURE_UPDATE_DURING_STOP=str(self.prefix)))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                events = [e["event"] for e in self.events()]
                self.assertIn("update-busy", events)
                self.assertNotIn("update-started", events)
                self.assertFalse(self.prefix.exists(), result.stderr)
                self.calls.unlink()

    def test_a_refused_uninstall_leaves_updater_state_the_updater_accepts(self):
        self.install_serving()
        self.environment["FIXTURE_STATUS_REPLY"] = self.TRANSITIONING_STATUS[0]
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                result = self.uninstall(uninstaller)
                self.assertNotEqual(result.returncode, 0)
                # update.rs refuses state that others can read or write.
                self.assertEqual((self.prefix / ".update").stat().st_mode & 0o777, 0o700)
                self.assertEqual((self.prefix / ".update/lock").stat().st_mode & 0o777, 0o600)
                self.assert_still_installed()

    def test_uninstall_keeps_the_marker_while_unrecognized_files_remain(self):
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                self.install_serving()
                self.record_update()
                self.write(self.prefix / "notes.txt", "kept\n")
                result = self.uninstall(uninstaller)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"kept {self.prefix} and its installation marker because it also contains:\n"
                              f"{self.prefix / 'notes.txt'}\n", result.stderr)
                self.assertEqual(sorted(p.name for p in self.prefix.iterdir()), [".cagents-install-root", "notes.txt"])
                self.calls.unlink()
        self.assert_installed(self.install(), 'candidate')

    def test_uninstall_refuses_a_symlinked_prefix(self):
        self.install_serving()
        link = self.directory / "linked prefix"
        link.symlink_to(self.prefix)
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                result = self.uninstall(uninstaller, prefix=link)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"refusing to uninstall through symlink {link}", result.stderr)
                self.assert_still_installed()
                self.assertFalse(self.calls.exists())

    def waiting_uninstall(self, uninstaller, state):
        """Start an uninstall that waits on the updater lock this test holds."""
        process = subprocess.Popen(self.uninstall_command(uninstaller), env=self.environment, cwd=self.directory,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(process.kill)
        deadline = time.monotonic() + 5
        # The waiter holds its own descriptor of the lock once it reaches flock.
        while not any(os.path.realpath(f"/proc/{pid}/fd/9") == str(state / "lock")
                      for pid in [process.pid, *self.children(process.pid)]):
            if process.poll() is not None:
                self.fail(f"the uninstall exited before waiting: {process.communicate()}")
            self.assertLess(time.monotonic(), deadline, "the uninstall never waited on the updater lock")
            time.sleep(0.02)
        return process

    @staticmethod
    def children(pid):
        try:
            return [int(child) for child in Path(f"/proc/{pid}/task/{pid}/children").read_text().split()]
        except OSError:
            return []

    def test_an_uninstall_waiting_on_another_finds_the_installation_gone(self):
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                self.install_serving()
                state = self.record_update()
                with (state / "lock").open("r+") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    process = self.waiting_uninstall(uninstaller, state)
                    # The first uninstall removes everything, then exits.
                    shutil.rmtree(self.prefix)
                stdout, stderr = process.communicate(timeout=INSTALL_TIMEOUT_SECONDS)
                self.assertEqual(process.returncode, 0, stdout + stderr)
                self.assertIn(f"no install.sh installation found at {self.prefix}", stderr)
                self.assertNotIn("No such file", stderr)
                self.assertFalse(self.prefix.exists())
                self.calls.unlink(missing_ok=True)

    def test_an_uninstall_whose_updater_state_was_replaced_while_waiting_refuses(self):
        self.install_serving()
        state = self.record_update()
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                with (state / "lock").open("r+") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    process = self.waiting_uninstall(uninstaller, state)
                    shutil.rmtree(state)
                stdout, stderr = process.communicate(timeout=INSTALL_TIMEOUT_SECONDS)
                self.assertNotEqual(process.returncode, 0, stdout + stderr)
                self.assertIn("the installation changed while waiting for the updater lock; retry", stderr)
                self.assert_still_installed()
                state = self.record_update()

    def test_uninstall_refuses_an_updater_lock_that_is_not_a_regular_file(self):
        self.install_serving()
        state = self.prefix / ".update"
        state.mkdir(mode=0o700)
        os.mkfifo(state / "lock", 0o600)
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                result = self.uninstall(uninstaller)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"unsafe updater state at {state}", result.stderr)
                self.assert_still_installed()

    def purge_environment(self):
        environment = dict(self.environment, XDG_CACHE_HOME=str(self.home / "custom cache"),
                           XDG_STATE_HOME=str(self.home / "custom state"))
        for variable in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
            for name in ("cagents", "cagents-development", "unrelated"):
                self.write(Path(environment[variable]) / name / "saved", "private fixture\n")
        return environment

    def test_purge_removes_production_state_under_a_custom_xdg_state_home(self):
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                self.install_serving()
                environment = self.purge_environment()
                result = self.uninstall(uninstaller, "--purge", environment=environment)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(self.prefix.exists())
                for variable in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
                    base = Path(environment[variable])
                    self.assertFalse((base / "cagents").exists(), variable)
                    for name in ("cagents-development", "unrelated"):
                        self.assertEqual((base / name / "saved").read_text(), "private fixture\n")
                self.calls.unlink()

    def test_purge_with_a_relative_xdg_state_home_changes_nothing(self):
        self.install_serving()
        environment = dict(self.purge_environment(), XDG_STATE_HOME="relative state")
        for uninstaller in self.UNINSTALLERS:
            with self.subTest(uninstaller=uninstaller):
                result = self.uninstall(uninstaller, "--purge", environment=environment)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("XDG_STATE_HOME must be an absolute path", result.stderr)
                self.assert_still_installed()
                self.assertEqual((self.home / "data/cagents/saved").read_text(), "private fixture\n")
                self.assertFalse(self.calls.exists())

if __name__ == "__main__":
    unittest.main()
