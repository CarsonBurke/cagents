#!/usr/bin/env python3
"""Run the real installer with private coordinator/host process fixtures.

The fixtures exercise archive extraction, coordinator selection, re-entry and
publication without compiling Rust, opening windows or starting a real daemon.
They do not replace the Rust source-inspection or install-guard contracts.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
VERSION = "9.9.9"
TOP = f"cagents-{VERSION}"
ARCHIVE_NAME = f"{TOP}-x86_64-unknown-linux-gnu.tar.gz"

COORDINATOR = r'''
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
log = pathlib.Path(os.environ['FIXTURE_CALLS'])
def record(event, **fields):
    with log.open('a') as output:
        output.write(json.dumps(dict(event=event, **fields)) + '\n')
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
    assert os.environ['CAGENTS_UI_UPDATE_GUARD_FD'] == 'fixture-only'
    record('verify')
elif args == ['--json', 'daemon', 'status']:
    record('status')
    rewrite = os.environ.get('FIXTURE_STATUS_REWRITES')
    if rewrite:
        # Something else writes the link path after the installer checked it.
        path = pathlib.Path(rewrite)
        path.unlink()
        path.write_text('written during the installation\n')
    print(json.dumps({'ok': True, 'command': 'daemon.status', 'data': {'running': False}},
                     separators=(',', ':')))
elif args == ['daemon', 'start']:
    record('daemon-start-fixture')
    if os.environ.get('FIXTURE_DAEMON_START_FAILS'):
        sys.exit(1)
else:
    raise AssertionError(args)
'''

INSPECTOR = r'''
import json, os, pathlib, sys
assert sys.argv[1:] == ['--inspect-upgrade-source', 'fixture']
with pathlib.Path(os.environ['FIXTURE_CALLS']).open('a') as output:
    output.write(json.dumps({'event': 'inspect', 'source': SOURCE}) + '\n')
print(json.dumps({'Ok': {'stopped': [], 'stopped_windows': [], 'labels': {}}}))
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
        }
        self.executable(self.tools / "getconf", '#!/bin/sh\nprintf "glibc 2.99\\n"\n')
        for name in ("curl", "wget", "gh"):
            self.executable(self.tools / name, '#!/bin/sh\nexit 97\n')
        self.executable(self.tools / "cagentsd", '#!/bin/sh\nexit 96\n')
        self.payload = self.directory / TOP
        self.python(self.payload / "bin/cagents", COORDINATOR)
        self.python(self.payload / "bin/cagentsd", "SOURCE = 'candidate'\n" + INSPECTOR)
        for name in ("cagents-ui", "cagents-uninstall"):
            self.executable(self.payload / "bin" / name, '#!/bin/sh\nexit 98\n')
        self.write(self.payload / "bin/.cagents-ui-lease-v1", "1\n")
        for name in ("applications/dev.cagents.Cagents.desktop",
                     "autostart/dev.cagents.Cagents.Daemon.desktop",
                     "icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
                     "icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
                     "metainfo/dev.cagents.Cagents.metainfo.xml", "doc/cagents/LICENSE"):
            source = "LICENSE" if name == "doc/cagents/LICENSE" else "packaging/" + Path(name).name
            path = self.payload / "share" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / source, path)
        self.archive = self.directory / ARCHIVE_NAME
        self.checksums = self.directory / "SHA256SUMS"
        self.package()
        self.environment.update(CAGENTS_ARCHIVE=str(self.archive), CAGENTS_CHECKSUMS=str(self.checksums))

    def package(self, *, exclude=None):
        with tarfile.open(self.archive, "w:gz") as archive:
            archive.add(self.payload, arcname=TOP,
                        filter=lambda entry: None if entry.name == f"{TOP}/{exclude}" else entry)
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
        return subprocess.run(
            ["/bin/sh", str(ROOT / "install.sh"), "--version", VERSION, "--prefix", str(self.prefix),
             *options],
            env=self.environment, cwd=self.directory, capture_output=True, text=True, timeout=10,
        )

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
        self.assertEqual(os.readlink(self.home / ".local/bin/cagents"), str(installed / "cagents"))
        events = self.events()
        self.assertEqual([e['source'] for e in events if e['event'] == 'inspect'], [source])
        self.assertEqual(sum(e['event'] == 'verify' for e in events), 1)
        self.assertEqual(sum(e['event'] == 'daemon-start-fixture' for e in events), 1)
        self.assertEqual(list(self.downloads.iterdir()), [], "downloaded coordinator pair was not cleaned")
        return next(e for e in events if e['event'] == 'coordinate')

    def test_staging_preserves_current_and_public_links(self):
        previous = self.installed_cli(guarded=True)
        current = self.prefix / "current"
        before = current.lstat()
        launcher = self.home / ".local/bin/cagents"
        launcher.parent.mkdir(parents=True)
        launcher.symlink_to(current / "bin/cagents")
        launcher_before = launcher.lstat()
        result = self.install("--stage-only")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(os.readlink(current), "versions/old")
        self.assertEqual(current.lstat().st_ino, before.st_ino)
        self.assertEqual(current.lstat().st_ctime_ns, before.st_ctime_ns)
        self.assertEqual(launcher.lstat().st_ino, launcher_before.st_ino)
        self.assertTrue((self.prefix / f"versions/{VERSION}/bin/cagents").is_file())
        self.assertFalse((self.prefix / f"versions/{VERSION}/.deferred-owner.json").exists())
        self.assertFalse((self.home / "data/applications").exists())
        self.assertEqual([event['event'] for event in self.events()], ['inspect', 'coordinate', 'verify'])
        self.assertIn(str(self.prefix / f"versions/{VERSION}/bin/cagents"), result.stdout)

    def test_staging_a_fresh_prefix_requires_an_ordinary_first_install(self):
        result = self.install("--stage-only")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('--stage-only requires an existing installation', result.stderr)
        self.assertIn('without --stage-only for the first install', result.stderr)
        self.assertFalse((self.prefix / "current").is_symlink())
        self.assertFalse((self.home / ".local/bin/cagents").exists())
        self.assertFalse((self.prefix / f"versions/{VERSION}").exists())

    def test_staging_refuses_a_dangling_current_without_publishing_the_version(self):
        self.installed_cli(guarded=True)
        (self.prefix / "current").unlink()
        (self.prefix / "current").symlink_to("versions/missing")
        result = self.install("--stage-only")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('--stage-only requires an existing installation', result.stderr)
        self.assertEqual(os.readlink(self.prefix / "current"), "versions/missing")
        self.assertFalse((self.prefix / f"versions/{VERSION}").exists())

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
        self.assertFalse((self.prefix / "versions" / VERSION).exists())

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


if __name__ == "__main__":
    unittest.main()
