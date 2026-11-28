"""Verify the macOS disk-image packaging and launcher contract on Linux."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
VERSION = "9.9.9"
ARCHIVE = f"cagents-{VERSION}-aarch64-apple-darwin.tar.gz"
IMAGE = f"Cagents-{VERSION}-arm64.dmg"
ALIAS = "Cagents-arm64.dmg"
APP = "Cagents.app"
LAUNCHER = "Contents/MacOS/Cagents"
RESOURCES = "Contents/Resources"


def executable(path, body, interpreter="/bin/sh"):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!{interpreter}\n{body}\n")
    path.chmod(0o755)


def checksum(path):
    return f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"


@unittest.skipUnless(sys.platform == "linux", "requires Linux stand-ins for Darwin tooling")
class MacosDiskImage(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        directory = tempfile.TemporaryDirectory(prefix="cagents-disk-image-", dir="/var/tmp")
        cls.addClassCleanup(directory.cleanup)
        root = Path(directory.name)
        cls.dist = root / "dist"
        cls.image = root / "image"
        tools = root / "tools"
        header = tools / "mach-o/dyld.h"
        header.parent.mkdir(parents=True)
        header.write_text('''
#include <stdint.h>
#include <unistd.h>
static int _NSGetExecutablePath(char *path, uint32_t *size)
{
    ssize_t result = readlink("/proc/self/exe", path, *size);
    if (result < 0 || (uint32_t)result >= *size) return -1;
    path[result] = '\\0';
    return 0;
}
''')
        executable(tools / "cc",
                   'shift 5; exec /usr/bin/cc -O2 -Wall -Wextra -Werror -I"${0%/*}" "$@"')
        executable(tools / "uname", 'case "$1" in -s) echo Darwin ;; -m) echo arm64 ;; *) exit 1 ;; esac')
        executable(tools / "date", 'exec /usr/bin/date -u -d "@$3" "$4"')
        executable(tools / "otool", 'echo "sectname __info_plist"')
        executable(tools / "plutil", "exit 0")
        executable(tools / "codesign", '''set -eu
for path do :; done
[ -e "$path" ]
case $1 in
    --force) if [ -d "$path" ]; then
        mkdir -p "$path/Contents/_CodeSignature"
        echo fixture > "$path/Contents/_CodeSignature/CodeResources"
    fi ;;
    --verify) [ ! -d "$path" ] || [ -f "$path/Contents/_CodeSignature/CodeResources" ] ;;
    *) exit 1 ;;
esac''')
        executable(tools / "tar", '''
import os, sys
args = iter(sys.argv[1:])
forward = []
for arg in args:
    if arg in ("--no-xattrs", "--no-acls", "--no-fflags", "--no-mac-metadata"):
        continue
    if arg in ("--uname", "--gname"):
        next(args)
    elif arg in ("--uid", "--gid"):
        forward.extend(("--owner" if arg == "--uid" else "--group", next(args)))
    else:
        forward.append(arg)
os.execv("/usr/bin/tar", ("tar", *forward))
''', "/usr/bin/python3")
        executable(tools / "hdiutil", '''set -eu
[ "$1" = create ] && [ "$2" = -srcfolder ] && [ "$4" = -volname ] && [ "$5" = Cagents ]
[ "$6" = -format ] && [ "$7" = UDZO ]
cp -R "$3" "$FIXTURE_IMAGE"
/usr/bin/tar -cf "$8" -C "$3" .
''')
        for name in ("cagents", "cagents-ui", "cagentsd", "cagents-dictate"):
            executable(root / "bin" / name, f'''case "$*" in
    --version) echo "{name} {VERSION}" ;;
    "daemon protocol-version") echo 9 ;;
    *) exit 1 ;;
esac''')
        result = subprocess.run(
            [str(ROOT / "scripts/package-macos"), "--prebuilt", str(root / "bin"),
             "--output", str(cls.dist), "--no-smoke", "--dmg"],
            env={"PATH": f"{tools}:{os.defpath}", "VERSION": VERSION, "CAGENTS_SKIP_STRIP": "1",
                 "TMPDIR": str(root), "FIXTURE_IMAGE": str(cls.image)},
            text=True, capture_output=True, timeout=30,
        )
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="cagents-first-launch-", dir="/var/tmp")
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.prefix = self.home / "Library/Application Support/cagents-install"
        self.app = self.home / "Applications" / APP
        self.app.parent.mkdir()
        shutil.copytree(self.image / APP, self.app)
        self.resources = self.app / RESOURCES
        self.installer = self.resources / "install-macos.sh"
        self.launched = self.home / "installed"
        self.environment = {"HOME": str(self.home), "PATH": os.defpath}

    def simulate_install(self):
        launcher = self.prefix / APP / LAUNCHER
        ui = self.prefix / "current/bin/cagents-ui"
        executable(ui, 'printf "%s\\n" "$@"')
        # The archive contains the normal launcher, compiled without the DMG flavor.
        launcher.parent.mkdir(parents=True)
        with tarfile.open(self.dist / ARCHIVE) as archive:
            launcher.write_bytes(archive.extractfile(f"cagents-{VERSION}/{APP}/{LAUNCHER}").read())
        launcher.chmod(0o755)
        (self.prefix / ".cagents-install-root").write_text("cagents install root\n")
        (self.prefix / ".previous").touch()

    def set_installer(self, body):
        executable(self.installer, body)
        (self.resources / "SHA256SUMS-macos").write_text(
            checksum(self.resources / ARCHIVE) + checksum(self.installer)
        )

    def run_app(self, *args):
        return subprocess.run([str(self.app / LAUNCHER), *args], env=self.environment,
                              cwd=self.home, text=True, capture_output=True, timeout=30)

    def test_disk_image_contains_offline_install_inputs_and_a_stable_alias(self):
        self.assertEqual((self.image / "Applications").readlink(), Path("/Applications"))
        self.assertEqual((self.dist / IMAGE).read_bytes(), (self.dist / ALIAS).read_bytes())
        names = (ARCHIVE, "install-macos.sh", IMAGE, ALIAS)
        self.assertEqual((self.dist / "SHA256SUMS-macos").read_text(),
                         "".join(checksum(self.dist / name) for name in names))
        for name in names[:2]:
            self.assertEqual((self.resources / name).read_bytes(), (self.dist / name).read_bytes())
        self.assertEqual((self.resources / "SHA256SUMS-macos").read_text(),
                         "".join(checksum(self.dist / name) for name in names[:2]))
        with tarfile.open(self.dist / ARCHIVE) as archive:
            self.assertFalse(any(name.endswith("/first-launch") for name in archive.getnames()))

    def test_first_launch_runs_the_local_installer_before_executing_the_ui(self):
        self.simulate_install()
        (self.prefix / ".previous").unlink()
        self.set_installer(f'''set -eu
[ "$1" = --version ] && [ "$2" = "{VERSION}" ] && [ "$3" = --prefix ]
[ "$4" = "$HOME/Library/Application Support/cagents-install" ]
[ "$5" = --no-start ] && [ "$6" = --bootstrap ]
[ "$CAGENTS_ARCHIVE" = "${{0%/*}}/{ARCHIVE}" ]
[ "$CAGENTS_CHECKSUMS" = "${{0%/*}}/SHA256SUMS-macos" ]
touch "$4/.previous" "$HOME/installed"
''')
        result = self.run_app("-psn_0_1234", "--version")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.launched.is_file())
        self.assertEqual(result.stdout, "--version\n")

    def test_later_launch_uses_the_selected_version_without_reinstalling(self):
        self.simulate_install()
        (self.prefix / APP / LAUNCHER).unlink()
        (self.prefix / "current").rename(self.prefix / "updated")
        executable(self.prefix / "updated/bin/cagents-ui", 'echo updated')
        (self.prefix / "current").symlink_to("updated")
        self.set_installer('touch "$HOME/installed"; exit 1')
        result = self.run_app()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "updated\n")
        self.assertFalse(self.launched.exists())

    def test_changed_embedded_installer_does_not_run(self):
        self.set_installer("exit 0")
        executable(self.installer, 'touch "$HOME/installed"')
        self.assertNotEqual(self.run_app().returncode, 0)
        self.assertFalse(self.launched.exists())

    def test_launcher_outside_applications_does_not_install(self):
        self.set_installer('touch "$HOME/installed"')
        mounted = self.home / "mounted" / APP
        mounted.parent.mkdir()
        self.app.rename(mounted)
        self.app = mounted
        self.assertNotEqual(self.run_app().returncode, 0)
        self.assertFalse(self.launched.exists())


if __name__ == "__main__":
    unittest.main()
