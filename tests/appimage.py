#!/usr/bin/env python3
"""Exercise AppRun against a disposable installed tree."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class AppImageLaunch(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="cagents-appimage-", dir="/var/tmp")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.home = self.root / "home"
        self.data = self.root / "data with spaces"
        self.prefix = self.data / "cagents-install"
        self.appdir = self.root / "AppDir"
        self.home.mkdir()
        self.appdir.mkdir()
        shutil.copy2(ROOT / "packaging/appimage/AppRun", self.appdir / "AppRun")
        self.log = self.root / "calls"
        self.environment = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(self.home),
            "XDG_DATA_HOME": str(self.data),
            "CALLS": str(self.log),
            "APPDIR": str(self.appdir),
            "APPIMAGE": "/private/download/cagents.AppImage",
        }
        self.executable(
            self.appdir / "install.sh",
            '''#!/bin/sh
set -eu
printf 'stage %s from %s\n' "$2" "$7" >>"$CALLS"
[ "$1" = --version ] && [ "$3" = --prefix ] && [ "$5" = --stage-only ] && [ "$6" = --appimage-current ]
[ -f "$CAGENTS_ARCHIVE" ] && [ -f "$CAGENTS_CHECKSUMS" ]
mkdir -p "$4/versions/$2/bin"
printf 'cagents install root\n' > "$4/.cagents-install-root"
ln -sfn "versions/$2" "$4/current"
cat > "$4/current/bin/cagents" <<'EOF'
#!/bin/sh
set -eu
[ -z "${APPDIR+x}" ] && [ -z "${APPIMAGE+x}" ] && [ -z "${CAGENTS_ARCHIVE+x}" ]
printf 'open installed %s\n' "$1" >>"$CALLS"
EOF
chmod 0755 "$4/current/bin/cagents"
''',
        )

    @staticmethod
    def executable(path, contents):
        path.write_text(contents)
        path.chmod(0o755)

    def installed(self, version):
        binary = self.prefix / "versions" / version / "bin/cagents"
        binary.parent.mkdir(parents=True)
        (self.prefix / ".cagents-install-root").write_text("cagents install root\n")
        (self.prefix / "current").symlink_to(f"versions/{version}")
        self.executable(binary, '#!/bin/sh\nprintf "open existing %s\\n" "$1" >>"$CALLS"\n')

    def launch(self, version):
        (self.appdir / "VERSION").write_text(version + "\n")
        (self.appdir / f"cagents-{version}-x86_64-unknown-linux-gnu.tar.gz").touch()
        (self.appdir / "SHA256SUMS").touch()
        return subprocess.run(
            [self.appdir / "AppRun"], env=self.environment, capture_output=True, text=True, timeout=10,
        )

    def test_first_and_equal_launch_install_once_and_open_installed_cli(self):
        for _ in range(2):
            result = self.launch("9.10.0")
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), [
            "stage 9.10.0 from -", "open installed ui", "open installed ui",
        ])

    def test_older_or_same_stable_version_only_opens_the_existing_ui(self):
        self.installed("9.10.0+local.abcd")
        for version in ("9.9.10", "9.10.0"):
            result = self.launch(version)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), ["open existing ui", "open existing ui"])

    def test_newer_launch_opens_previous_ui_then_stages_with_exact_source(self):
        self.installed("9.9.10")
        result = self.launch("9.10.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), [
            "open existing ui", "stage 9.10.0 from versions/9.9.10",
        ])

    def test_foreign_current_link_is_rejected_without_launching_or_installing(self):
        self.installed("9.9.10")
        (self.prefix / "current").unlink()
        (self.prefix / "current").symlink_to(self.root)
        result = self.launch("9.10.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())

    def test_shared_writable_install_root_is_rejected_before_launch(self):
        self.installed("9.9.10")
        self.prefix.chmod(0o775)
        result = self.launch("9.10.0")
        self.assertIn("not privately owned", result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
