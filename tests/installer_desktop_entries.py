#!/usr/bin/env python3
"""Exercise only install.sh's desktop generators with inert executable fixtures."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
CASES = (
    ("plain", "prefix-plain"),
    ("spaces", "prefix with spaces"),
    ("quotes", 'prefix "quote"'),
    ("backslash", "prefix \\backslash"),
    ("dollar", "prefix $dollar"),
    ("backtick", "prefix `backtick`"),
    ("percent", "prefix %F %U 100%"),
    ("equals", "prefix=assigned"),
    ("percent-equals", "prefix %F=assigned"),
    ("combined", 'prefix "quote" \\backslash $dollar `backtick` %F=assigned'),
    ("control-characters", "prefix\twith\rcarriage"),
)


class InstallerDesktopEntries(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="installer-desktop-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        installer = (ROOT / "install.sh").read_text()
        helper_start = installer.index("desktop_exec_escape() {\n")
        helper_end = installer.index('\nINSTALL_STAGE=$PREFIX/.staging.$$',
                                     helper_start)
        emitter_start = installer.index('\nAUTOSTART_EXEC=$(desktop_exec_escape "$BIN_TARGET")',
                                        helper_end) + 1
        emitter_end = installer.index('\nchmod 0755 "$INSTALL_STAGE/$TOP_LEVEL"', emitter_start)
        self.generator = self.directory / "desktop-generator.sh"
        # These source slices exclude installer admission, extraction and activation.
        self.generator.write_text(
            '#!/bin/sh\nset -eu\nBIN_TARGET=$1\nPREFIX=$2\nINSTALL_STAGE=$3\nTOP_LEVEL=fixture\n'
            'fail() { printf "%s\\n" "$*" >&2; exit 1; }\n'
            + installer[helper_start:helper_end] + "\n"
            + installer[emitter_start:emitter_end] + "\n"
        )

    def make_entries(self, name, suffix):
        case = self.directory / name
        prefix = case / suffix
        executable = prefix / "current/bin/cagents"
        executable.parent.mkdir(parents=True)
        executable.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "receipt = os.environ['TRIAGE_LAUNCH']\n"
            "with open(receipt + '.tmp', 'w') as output:\n"
            "    json.dump({'argv': sys.argv, 'cwd': os.getcwd()}, output)\n"
            "os.replace(receipt + '.tmp', receipt)\n"
        )
        executable.chmod(0o755)
        stage = case / "generated-files"
        application = stage / "fixture/share/applications/dev.cagents.Cagents.desktop"
        application.parent.mkdir(parents=True)
        shutil.copy2(ROOT / "packaging/dev.cagents.Cagents.desktop", application)
        autostart = stage / "fixture/share/autostart/dev.cagents.Cagents.Daemon.desktop"
        autostart.parent.mkdir(parents=True)
        result = subprocess.run(
            ["/bin/sh", str(self.generator), str(executable), str(prefix), str(stage)],
            cwd=self.directory, text=True, capture_output=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return prefix, executable, {"application": application, "autostart": autostart}

    @unittest.skipUnless(shutil.which("gio"), "gio is required to exercise desktop Exec launch")
    def test_application_and_autostart_launch_exact_executable_and_arguments(self):
        gio = shutil.which("gio")
        for name, suffix in CASES:
            prefix, executable, entries = self.make_entries(name, suffix)
            for kind, arguments in (("application", ["ui"]),
                                    ("autostart", ["daemon", "start"])):
                with self.subTest(prefix=suffix, entry=kind):
                    receipt = self.directory / f"{name}-{kind}-actual-launch.json"
                    environment = os.environ.copy()
                    environment["TRIAGE_LAUNCH"] = str(receipt)
                    environment["XDG_DATA_HOME"] = str(self.directory / "private-data")
                    result = subprocess.run(
                        [gio, "launch", str(entries[kind])],
                        cwd=self.directory, env=environment,
                        text=True, capture_output=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    deadline = time.monotonic() + 2
                    while not receipt.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue(receipt.exists(), "fixture was not launched: " + result.stderr)
                    actual = json.loads(receipt.read_text())
                    self.assertEqual(actual["argv"], [str(executable), *arguments])
                    self.assertEqual(actual["cwd"], str(self.directory))

    @unittest.skipUnless(shutil.which("desktop-file-validate"),
                         "desktop-file-validate is required to check generated entry syntax")
    def test_generated_entries_pass_desktop_file_validation(self):
        validator = shutil.which("desktop-file-validate")
        for name, suffix in CASES:
            prefix, executable, entries = self.make_entries(name, suffix)
            for kind, desktop in entries.items():
                with self.subTest(prefix=suffix, entry=kind):
                    result = subprocess.run(
                        [validator, str(desktop)], text=True, capture_output=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_desktop_values_decode_to_exact_tryexec_and_icon_paths(self):
        system_python = Path("/usr/bin/python3")
        if not system_python.is_file():
            self.skipTest("system Python is required for independent GLib key-file decoding")
        probe = subprocess.run(
            [str(system_python), "-c", "from gi.repository import GLib"],
            text=True, capture_output=True, timeout=10,
        )
        if probe.returncode:
            self.skipTest("system Python GLib introspection is unavailable")
        reader = '''import json, sys
from gi.repository import GLib
entry = GLib.KeyFile()
entry.load_from_file(sys.argv[1], GLib.KeyFileFlags.NONE)
print(json.dumps({name: entry.get_string("Desktop Entry", name)
                  for name in ("TryExec", "Icon")}))
'''
        cases = (*CASES, ("literal-escapes", r"prefix \s \n \t \r \\ trailing"))
        for name, suffix in cases:
            with self.subTest(prefix=suffix):
                prefix, executable, entries = self.make_entries(name, suffix)
                result = subprocess.run(
                    [str(system_python), "-c", reader, str(entries["application"])],
                    text=True, capture_output=True, timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                values = json.loads(result.stdout)
                self.assertEqual(values["TryExec"], str(executable))
                self.assertEqual(values["Icon"], str(
                    prefix / "current/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"
                ))


if __name__ == "__main__":
    unittest.main()
