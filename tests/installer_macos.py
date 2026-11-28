"""Exercise guarded macOS installer publication with a private tree on Linux."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install-macos.sh"
VERSION = "9.9.9"
APP = "Cagents.app"
LAUNCHER = "Contents/MacOS/Cagents"


@unittest.skipUnless(sys.platform == "linux" and os.getuid() != 0, "requires GNU tools and an ordinary user")
class MacosInstaller(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="cagents-macos-install-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.home = self.root / "home"
        self.home.mkdir()
        tools = self.root / "tools"
        self.stage = self.root / "release"
        self.prefix = self.root / "install"
        self.tree = self.prefix / "versions" / VERSION
        self.app = self.prefix / APP
        self.installer = self.root / INSTALLER.name

        # Daemon status uses an absolute JXA command on macOS. The fixture's
        # status reader reports an absent daemon and never contacts the host.
        source = INSTALLER.read_text()
        self.assertIn("/usr/bin/osascript -l JavaScript", source)
        self.installer.write_text(source.replace("/usr/bin/osascript -l JavaScript", "osascript -l JavaScript"))
        for name in ("codesign", "osascript", "plutil"):
            self.executable(tools / name, "exit 0")
        for name in ("dscl", "launchctl", "lsof"):
            self.executable(tools / name, "exit 1")
        self.executable(tools / "uname", 'case "$1" in -s) echo Darwin ;; -m) echo arm64 ;; *) exit 1 ;; esac')
        self.executable(tools / "ditto", 'exec /usr/bin/cp -a -- "$@"')
        self.executable(tools / "mv", 'if [ "$1" = -fh ]; then shift; set -- -fT "$@"; fi\nexec /usr/bin/mv "$@"')
        self.executable(tools / "stat", '[ "$1" = -f ] && [ "$2" = %u ] || exit 1\nexec /usr/bin/stat -c %u -- "$3"')

        for program in ("cagents", "cagentsd", "cagents-ui", "cagents-dictate"):
            self.executable(self.stage / "bin" / program, f'''case "$*" in
    --version) echo "{program} {VERSION}" ;;
    "--no-start verify-install-guard --publish-only") exit 0 ;;
    "--no-start daemon status --json") echo '{{"data":{{"running":false,"daemon_present":false}}}}' ;;
    *) exit 1 ;;
esac''')
        self.executable(self.stage / APP / LAUNCHER, "exit 0")
        shutil.copytree(self.stage, self.tree)
        shutil.copytree(self.stage / APP, self.app)
        (self.prefix / "current").symlink_to(f"versions/{VERSION}")
        (self.prefix / ".cagents-install-root").write_text("cagents install root\n")
        self.environment = {
            "HOME": str(self.home), "PATH": str(tools) + os.pathsep + os.defpath,
            "TMPDIR": str(self.root),
        }

    @staticmethod
    def executable(path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)

    def reinstall(self):
        result = subprocess.run(
            ["/bin/sh", str(self.installer), "--no-start", "--prefix", str(self.prefix),
             "--version", VERSION, "--publish-tree", str(self.stage),
             "--publisher", str(self.stage / "bin/cagents")],
            env=self.environment, cwd=self.root, text=True, capture_output=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_reinstall_repairs_a_non_executable_program(self):
        program = self.tree / "bin/cagents-ui"
        program.chmod(0o644)
        self.reinstall()
        self.assertTrue(os.access(program, os.X_OK))

    def test_reinstall_repairs_a_non_executable_app_launcher(self):
        launcher = self.app / LAUNCHER
        launcher.chmod(0o644)
        self.reinstall()
        self.assertTrue(os.access(launcher, os.X_OK))

    def test_reinstall_keeps_healthy_trees(self):
        trees = (self.tree, self.app)
        before = [tree.stat().st_ino for tree in trees]
        self.reinstall()
        self.assertEqual([tree.stat().st_ino for tree in trees], before)


if __name__ == "__main__":
    unittest.main()
