#!/usr/bin/python3
"""Contracts for scripts/gradle-remote, with fake ssh, rsync and gradlew; no network or Gradle."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
# Bounds only a hang; a passing run returns at once.
HANG_GUARD = 60

# Records each call, with its standard input for ssh, and stands in for the
# host: ssh runs the script it is sent with bash, in FAKE_HOME, and rsync
# copies with the real rsync after mapping HOST:PATH to FAKE_HOME/PATH.
FAKE_SSH = f'''#!{sys.executable}
import json, os, subprocess, sys
args = sys.argv[1:]
while args and args[0] == "-o":
    args = args[2:]
host, command = args[0], args[1:]
script = sys.stdin.read() if command == ["bash", "-s"] else None
with open(os.environ["FAKE_RECORD"], "a") as out:
    out.write(json.dumps({{"tool": "ssh", "host": host, "command": command, "script": script}}) + "\\n")
home = os.environ["FAKE_HOME"]
env = {{**os.environ, "HOME": home}}
if script is not None:
    sys.exit(subprocess.run(["bash", "-c", script], cwd=home, env=env).returncode)
sys.exit(subprocess.run(command, cwd=home, env=env, shell=len(command) == 1).returncode)
'''

FAKE_RSYNC = f'''#!{sys.executable}
import json, os, re, subprocess, sys
home = os.environ["FAKE_HOME"]
args = [re.sub(r"^[^/:]+:", home + "/", arg) if re.match(r"^[^/:-][^/:]*:", arg) else arg for arg in sys.argv[1:]]
with open(os.environ["FAKE_RECORD"], "a") as out:
    out.write(json.dumps({{"tool": "rsync", "args": sys.argv[1:]}}) + "\\n")
sys.exit(subprocess.run([os.environ["REAL_RSYNC"], *args]).returncode)
'''

# Records its arguments and working directory, and writes what a build makes.
FAKE_GRADLEW = '''#!/bin/sh
printf '%s\\n' "gradlew $* in $(pwd) home=$ANDROID_HOME local=$CAGENTS_GRADLE_LOCAL" >>"$FAKE_GRADLE_RECORD"
mkdir -p shared/build/test-results/jvmTest androidApp/build/outputs/apk/debug
echo report >shared/build/test-results/jvmTest/TEST-a.xml
echo apk >androidApp/build/outputs/apk/debug/app-debug.apk
exit "${FAKE_GRADLE_STATUS:-0}"
'''


class GradleRemoteTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.temp = Path(directory.name)
        self.checkout = self.temp / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        shutil.copy2(ROOT / "scripts/gradle-remote", self.checkout / "scripts/gradle-remote")
        mobile = self.checkout / "mobile"
        (mobile / "shared/src").mkdir(parents=True)
        (mobile / "shared/src/Main.kt").write_text("source\n")
        (mobile / "local.properties").write_text("sdk.dir=/here\n")
        (mobile / "shared/build/stale").mkdir(parents=True)
        (mobile / "gradlew").write_text(FAKE_GRADLEW)
        (mobile / "gradlew").chmod(0o755)
        (self.checkout / "crates/mobile-api/fixtures").mkdir(parents=True)
        (self.checkout / "crates/mobile-api/fixtures/v1.jsonl").write_text("{}\n")
        self.home = self.temp / "host-home"
        self.home.mkdir()
        bin_directory = self.temp / "bin"
        bin_directory.mkdir()
        for name, text in (("ssh", FAKE_SSH), ("rsync", FAKE_RSYNC)):
            (bin_directory / name).write_text(text)
            (bin_directory / name).chmod(0o755)
        self.record = self.temp / "record.jsonl"
        self.gradle_record = self.temp / "gradle.txt"
        self.env = {
            **os.environ,
            "PATH": f"{bin_directory}:{os.environ['PATH']}",
            "FAKE_HOME": str(self.home),
            "FAKE_RECORD": str(self.record),
            "FAKE_GRADLE_RECORD": str(self.gradle_record),
            "REAL_RSYNC": shutil.which("rsync"),
            "CAGENTS_BUILD_REMOTE": "buildhost",
        }
        self.env.pop("ANDROID_HOME", None)
        self.env.pop("CAGENTS_GRADLE_LOCAL", None)

    def run_script(self, *args, **env):
        return subprocess.run([str(self.checkout / "scripts/gradle-remote"), *args],
                              env={**self.env, **env}, capture_output=True, text=True, timeout=HANG_GUARD)

    def remote_mobile(self):
        cache = self.home / ".cache/cagents-gradle"
        [checkout] = list(cache.iterdir())
        return checkout / "mobile"

    def test_builds_on_the_host_and_brings_back_reports_and_fetched_outputs(self):
        result = self.run_script("--fetch", "androidApp/build/outputs/apk/debug", "--", ":a:b", "it's")
        self.assertEqual(result.returncode, 0, result.stderr)
        remote = self.remote_mobile()
        # Sources go; outputs and machine-local settings don't.
        self.assertTrue((remote / "shared/src/Main.kt").is_file())
        self.assertFalse((remote / "local.properties").exists())
        self.assertFalse((remote / "shared/build/stale").exists())
        self.assertTrue((remote.parent / "crates/mobile-api/fixtures/v1.jsonl").is_file())
        # Gradle ran there, with the arguments intact and the SDK default.
        gradle = self.gradle_record.read_text()
        self.assertIn(f"gradlew --console=plain :a:b it's in {remote}", gradle)
        self.assertIn(f"home={self.home}/Android/Sdk local=1", gradle)
        # The script went to bash, not the login shell, under the lock.
        scripts = [line for line in self.record.read_text().splitlines() if '"script": "' in line]
        self.assertEqual(len(scripts), 1)
        self.assertIn("flock ../.lock", scripts[0])
        # Reports and the fetched APK came back.
        mobile = self.checkout / "mobile"
        self.assertEqual((mobile / "shared/build/test-results/jvmTest/TEST-a.xml").read_text(), "report\n")
        self.assertEqual((mobile / "androidApp/build/outputs/apk/debug/app-debug.apk").read_text(), "apk\n")

    def test_a_failed_build_still_returns_its_reports_but_fetches_nothing(self):
        result = self.run_script("--fetch", "androidApp/build/outputs/apk/debug", ":a", FAKE_GRADLE_STATUS="3")
        self.assertEqual(result.returncode, 3)
        mobile = self.checkout / "mobile"
        self.assertTrue((mobile / "shared/build/test-results/jvmTest/TEST-a.xml").is_file())
        self.assertFalse((mobile / "androidApp/build/outputs/apk/debug/app-debug.apk").exists())

    def test_runs_here_only_when_asked_to(self):
        result = self.run_script(":a", CAGENTS_BUILD_REMOTE="local")
        self.assertEqual(result.returncode, 64)
        self.assertIn("CAGENTS_GRADLE_LOCAL=1", result.stderr)
        self.assertFalse(self.gradle_record.exists(), "Gradle didn't run")
        result = self.run_script(":a", CAGENTS_GRADLE_LOCAL="1", ANDROID_HOME="/sdk")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"gradlew --console=plain :a in {self.checkout / 'mobile'} home=/sdk local=1",
                      self.gradle_record.read_text())
        self.assertFalse(self.record.exists(), "nothing went to a host")

    def test_the_wrapper_refuses_to_start_gradle_unless_told_to(self):
        gradlew = ROOT / "mobile/gradlew"
        env = {key: value for key, value in self.env.items() if key != "CAGENTS_GRADLE_LOCAL"}
        result = subprocess.run([str(gradlew), "--version"], env=env, cwd=ROOT / "mobile",
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 64)
        self.assertIn("mise run mobile:test", result.stderr)

    def test_refuses_no_gradle_arguments(self):
        self.assertEqual(self.run_script().returncode, 2)
        self.assertEqual(self.run_script("--fetch").returncode, 2)


if __name__ == "__main__":
    unittest.main()
