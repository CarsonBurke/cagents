#!/usr/bin/env python3
"""Exercise held-command lifetime with private locks and owned processes only."""

import contextlib
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
import unittest.mock


ROOT = Path(__file__).resolve().parents[1]
# Isolated Python (-I) leaves this directory off sys.path.
_custody_spec = importlib.util.spec_from_file_location("fixture_custody", ROOT / "tests/fixture_custody.py")
fixture_custody = importlib.util.module_from_spec(_custody_spec)
_custody_spec.loader.exec_module(fixture_custody)
FixtureDirectory = fixture_custody.FixtureDirectory


# Builds here run in place: no test makes a systemd scope or starts the
# host's jobserver.
os.environ["CAGENTS_BUILD_SCOPE"] = "0"
# Nor may a guard they run trim the host's real offload slots.
os.environ["CAGENTS_BUILD_OFFLOAD_SLOTS"] = ""
# A caller's admission budget would let a waiter that should never let go
# expire instead; the tests that want one set it.
os.environ.pop("CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS", None)
# Bounds only a hang: every wait it guards returns as soon as what it waits for happens.
HANG_GUARD = 60
# What a checkout's gate runs beside it to build here.
GATE_FILES = ("cargo-gate", "cargo-gate-command.py", "build-host")


def copy_gate(directory):
    for name in GATE_FILES:
        shutil.copy2(ROOT / "scripts" / name, Path(directory) / name)
    return Path(directory) / "cargo-gate"


class InstalledGateDeferral(unittest.TestCase):
    """A checkout whose gate has fallen behind the installed one runs that."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-deferral-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "checkout").mkdir()
        self.gate = copy_gate(self.root / "checkout")
        self.version = int(re.search(r"^gate_version=(\d+)$", self.gate.read_text(), re.M)[1])
        self.installed = self.root / "cagents-cargo-gate"
        self.env = os.environ | {"CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"),
                                 "CAGENTS_BUILD_REMOTE": "local",
                                 "CAGENTS_BUILD_INSTALLED_GATE": str(self.installed)}
        self.env.pop("CAGENTS_BUILD_SLOT_HELD", None)

    def install(self, version):
        self.installed.write_text(f"#!/bin/sh\ngate_version={version}\necho installed \"$@\"\n")
        self.installed.chmod(0o755)

    def run_gate(self, env=None):
        return subprocess.run([str(self.gate), "exec", "echo", "checkout"], env=env or self.env,
                              cwd=self.root, capture_output=True, text=True, timeout=HANG_GUARD)

    def test_a_newer_installed_gate_runs_instead(self):
        self.install(self.version + 1)
        self.assertEqual(self.run_gate().stdout, "installed exec echo checkout\n")

    def test_an_installed_gate_no_newer_than_this_one_does_not(self):
        for version in (self.version, self.version - 1):
            with self.subTest(version=version):
                self.install(version)
                self.assertEqual(self.run_gate().stdout, "checkout\n")

    def test_an_empty_setting_runs_this_gate_always(self):
        self.install(self.version + 1)
        self.assertEqual(self.run_gate(self.env | {"CAGENTS_BUILD_INSTALLED_GATE": ""}).stdout,
                         "checkout\n")


class ThroughAMiseShim(unittest.TestCase):
    """Under a moved HOME a mise shim finds itself on PATH and re-executes forever."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-shim-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        checkout = self.root / "checkout"
        checkout.mkdir()
        self.gate = copy_gate(checkout)
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        self.shims = self.root / "real/.local/share/mise/shims"
        self.shims.mkdir(parents=True)
        # Like mise: runs the real tool only if this is its data directory's shims.
        mise = self.root / "mise"
        mise.write_text(textwrap.dedent(f"""\
            #!/bin/sh
            data=${{MISE_DATA_DIR:-${{XDG_DATA_HOME:-$HOME/.local/share}}/mise}}
            [ "$data/shims" = "{self.shims}" ] || exec "$0" "$@"
            exec {shlex.quote(sys.executable)} "$@"
        """))
        mise.chmod(0o755)
        (self.shims / "python3").symlink_to(mise)
        self.env = os.environ | {
            "HOME": str(self.scratch), "PATH": f"{self.shims}:{os.defpath}",
            "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"), "CAGENTS_BUILD_REMOTE": "local",
            "CAGENTS_BUILD_INSTALLED_GATE": ""}
        for name in ("MISE_DATA_DIR", "XDG_DATA_HOME", "CAGENTS_BUILD_SLOT_HELD"):
            self.env.pop(name, None)

    def gate_runs(self, *command):
        try:
            return subprocess.run([str(self.gate), "exec", *command], env=self.env, cwd=self.root,
                                  capture_output=True, text=True, timeout=HANG_GUARD)
        except subprocess.TimeoutExpired:
            self.fail("the gate never finished: the shim kept re-executing itself")

    def test_the_gate_finishes_under_a_moved_home(self):
        result = self.gate_runs("echo", "finished")
        self.assertEqual(result.stdout, "finished\n", result.stderr)

    def test_what_moves_home_under_the_gate_can_still_use_the_shim(self):
        self.env["HOME"] = str(self.root / "real")
        result = self.gate_runs("sh", "-c", f'HOME={self.scratch} python3 -c "print(1)"')
        self.assertEqual(result.stdout, "1\n", result.stderr)

    def test_a_callers_data_directory_is_theirs(self):
        self.env["HOME"] = str(self.root / "real")
        theirs = str(self.root / "their-own-data")
        self.env["MISE_DATA_DIR"] = theirs
        result = self.gate_runs("sh", "-c", 'echo "$MISE_DATA_DIR"')
        self.assertEqual(result.stdout.strip(), theirs, result.stderr)

    def test_an_empty_data_directory_counts_as_unset(self):
        # As it does to mise, which would keep re-executing.
        self.env["HOME"] = str(self.root / "real")
        self.env["MISE_DATA_DIR"] = ""
        result = self.gate_runs("sh", "-c", f'HOME={self.scratch} python3 -c "print(1)"')
        self.assertEqual(result.stdout, "1\n", result.stderr)


class ScriptsNameTheirPython(unittest.TestCase):
    """`python3` found on PATH can be a shim, which can re-execute forever or stop to ask."""

    # What cargo-offload starts over SSH on the build host, under that host's own
    # HOME: no moved HOME there, and its path to Python isn't this host's to name.
    on_the_build_host = {
        ("cargo-offload", 'return ["python3", "-c", BOOTSTRAP, encoded]'),
        ("cargo-offload", 'RUNNER = ["python3", "-c",'),
    }
    # Text that names the language rather than running it, such as the code
    # samples scripts/fixture-workspace writes into generated conversations.
    language_names = {
        ("content.py", '"python": """def {name}(rows, *, batch=500):'),
    }
    bare = re.compile(r"(?<![\w/.-])python3?(?:\.\d+)?(?![\w.-])")

    def test_no_script_finds_python_on_path(self):
        found = []
        for script in sorted((ROOT / "scripts").rglob("*")):
            if not script.is_file():
                continue
            try:
                lines = script.read_text().splitlines()
            except UnicodeDecodeError:
                continue
            if lines and lines[0].startswith("#!") and "python" in lines[0] and not lines[0].startswith("#!/usr/bin/python3"):
                found.append(f"{script.name}:1: {lines[0]}")
            for number, line in enumerate(lines, 1):
                text = line.strip()
                if (text.startswith("#") or (script.name, text) in self.on_the_build_host
                        or (script.name, text) in self.language_names):
                    continue
                if self.bare.search(line):
                    found.append(f"{script.name}:{number}: {text}")
        self.assertEqual(found, [], "name /usr/bin/python3; see docs/development.md, Scripts and mise shims")


class InstalledGateSync(unittest.TestCase):
    """A gate change merged into main is installed by the next gate run."""

    files = ("cargo-gate", "build-host", "cargo-gate-command.py", "cargo-gate-sync.py",
             "cargo-host-test-helper.py", "cargo_artifacts.py",
             "cargo-gate-rustc-guard", "cargo-offload", "memory-capped",
             "seed-build-dir", "build-disk-guard", "prune-build-dirs", "install-cargo-gate")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-sync-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.checkout = self.root / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        for name in self.files:
            shutil.copy2(ROOT / "scripts" / name, self.checkout / "scripts" / name)
        self.clock = int(time.time()) - 1000
        self.git_env = os.environ | {
            "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
        self.git("init", "-q", "-b", "main")
        self.first = self.commit("gate")
        self.destination = self.root / "installed"
        self.destination.mkdir()
        self.installed = self.destination / "cagents-cargo-gate"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_BUILD_")} | {
            "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"),
            "CAGENTS_BUILD_REMOTE": "local", "CAGENTS_BUILD_SCOPE": "0",
            "CAGENTS_BUILD_OFFLOAD_SLOTS": "",
            "CAGENTS_BUILD_INSTALLED_GATE": str(self.installed)}
        self.install()
        self.assertEqual(self.recorded(), self.first)

    def install(self):
        result = subprocess.run([str(self.checkout / "scripts/install-cargo-gate"),
                                 str(self.destination)], env=self.env, capture_output=True,
                                text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)

    def git(self, *arguments, env=None):
        return subprocess.run(["git", *arguments], cwd=self.checkout, env=env or self.git_env,
                              check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, message, at=None):
        """Commits everything, by default at a time before any installation."""
        if at is None:
            self.clock += 1
            at = self.clock
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message,
                 env=self.git_env | {"GIT_COMMITTER_DATE": f"@{at} +0000",
                                     "GIT_AUTHOR_DATE": f"@{at} +0000"})
        return self.git("rev-parse", "HEAD")

    def later(self):
        return int(time.time()) + 100

    def main_moves_to(self, commit):
        self.git("update-ref", "refs/remotes/origin/main", commit)

    def record(self):
        return json.loads((self.destination / "cagents-cargo-gate.installation").read_text())

    def recorded(self):
        return self.record()["commit"]

    def target(self):
        return os.readlink(self.installed)

    def run_installed(self, cwd=None):
        return subprocess.run([str(self.installed), "exec", "sh", "-c", "printf ran"],
                              cwd=cwd or self.checkout, env=self.env, capture_output=True,
                              text=True, timeout=120)

    def change_gate(self, line):
        """Adds LINE to the gate, run before anything else it does."""
        gate = self.checkout / "scripts/cargo-gate"
        shebang, rest = gate.read_text().split("\n", 1)
        gate.write_text(f"{shebang}\n{line}\n{rest}")

    def test_main_changing_the_gate_installs_it_and_runs_it(self):
        self.change_gate("printf 'main gate\\n' >&2")
        merged = self.commit("change the gate", at=self.later())
        self.main_moves_to(merged)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("installed origin/main's gate", result.stderr)
        # Main's gate ran this command, not the one it replaced.
        self.assertIn("main gate", result.stderr)
        self.assertEqual(self.recorded(), merged)
        # Once installed, the next run has nothing to do.
        again = self.run_installed()
        self.assertEqual((again.returncode, again.stdout, again.stderr), (0, "ran", "main gate\n"))

    def test_main_changing_nothing_of_the_gate_installs_nothing(self):
        (self.checkout / "README").write_text("unrelated\n")
        merged = self.commit("unrelated", at=self.later())
        self.main_moves_to(merged)
        before = self.target()
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), before)
        self.assertEqual(self.recorded(), self.first)
        self.assertEqual((self.destination / "cagents-cargo-gate.checked").read_text(),
                         merged + "\n")

    def test_an_installation_newer_than_mains_change_stays_until_main_changes_the_gate(self):
        # Installed from a branch under test after main last changed the gate.
        self.change_gate("# branch change")
        self.commit("branch")
        self.install()
        branch = self.target()
        self.git("checkout", "-q", "-b", "later", self.first)
        (self.checkout / "README").write_text("unrelated\n")
        self.main_moves_to(self.commit("unrelated", at=self.later()))
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), branch)
        # Main changes the gate after the branch was installed.
        self.change_gate("# another change")
        merged = self.commit("another change", at=self.later() + 1)
        self.main_moves_to(merged)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertNotEqual(self.target(), branch)
        self.assertEqual(self.recorded(), merged)

    def test_a_branch_main_merged_as_other_commits_is_current(self):
        self.change_gate("# branch change")
        self.commit("branch")
        self.install()
        installed = self.target()
        # Main squashes the branch: other commits, later, with the same files.
        squashed = self.git("commit-tree", "-p", self.first, "-m", "squash", "HEAD^{tree}",
                            env=self.git_env | {"GIT_COMMITTER_DATE": f"@{self.later()} +0000"})
        self.main_moves_to(squashed)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), installed)

    def test_an_installation_from_before_records_is_replaced_by_mains_newer_gate(self):
        (self.destination / "cagents-cargo-gate.installation").unlink()
        os.utime(self.installed, (self.clock, self.clock))
        self.change_gate("# merged change")
        merged = self.commit("change the gate", at=self.later())
        self.main_moves_to(merged)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("installed origin/main's gate", result.stderr)
        self.assertEqual(self.recorded(), merged)

    def test_a_gate_main_cannot_install_is_reported_and_not_retried(self):
        installer = self.checkout / "scripts/install-cargo-gate"
        installer.write_text("#!/bin/sh\necho broken installer >&2\nexit 1\n")
        broken = self.commit("break the installer", at=self.later())
        self.main_moves_to(broken)
        before = self.target()
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("could not install origin/main's gate", result.stderr)
        self.assertIn("broken installer", result.stderr)
        self.assertEqual(self.target(), before)
        again = self.run_installed()
        self.assertEqual((again.returncode, again.stdout), (0, "ran"), again.stderr)
        self.assertIn("could not be installed", again.stderr)
        self.assertNotIn("broken installer", again.stderr)

    def test_an_installer_that_records_nothing_is_not_run_again(self):
        installer = self.checkout / "scripts/install-cargo-gate"
        installer.write_text(installer.read_text().replace(
            'mv -f -- "$destination/cagents-cargo-gate.installation.new" '
            '"$destination/cagents-cargo-gate.installation"', ':'))
        self.change_gate("# merged change")
        merged = self.commit("an installer that records nothing", at=self.later())
        self.main_moves_to(merged)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("did not record what it installed", result.stderr)
        again = self.run_installed()
        self.assertEqual((again.returncode, again.stdout), (0, "ran"), again.stderr)
        self.assertIn("could not be installed", again.stderr)

    def test_a_gate_finding_another_installing_goes_on_and_installs_later(self):
        self.change_gate("# merged change")
        merged = self.commit("change the gate", at=self.later())
        self.main_moves_to(merged)
        before = self.target()
        with open(self.destination / "cagents-cargo-gate.install.lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), before)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertEqual(self.recorded(), merged)

    def test_a_merged_branch_counts_from_when_main_merged_it(self):
        # Installed from a branch main never takes.
        self.git("checkout", "-q", "-b", "unmerged")
        self.change_gate("# unmerged change")
        self.commit("unmerged")
        self.install()
        # A branch written before that installation, merged into main after.
        self.git("checkout", "-q", "-b", "written-earlier", self.first)
        self.change_gate("# merged change")
        self.commit("written earlier")
        self.git("checkout", "-q", "-b", "trunk", self.first)
        self.git("merge", "-q", "--no-ff", "-m", "merge", "written-earlier",
                 env=self.git_env | {"GIT_COMMITTER_DATE": f"@{self.later()} +0000"})
        merged = self.git("rev-parse", "HEAD")
        self.main_moves_to(merged)
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("installed origin/main's gate", result.stderr)
        self.assertEqual(self.recorded(), merged)

    def test_a_newer_gate_installed_from_changed_files_stays(self):
        gate = self.checkout / "scripts/cargo-gate"
        text = gate.read_text()
        version = int(re.search(r"^gate_version=(\d+)$", text, re.M).group(1))
        gate.write_text(text.replace(f"gate_version={version}\n", f"gate_version={version + 1}\n"))
        self.install()
        # Files that are not their commit's record no commit.
        self.assertIsNone(self.recorded())
        installed = self.target()
        gate.write_text(text + "# merged change\n")
        self.main_moves_to(self.commit("change the gate", at=self.later()))
        result = self.run_installed()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), installed)
        again = self.run_installed()
        self.assertEqual((again.returncode, again.stdout, again.stderr), (0, "ran", ""))

    def test_another_clone_of_the_same_repository_installs_mains(self):
        self.git("remote", "add", "origin", "https://github.com/example/gate.git")
        self.install()
        clone = self.root / "clone"
        subprocess.run(["git", "clone", "-q", str(self.checkout), str(clone)], env=self.git_env,
                       check=True, capture_output=True)
        def in_clone(*arguments, env=None):
            subprocess.run(["git", *arguments], cwd=clone, env=env or self.git_env, check=True,
                           capture_output=True)
        # Another way to the same repository, which ignores case.
        in_clone("remote", "set-url", "origin", "ssh://git@github.com:22/Example/gate")
        with open(clone / "scripts/cargo-gate", "a") as gate:
            gate.write("# merged change\n")
        in_clone("commit", "-qam", "change the gate",
                 env=self.git_env | {"GIT_COMMITTER_DATE": f"@{self.later()} +0000"})
        in_clone("update-ref", "refs/remotes/origin/main", "HEAD")
        result = self.run_installed(cwd=clone)
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertIn("installed origin/main's gate", result.stderr)
        self.assertEqual(self.record()["origin"], "ssh://git@github.com:22/Example/gate")

    def test_a_checkout_of_another_repository_changes_nothing(self):
        other = self.root / "other"
        (other / "scripts").mkdir(parents=True)
        for name in self.files:
            shutil.copy2(self.checkout / "scripts" / name, other / "scripts" / name)
        with open(other / "scripts/cargo-gate", "a") as gate:
            gate.write("# a fork's change\n")
        self.git("remote", "add", "origin", "https://github.com/example/gate.git")
        self.install()
        for arguments in (["init", "-q", "-b", "main"], ["add", "-A"],
                          ["remote", "add", "origin", "https://github.com/fork/gate.git"],
                          ["commit", "-q", "-m", "x"],
                          ["update-ref", "refs/remotes/origin/main", "HEAD"]):
            subprocess.run(["git", *arguments], cwd=other, env=self.git_env, check=True,
                           capture_output=True)
        before = self.target()
        result = self.run_installed(cwd=other)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "ran", ""))
        self.assertEqual(self.target(), before)
        self.assertEqual(self.recorded(), self.first)


class OwnedCommandSupervisor(unittest.TestCase):
    """scripts/cargo-gate-command.py on its own, with a command that reports
    which signals it ignores and waits."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="cagents-supervisor-", dir="/var/tmp")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.ignored = self.root / "ignored"
        self.command = [sys.executable, "-I", "-c", textwrap.dedent(f"""
            import pathlib, signal
            status = pathlib.Path("/proc/self/status").read_text().splitlines()
            mask = next(line.split()[1] for line in status if line.startswith("SigIgn:"))
            pathlib.Path({str(self.ignored)!r}).write_text(mask)
            signal.pause()
        """)]

    def supervise(self, *options, ignore=()):
        def ignoring():
            for number in ignore:
                signal.signal(number, signal.SIG_IGN)
        process = subprocess.Popen(
            [sys.executable, "-I", str(ROOT / "scripts/cargo-gate-command.py"), *options, "/dev/null",
             *self.command], stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
            preexec_fn=ignoring)
        self.addCleanup(lambda: process.poll() is None and (process.kill(), process.wait()))
        deadline = time.monotonic() + HANG_GUARD
        while not self.ignored.exists():
            self.assertLess(time.monotonic(), deadline, "the command never started")
            if process.poll() is not None:
                self.fail(process.stderr.read())
            time.sleep(0.02)
        return process

    def test_a_signal_the_caller_ignores_cancels_nothing_and_stays_ignored(self):
        # As under nohup.
        process = self.supervise(ignore=(signal.SIGHUP,))
        self.assertTrue(int(self.ignored.read_text(), 16) & (1 << (signal.SIGHUP - 1)))
        process.send_signal(signal.SIGHUP)
        time.sleep(0.3)
        self.assertIsNone(process.poll())
        process.terminate()
        self.assertEqual(process.wait(timeout=HANG_GUARD), 128 + signal.SIGTERM)

    def test_a_wrapper_dies_by_the_signal_that_cancelled_it_and_names_itself(self):
        process = self.supervise("--wrapper", "some-wrapper")
        process.send_signal(signal.SIGINT)
        self.assertEqual(process.wait(timeout=HANG_GUARD), -signal.SIGINT)
        failed = subprocess.run(
            [sys.executable, "-I", str(ROOT / "scripts/cargo-gate-command.py"), "--wrapper", "some-wrapper",
             "/dev/null", str(self.root / "missing")], capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(failed.returncode, 127)
        self.assertTrue(failed.stderr.startswith("some-wrapper: "), failed.stderr)


class CompilerGuard(unittest.TestCase):
    """scripts/cargo-gate-rustc-guard on its own, with stand-in compilers that
    record what reaches them: what only asks the compiler something passes,
    and anything that would compile here is refused."""

    QUERIES = ([], ["-vV"], ["-V"], ["--version"], ["-h"], ["--help"], ["--explain", "E0382"],
               ["--explain=E0382"], ["-Chelp"], ["-C", "help"], ["-Zhelp"], ["-Whelp"],
               ["--print", "sysroot"], ["--print=cfg"], ["--print=file-names=names.txt"],
               # Cargo's query of the target: it prints and stops.
               ["-", "--crate-name", "___", "--print=file-names", "--crate-type", "bin"],
               # Any print but those made while linking stops before compiling.
               ["--print=native-static-libs", "--print=cfg", "main.rs"])
    COMPILES = (["main.rs"], ["-v", "main.rs"], ["-C", "opt-level=3", "main.rs"],
                ["--crate-name", "probe", "--edition=2021", "src/main.rs"],
                # Printed while linking, after compiling.
                ["--crate-name", "probe", "--print=native-static-libs", "src/lib.rs"],
                ["--print", "native-static-libs", "main.rs"], ["--print=link-args", "main.rs"],
                ["--print=link-args=args.txt", "main.rs"])

    TOOLS = ("rustc", "rustdoc", "clippy-driver")

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="cagents-compiler-guard-", dir="/var/tmp")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.log = self.root / "log"
        self.guard = ROOT / "scripts/cargo-gate-rustc-guard"
        # An outer `exec` command's gate can be another copy of the guard.
        copy = self.root / "copy"
        copy.mkdir()
        shutil.copy2(self.guard, copy / self.guard.name)
        self.real = self.root / "real"
        self.shims = [self.root / "shims", self.root / "outer-shims"]
        # Like mise's shim for a tool it does not manage here: it runs the
        # first one on PATH outside its own directory, and gives up when that
        # keeps coming back to it.
        mise = self.root / "mise"
        for directory in (self.real, *self.shims, mise):
            directory.mkdir()
        for tool in self.TOOLS:
            (self.real / tool).write_text(
                f'#!/bin/sh\nprintf "{tool} %s\\n" "$*" >>"{self.log}"\n')
            (self.real / tool).chmod(0o755)
            (self.shims[0] / tool).symlink_to(self.guard)
            (self.shims[1] / tool).symlink_to(copy / self.guard.name)
            (mise / tool).write_text(textwrap.dedent(f"""\
                #!/bin/sh
                [ "${{MISE_DEPTH:-0}}" -lt 3 ] || {{ echo "mise: shim fallback recursion detected" >&2; exit 1; }}
                export MISE_DEPTH=$((${{MISE_DEPTH:-0}} + 1))
                here=$(dirname "$0")
                IFS=:
                for entry in $PATH; do
                    [ "$entry" != "$here" ] && [ -x "$entry/{tool}" ] && exec "$entry/{tool}" "$@"
                done
                exit 127
            """))
            (mise / tool).chmod(0o755)
        self.env = {"PATH": os.pathsep.join([*map(str, self.shims), str(mise), str(self.real),
                                             "/usr/bin", "/bin"])}

    def run_guard(self, command, **env):
        self.log.unlink(missing_ok=True)
        result = subprocess.run(command, env=self.env | env, cwd=self.root, capture_output=True,
                                text=True, timeout=HANG_GUARD)
        ran = [line.rstrip() for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, ran

    def test_a_compiler_run_directly_only_answers_queries(self):
        for tool in self.TOOLS:
            for arguments in self.QUERIES:
                with self.subTest(tool=tool, arguments=arguments):
                    result, ran = self.run_guard([str(self.shims[0] / tool), *arguments])
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(ran, [" ".join([tool, *arguments])])
            for arguments in self.COMPILES:
                with self.subTest(tool=tool, arguments=arguments):
                    result, ran = self.run_guard([str(self.shims[0] / tool), *arguments])
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(ran, [])
                    self.assertIn(f"`{tool}` run directly inside `cargo-gate exec` would compile on this host",
                                  result.stderr)

    def test_as_cargos_wrapper_it_only_passes_queries_to_the_callers_wrapper(self):
        wrapper = self.root / "callers-wrapper"
        wrapper.write_text(f'#!/bin/sh\nprintf "wrapper %s\\n" "$*" >>"{self.log}"\nexec "$@"\n')
        wrapper.chmod(0o755)
        rustc = str(self.real / "rustc")
        for given in ({}, {"CAGENTS_BUILD_EXEC_RUSTC_WRAPPER": str(wrapper)}):
            for arguments in self.QUERIES[1:]:
                with self.subTest(given=given, arguments=arguments):
                    result, ran = self.run_guard(
                        [str(self.guard), rustc, *arguments], **given)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    expected = [" ".join(["rustc", *arguments])]
                    if given:
                        expected.insert(0, " ".join(["wrapper", rustc, *arguments]))
                    self.assertEqual(ran, expected)
            for arguments in self.COMPILES:
                with self.subTest(given=given, arguments=arguments):
                    result, ran = self.run_guard(
                        [str(self.guard), rustc, *arguments], **given)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(ran, [])
                    self.assertIn("would compile on this host", result.stderr)
                    self.assertIn("CAGENTS_BUILD_REMOTE=local", result.stderr)

    def test_a_compiler_missing_beyond_the_guard_is_named(self):
        self.env["PATH"] = os.pathsep.join([str(self.shims[0]), "/nonexistent"])
        result, ran = self.run_guard([str(self.shims[0] / "rustc"), "-vV"])
        self.assertEqual(result.returncode, 127)
        self.assertIn("no rustc on PATH besides cargo-gate's guard", result.stderr)


class HeldCommand(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise unittest.SkipTest("Linux pidfds are required")
        # Importing fixture_custody made this process the gates' subreaper.

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-held-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = copy_gate(self.root)
        self.lock = self.root / "slot.lock"
        self.env = os.environ | {
            "CAGENTS_BUILD_LOCK": str(self.lock),
            "CAGENTS_BUILD_SLOTS": "1",
            "CARGO_BUILD_JOBS": "1",
            # This copy of the gate, not a newer installed one, building here.
            "CAGENTS_BUILD_INSTALLED_GATE": "",
            "CAGENTS_BUILD_REMOTE": "local",
            # A gate these tests kill leaves its command directory behind.
            "TMPDIR": str(self.root),
        }
        self.env.pop("CAGENTS_BUILD_SLOT_HELD", None)
        self.children = []
        self.descendants = []
        self.addCleanup(self.cleanup_processes)

    def cleanup_processes(self):
        # Every pidfd comes from a private ready receipt or a verified child
        # of this fixture's live gate. Release graceful handlers first, then
        # kill only the original owned handles.
        (self.root / "release").touch()
        for _pid, pidfd in self.descendants:
            if not select.select([pidfd], [], [], 0)[0]:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=HANG_GUARD)
            if child.stdout:
                child.stdout.close()
            if child.stderr:
                child.stderr.close()
        for pid, pidfd in self.descendants:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass  # The fixed supervisor already reaped it.
            os.close(pidfd)

    def wait_for(self, predicate, label, timeout=HANG_GUARD):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                self.fail(f"timed out waiting for {label}")
            time.sleep(0.01)

    def slot_is_free(self):
        with self.lock.open("a") as slot:
            try:
                fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False

    def launch_background_service(self):
        # A held command that leaves a service running behind it, as a build
        # starting a compiler cache does.
        worker = self.root / "worker.py"
        worker.write_text("""
import json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
(root / 'ready.tmp').write_text(json.dumps({'pid': os.getpid()}))
(root / 'ready.tmp').rename(root / 'ready')
while True:
    time.sleep(1)
""")
        child = subprocess.Popen(
            [str(self.gate), "exec", "/bin/sh", "-c",
             '"$1" "$2" "$3" & while [ ! -f "$3/ready" ]; do sleep 0.01; done', "fixture",
             sys.executable, str(worker), str(self.root)],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.children.append(child)
        self.wait_for(lambda: (self.root / "ready").exists(), "descendant ready")
        pid = json.loads((self.root / "ready").read_text())["pid"]
        pidfd = os.pidfd_open(pid)
        self.descendants.append((pid, pidfd))
        return child, pidfd

    def test_success_leaves_background_cache_services_outside_the_slot(self):
        child, pidfd = self.launch_background_service()
        self.assertEqual(child.wait(timeout=HANG_GUARD), 0)
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        self.assertTrue(self.slot_is_free())

    def test_cancelling_a_held_command_interrupts_it_and_frees_its_shares(self):
        # The command answers an interrupt, as Cargo does by giving back its
        # jobserver tokens; whoever cancels the gate cancels it.
        command = self.root / "interruptible.py"
        command.write_text("""
import pathlib, signal, sys, time
root = pathlib.Path(sys.argv[1])
def stop(number, _frame):
    (root / 'received').write_text(signal.Signals(number).name)
    sys.exit(130)
for number in (signal.SIGINT, signal.SIGTERM):
    signal.signal(number, stop)
(root / 'ready').touch()
while True:
    time.sleep(0.05)
""")
        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            with self.subTest(signal=number):
                for name in ("ready", "received"):
                    (self.root / name).unlink(missing_ok=True)
                child = subprocess.Popen(
                    [str(self.gate), "exec", sys.executable, str(command), str(self.root)],
                    env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
                )
                self.children.append(child)
                try:
                    self.wait_for((self.root / "ready").exists, "the held command")
                    self.assertFalse(self.slot_is_free(), "the held command owns its share")
                    child.send_signal(number)
                    status = child.wait(timeout=HANG_GUARD)
                    self.assertIn(status, (-number, 128 + number))
                    self.wait_for((self.root / "received").exists, "the command's interrupt")
                    self.assertEqual((self.root / "received").read_text(), "SIGINT")
                    # Nothing of the gate is left holding the share or waiting on it.
                    self.wait_for(self.slot_is_free, "the share of a cancelled gate")
                    self.assertEqual(list(self.root.glob("cagents-cargo-shim.*")), [])
                finally:
                    # Whatever the gate left in its group ends before the next signal.
                    self.end_group(child.pid)
                    self.wait_for(self.slot_is_free, "the group's share")

    def run_gate(self, *arguments, **options):
        return subprocess.run(
            [str(self.gate), *arguments], env=self.env,
            capture_output=True, text=True, timeout=HANG_GUARD, **options,
        )

    def test_exit_status_and_missing_command_are_preserved(self):
        for status in (0, 17):
            with self.subTest(status=status):
                result = self.run_gate("exec", "/bin/sh", "-c", f"exit {status}")
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertTrue(self.slot_is_free())
        result = self.run_gate("exec", str(self.root / "missing"))
        self.assertEqual(result.returncode, 127, result.stderr)
        self.assertTrue(self.slot_is_free())

    def test_nested_exec_uses_the_held_slot(self):
        result = self.run_gate("exec", str(self.gate), "exec", "/bin/sh", "-c", "exit 19")
        self.assertEqual(result.returncode, 19, result.stderr)
        self.assertTrue(self.slot_is_free())

    def test_stdin_and_caller_descriptor_reach_the_command(self):
        with (self.root / "caller-output").open("w") as output:
            descriptor = fcntl.fcntl(output.fileno(), fcntl.F_DUPFD_CLOEXEC, 20)
            try:
                result = self.run_gate(
                    "exec", sys.executable, "-c",
                    "import os,sys; os.write(int(sys.argv[1]), sys.stdin.buffer.read())",
                    str(descriptor), pass_fds=(descriptor,), input="caller data\n",
                )
            finally:
                os.close(descriptor)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "caller-output").read_text(), "caller data\n")

    def test_exec_preserves_the_controlling_terminal(self):
        child, terminal = pty.fork()
        if child == 0:
            os.execve(self.gate, [
                str(self.gate), "exec", sys.executable, "-c",
                "open('/dev/tty').close(); print('controlling-terminal-ok')",
            ], self.env)
        pidfd = os.pidfd_open(child)
        output = bytearray()
        try:
            deadline = time.monotonic() + HANG_GUARD
            while True:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, "terminal command did not finish")
                ready, _, _ = select.select([terminal], [], [], remaining)
                self.assertTrue(ready, "terminal command did not finish")
                try:
                    chunk = os.read(terminal, 4096)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not chunk:
                    break
                output.extend(chunk)
            _, status = os.waitpid(child, 0)
            self.assertEqual(os.waitstatus_to_exitcode(status), 0, output.decode())
            self.assertIn(b"controlling-terminal-ok", output)
        finally:
            if not select.select([pidfd], [], [], 0)[0]:
                # pty.fork made this exact unreaped fixture child its own
                # session/group leader; no other command shares that group.
                os.killpg(child, signal.SIGKILL)
                os.waitpid(child, 0)
            os.close(pidfd)
            os.close(terminal)

    def test_cancelled_build_does_not_run_the_second_cargo_phase(self):
        # Real gate, stub Cargo: cancellation remains cancellation even when
        # the build command handles TERM and returns success.
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!{sys.executable}
import pathlib, signal, sys, time
root = pathlib.Path({str(self.root)!r})
with (root / 'cargo-calls').open('a') as calls:
    calls.write(' '.join(sys.argv[1:]) + '\\n')
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
(root / 'build-ready').touch()
while True:
    time.sleep(1)
""")
        cargo.chmod(0o755)
        self.env["PATH"] = f"{binary}:{self.env['PATH']}"
        for command, number in (("test", signal.SIGTERM), ("bench", signal.SIGINT), ("run", signal.SIGHUP)):
            with self.subTest(command=command, signal=number):
                ready = self.root / "build-ready"
                calls = self.root / "cargo-calls"
                child = subprocess.Popen(
                    [str(self.gate), command, "--locked", "--", "program-argument"],
                    env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    start_new_session=True,
                )
                self.children.append(child)
                self.wait_for(ready.exists, "stub Cargo build")
                child.send_signal(number)
                # The gate is build-host by now, which ends as the signal would.
                self.assertEqual(child.wait(timeout=HANG_GUARD), -number)
                expected = "build --locked" if command == "run" else f"{command} --no-run --locked"
                self.assertEqual(calls.read_text().splitlines(), [expected])
                self.assertTrue(self.slot_is_free())
                ready.unlink()
                calls.unlink()

    def leftover_daemon_fixture(self):
        # A Cargo that leaves a daemon behind, in the build phase or the test run,
        # as a fixture cagentsd did when its `cargo test` was killed. Both stay
        # alive past the hang guard; cleanup kills them once they are known.
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!/bin/sh
root={str(self.root)!r}
case " $* " in
    *" --no-run "*) [ -e "$root/in-build" ] || exit 0 ;;
esac
setsid sleep {10 * HANG_GUARD} &
printf '%s %s' "$!" "$$" > "$root/ready.new"
mv "$root/ready.new" "$root/ready"
exec sleep {10 * HANG_GUARD}
""")
        cargo.chmod(0o755)
        self.env["PATH"] = f"{binary}:{self.env['PATH']}"

    def open_files(self, pid):
        # A process still starting closes files as it loads, so one listed may be gone.
        files = []
        for entry in Path(f"/proc/{pid}/fd").iterdir():
            with contextlib.suppress(FileNotFoundError):
                files.append(os.readlink(entry))
        return files

    def kill_gate_and_check_the_daemon(self, child):
        self.wait_for((self.root / "ready").exists, "the leftover daemon")
        daemon, stub = map(int, (self.root / "ready").read_text().split())
        for pid in (daemon, stub):
            with contextlib.suppress(ProcessLookupError):
                self.descendants.append((pid, os.pidfd_open(pid)))
        child.kill()
        child.wait(timeout=HANG_GUARD)
        os.kill(daemon, 0)
        self.assertNotIn(str(self.lock), self.open_files(daemon))
        self.wait_for(self.slot_is_free, "the slot of a killed gate")
        os.kill(daemon, 0)

    def test_a_daemon_from_a_killed_test_run_does_not_keep_the_slot(self):
        self.leftover_daemon_fixture()
        child = subprocess.Popen(
            [str(self.gate), "test", "--locked"], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(child)
        self.kill_gate_and_check_the_daemon(child)

    def test_a_daemon_from_a_killed_build_does_not_keep_the_slot(self):
        self.leftover_daemon_fixture()
        (self.root / "in-build").touch()
        child = subprocess.Popen(
            [str(self.gate), "test", "--locked"], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(child)
        self.kill_gate_and_check_the_daemon(child)

    def test_under_flock_close_a_gated_command_uses_the_callers_slot(self):
        result = subprocess.run(
            ["flock", "--close", str(self.lock), str(self.gate), "exec", "true"],
            env=self.env, capture_output=True, text=True, timeout=HANG_GUARD,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def under_flock(self, slot, *inner, env=None):
        """A gated `exec true` under `flock --close slot`, through `inner`, without waiting long."""
        return subprocess.run(
            ["flock", "--close", str(slot), *inner, str(self.gate), "exec", "true"],
            env=env or self.env, capture_output=True, text=True, timeout=HANG_GUARD,
        )

    def hold_slot_elsewhere(self, slot=None):
        holder = subprocess.Popen(["flock", "--close", str(slot or self.lock), "sleep", "infinity"],
                                  start_new_session=True)
        self.children.append(holder)
        self.addCleanup(self.end_group, holder.pid)
        self.wait_for(lambda: self.is_locked(slot or self.lock), "the other holder")

    def is_locked(self, path):
        with Path(path).open("a") as probe:
            try:
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
        return False

    def assert_gate_queues(self, *command, env=None):
        child = subprocess.Popen(
            [*command, str(self.gate), "exec", "true"], env=env or self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(child)
        self.addCleanup(self.end_group, child.pid)
        time.sleep(1)
        self.assertIsNone(child.poll(), "the gate must queue")

    def test_a_flock_further_up_still_counts_as_the_callers_slot(self):
        script = f'"{self.gate}" exec true; :'
        result = subprocess.run(
            ["flock", "--close", str(self.lock), "sh", "-c", f"sh -c '{script}'; :"],
            env=self.env, capture_output=True, text=True, timeout=HANG_GUARD,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def test_flock_on_a_numbered_slot_counts_only_if_that_slot_exists(self):
        second = Path(f"{self.lock}.1")
        result = self.under_flock(second, env=self.env | {"CAGENTS_BUILD_SLOTS": "2"})
        self.assertEqual(result.returncode, 0, result.stderr)
        # Run in that slot, not admitted to a free one: an admitted command gets the marker.
        result = subprocess.run(
            ["flock", "--close", f"{self.lock}.10", str(self.gate), "exec",
             "sh", "-c", 'test -z "${CAGENTS_BUILD_SLOT_HELD+x}"'],
            env=self.env | {"CAGENTS_BUILD_SLOTS": "12"}, capture_output=True, text=True,
            timeout=HANG_GUARD,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.hold_slot_elsewhere()
        self.assert_gate_queues("flock", "--close", str(second))

    def test_flock_on_the_queue_or_another_file_is_not_a_slot(self):
        self.hold_slot_elsewhere()
        for name in (".queue", ".cost", ".1.queue", ".00"):
            self.assert_gate_queues("flock", "--close", f"{self.lock}{name}")

    def test_flock_through_a_symlink_counts_as_the_callers_slot(self):
        link = self.root / "link.lock"
        link.symlink_to(self.lock)
        result = self.under_flock(self.lock, env=self.env | {"CAGENTS_BUILD_LOCK": str(link)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def test_a_flock_with_a_slot_open_but_not_locked_is_not_holding_it(self):
        # flock --close closes only the lock it took, so this one keeps the slot open.
        other = self.root / "other.lock"
        self.hold_slot_elsewhere()
        with self.lock.open("a") as opened:
            child = subprocess.Popen(
                ["flock", "--close", str(other), str(self.gate), "exec", "true"],
                env=self.env, pass_fds=(opened.fileno(),),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
            )
        self.children.append(child)
        self.addCleanup(self.end_group, child.pid)
        time.sleep(1)
        self.assertIsNone(child.poll(), "an open descriptor is not a held slot")

    def test_a_leftover_that_is_not_flock_does_not_admit_what_it_starts(self):
        # A held slot on an ancestor that isn't flock, and that closes it for its children.
        script = f'exec 7>>"{self.lock}"; flock 7; "{self.gate}" exec true 7>&-; :'
        self.assert_gate_queues_script(script)

    def assert_gate_queues_script(self, script):
        ready = self.root / "started"
        child = subprocess.Popen(
            ["sh", "-c", f': > "{ready}"; {script}'], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(child)
        self.addCleanup(self.end_group, child.pid)
        self.wait_for(ready.exists, "the holder")
        time.sleep(1)
        self.assertIsNone(child.poll(), "only a running flock's slot counts")

    def wait_for_gate_request(self, gate):
        """Pin this private gate's waiting tree until its build-host waits on
        the queue: blocked on it behind the head, or holding it as the head."""
        pinned = {pid for pid, _pidfd in self.descendants}
        queue = os.path.realpath(f"{self.lock}.queue")

        def requesting():
            parents = [gate.pid]
            found = False
            for parent in parents:
                try:
                    children = Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
                except (FileNotFoundError, ProcessLookupError):
                    continue
                for value in children:
                    pid = int(value)
                    try:
                        pidfd = os.pidfd_open(pid)
                    except ProcessLookupError:
                        continue
                    try:
                        # Bind the handle to this still-live owned parent before
                        # registering it for cleanup; a recycled unrelated PID
                        # must never become a fixture descendant.
                        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                        if int(stat[1]) != parent:
                            continue
                        if pid not in pinned:
                            self.descendants.append((pid, pidfd))
                            pinned.add(pid)
                            pidfd = None
                        parents.append(pid)
                        found |= self.waits_on_queue(pid, queue)
                    except (FileNotFoundError, ProcessLookupError):
                        pass
                    finally:
                        if pidfd is not None:
                            os.close(pidfd)
            # The gate itself becomes build-host when it builds Cargo.
            return found or self.waits_on_queue(gate.pid, queue)

        self.wait_for(requesting, "the gate's owned lock request")

    def waits_on_queue(self, pid, queue):
        try:
            arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            if not any(argument.endswith(b"/build-host") for argument in arguments):
                return False
            for descriptor in Path(f"/proc/{pid}/fd").iterdir():
                with contextlib.suppress(FileNotFoundError):
                    if os.readlink(descriptor) == queue:
                        return True
        except (FileNotFoundError, ProcessLookupError):
            pass
        return False

    def assert_killed_waiter_releases_callers(self, *, behind_head, command):
        self.hold_slot_elsewhere()
        slot_holder = self.children[-1]
        head = None
        if behind_head:
            head = subprocess.Popen(
                [str(self.gate), "exec", "true"], env=self.env,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )
            self.children.append(head)
            self.addCleanup(self.end_group, head.pid)
            self.wait_for(lambda: self.is_locked(f"{self.lock}.queue"), "the live queue head")
            self.wait_for_gate_request(head)

        unrelated = self.root / "unrelated.lock"
        reader, writer = os.pipe()
        self.addCleanup(os.close, reader)
        descriptor = fcntl.fcntl(writer, fcntl.F_DUPFD_CLOEXEC, 20)
        os.close(writer)
        try:
            with unrelated.open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                inherited_lock = fcntl.fcntl(lock.fileno(), fcntl.F_DUPFD_CLOEXEC, 20)
                try:
                    gate = subprocess.Popen(
                        [str(self.gate), *command], env=self.env,
                        pass_fds=(descriptor, inherited_lock),
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
                    )
                finally:
                    os.close(inherited_lock)
        finally:
            os.close(descriptor)
        self.children.append(gate)
        self.addCleanup(self.end_group, gate.pid)
        self.wait_for_gate_request(gate)
        self.assertTrue(self.is_locked(unrelated), "the gate inherited the caller's lock")
        self.assertFalse(select.select([reader], [], [], 0)[0], "the gate inherited the extra pipe")
        gate.kill()
        # Nothing else frees what the gate's waiters hold: the slot's holder
        # never lets go, so releasing them at all shows they died with the gate.
        deadline = time.monotonic() + HANG_GUARD
        gate.wait(timeout=HANG_GUARD)
        pending = {gate.stdout.fileno(): "stdout", gate.stderr.fileno(): "stderr", reader: "extra pipe"}
        while pending or self.is_locked(unrelated):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            for stream in select.select(list(pending), [], [], min(remaining, 0.01))[0]:
                if not os.read(stream, 4096):
                    pending.pop(stream)
        self.assertIsNone(slot_holder.poll(), "the original slot holder must stay alive")
        self.assertFalse(self.slot_is_free(), "the original slot must stay occupied")
        if head is not None:
            self.assertIsNone(head.poll(), "the live queue head must stay alive")
            self.assertTrue(self.is_locked(f"{self.lock}.queue"), "the live head keeps its queue")
        retained = dict(streams=list(pending.values()), unrelated_lock_held=self.is_locked(unrelated))
        self.assertEqual(retained, dict(streams=[], unrelated_lock_held=False),
                         "killed waiter retained caller resources")

    # `exec` runs build-host beside the gate's shell; a Cargo command's gate
    # becomes build-host.
    SCRIPT, CARGO = ("exec", "true"), ("check", "--locked")

    def test_killed_queue_head_releases_extra_descriptors_without_releasing_holder(self):
        self.assert_killed_waiter_releases_callers(behind_head=False, command=self.SCRIPT)

    def test_killed_cargo_queue_head_releases_extra_descriptors_without_releasing_holder(self):
        self.assert_killed_waiter_releases_callers(behind_head=False, command=self.CARGO)

    def test_killed_waiter_behind_live_head_releases_all_caller_descriptors(self):
        self.assert_killed_waiter_releases_callers(behind_head=True, command=self.SCRIPT)

    def test_killed_cargo_waiter_behind_live_head_releases_all_caller_descriptors(self):
        self.assert_killed_waiter_releases_callers(behind_head=True, command=self.CARGO)

    def test_a_killed_gate_leaves_nobody_holding_the_queue(self):
        self.assert_killed_gate_frees_the_queue(self.SCRIPT)

    def test_a_killed_cargo_gate_leaves_nobody_holding_the_queue(self):
        self.assert_killed_gate_frees_the_queue(self.CARGO)

    def assert_killed_gate_frees_the_queue(self, command):
        self.hold_slot_elsewhere()
        queue = Path(f"{self.lock}.queue")
        gate = subprocess.Popen(
            [str(self.gate), *command], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(gate)
        self.addCleanup(self.end_group, gate.pid)
        self.wait_for(lambda: self.is_locked(queue), "the gate at the head of the queue")
        self.wait_for_gate_request(gate)
        gate.kill()
        gate.wait(timeout=HANG_GUARD)
        self.wait_for(lambda: not self.is_locked(queue), "the queue to free")
        # Read to EOF without blocking: anything the gate wrote first also reads ready.
        for stream in (gate.stdout, gate.stderr):
            deadline = time.monotonic() + HANG_GUARD
            while select.select([stream], [], [], max(0, deadline - time.monotonic()))[0]:
                if not os.read(stream.fileno(), 4096):
                    break
            else:
                self.fail("a stream stayed open")

    def end_group(self, group):
        with contextlib.suppress(ProcessLookupError):
            os.killpg(group, signal.SIGKILL)

    def test_an_unrelated_holder_is_still_waited_for(self):
        self.hold_slot_elsewhere()
        self.assert_gate_queues()

    def test_successful_split_preserves_cargo_and_program_arguments(self):
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!{sys.executable}
import json, pathlib, sys
with pathlib.Path({str(self.root / 'cargo-calls')!r}).open('a') as calls:
    calls.write(json.dumps(sys.argv[1:]) + '\\n')
""")
        cargo.chmod(0o755)
        self.env["PATH"] = f"{binary}:{self.env['PATH']}"
        for command in ("test", "bench", "run"):
            with self.subTest(command=command):
                result = self.run_gate(command, "--locked", "--", "argument with spaces")
                self.assertEqual(result.returncode, 0, result.stderr)
                first = ["build", "--locked"] if command == "run" else [command, "--no-run", "--locked"]
                calls = self.root / "cargo-calls"
                self.assertEqual(
                    [json.loads(line) for line in calls.read_text().splitlines()],
                    [first, [command, "--locked", "--", "argument with spaces"]],
                )
                calls.unlink()


PREFLIGHT_TOOL = r'''#!/usr/bin/python3
import fcntl
import json
import os
from pathlib import Path
import signal
import sys
import time

root = Path(os.environ["FIXTURE_ROOT"])
config = json.loads((root / "control.json").read_text())
program = Path(sys.argv[0]).name
phase = ("metadata" if program == "cargo" and sys.argv[1:2] == ["metadata"] else
         "compiler" if program == "cargo" else
         "guard" if program == "build-disk-guard" else "seed")
with open(os.environ["CAGENTS_BUILD_LOCK"], "a") as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        held = False
    except BlockingIOError:
        held = True
event = dict(phase=phase, pid=os.getpid(), arguments=sys.argv[1:], held=held,
             marker=os.environ.get("CAGENTS_BUILD_SLOT_HELD", ""),
             protected={name: os.environ[name] for name in
                        ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")})

def record(entry):
    with (root / "calls.jsonl").open("a") as output:
        output.write(json.dumps(entry) + "\n")

# The test or its cleanup releases every hold; this bounds only a test that does neither.
HOLD_SECONDS = 600

def interrupted(number, _frame):
    (root / (phase + ".signalled")).write_text(str(number))
    deadline = time.monotonic() + HOLD_SECONDS
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    raise SystemExit(128 + number)

for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
    signal.signal(number, interrupted)
record(event)
if config.get("hold") == phase:
    receipt = root / (phase + ".ready")
    receipt.with_suffix(".tmp").write_text(json.dumps(event))
    receipt.with_suffix(".tmp").replace(receipt)
    deadline = time.monotonic() + HOLD_SECONDS
    while not (root / "release").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    if not (root / "release").exists():
        raise SystemExit(95)

if phase == "metadata":
    if config.get("metadata_exit"):
        raise SystemExit(config["metadata_exit"])
    metadata = (dict(target_directory=config["target_directory"]) if "target_directory" in config else
                dict(build_directory=config.get("build_directory", str(root / "owned-build"))))
    print(json.dumps(metadata, separators=(",", ":")))
    sys.stdout.flush()
    if config.get("metadata_exit_after_output"):
        raise SystemExit(config["metadata_exit_after_output"])
elif phase == "compiler":
    data = sys.stdin.buffer.read()
    if "FIXTURE_OUTPUT_FD" in os.environ:
        os.write(int(os.environ["FIXTURE_OUTPUT_FD"]), data)
    record(dict(phase="compiler-stdin", stdin=data.decode()))
    print("private compiler output")
raise SystemExit(config.get(phase + "_exit", 0))
'''


class PreflightCancellation(unittest.TestCase):
    """Actual gate preflight with private fake tools and exact child custody."""

    @classmethod
    def setUpClass(cls):
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise unittest.SkipTest("Linux pidfds are required")
        # Importing fixture_custody made this process the gates' subreaper.

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="gate-preflight-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        copy_gate(self.root)
        (self.root / "bin").mkdir()
        for name in ("bin/cargo", "build-disk-guard", "seed-build-dir"):
            path = self.root / name
            path.write_text(PREFLIGHT_TOOL)
            path.chmod(0o700)
        for name in ("home", "codex", "tmp"):
            (self.root / name).mkdir(mode=0o700)
        self.lock = self.root / "slot.lock"
        self.env = {
            "PATH": str(self.root / "bin") + ":/usr/bin:/bin",
            "HOME": str(self.root / "home"), "CODEX_HOME": str(self.root / "codex"),
            "CAGENTS_AGENT_AUTHORITY": "private-preflight-authority",
            "CAGENTS_BUILD_LOCK": str(self.lock), "CAGENTS_BUILD_SLOTS": "1",
            "CARGO_BUILD_JOBS": "1", "FIXTURE_ROOT": str(self.root),
            "TMPDIR": str(self.root / "tmp"), "LC_ALL": "C",
            "CAGENTS_BUILD_REMOTE": "local", "CAGENTS_BUILD_INSTALLED_GATE": "",
            "CAGENTS_BUILD_SCOPE": "0",
        }
        self.children = []
        self.descendants = []
        self.addCleanup(self.cleanup_processes)
        self.configure()

    def configure(self, **config):
        (self.root / "control.json").write_text(json.dumps(config))

    def cleanup_processes(self):
        (self.root / "release").touch()
        for _pid, pidfd in self.descendants:
            if not select.select([pidfd], [], [], 0)[0]:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        for process in self.children:
            if process.poll() is None:
                # Only this fixture's unreaped session leader reserves this
                # group ID; all its descendants are inert private tools.
                os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=HANG_GUARD)
        for pid, pidfd in self.descendants:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
            os.close(pidfd)

    def clear(self):
        for name in ("calls.jsonl", "release"):
            (self.root / name).unlink(missing_ok=True)
        for pattern in ("*.ready", "*.signalled"):
            for path in self.root.glob(pattern):
                path.unlink()

    def start(self, *, environment=None, pass_fds=()):
        process = subprocess.Popen(
            [str(self.root / "cargo-gate"), "test", "--locked", "--no-run"],
            cwd=self.root, env=self.env if environment is None else environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, start_new_session=True, pass_fds=pass_fds,
        )
        self.children.append(process)
        return process

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def slot_is_free(self):
        with self.lock.open("a") as slot:
            try:
                fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False

    def wait_for(self, predicate, label, timeout=HANG_GUARD):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                self.fail("timed out waiting for " + label)
            time.sleep(0.01)

    def assert_preflight_unslotted(self, calls):
        for event in calls:
            if event["phase"] not in ("compiler", "compiler-stdin"):
                self.assertFalse(event["held"], event)
                self.assertFalse(event["marker"], event)
                self.assertEqual(event["protected"], {
                    name: self.env[name] for name in ("HOME", "CODEX_HOME", "CAGENTS_AGENT_AUTHORITY")
                })

    def test_each_preflight_phase_forwards_cancellation_and_drains_before_exit(self):
        # The gate kills what is still draining after its grace; this fixture
        # drains until released, so a grace longer than the hang guard keeps a
        # slow observation of that drain from racing the kill.
        helper = self.root / "cargo-gate-command.py"
        source = helper.read_text()
        self.assertEqual(source.count("GRACE_SECONDS = 2.0\n"), 1)
        helper.write_text(source.replace("GRACE_SECONDS = 2.0\n", f"GRACE_SECONDS = {10 * HANG_GUARD}\n"))
        for phase in ("metadata", "guard", "seed"):
            for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
                with self.subTest(phase=phase, signal=number):
                    self.clear()
                    self.configure(hold=phase)
                    process = self.start()
                    ready = self.root / (phase + ".ready")
                    self.wait_for(ready.exists, phase + " ready")
                    pid = json.loads(ready.read_text())["pid"]
                    pidfd = os.pidfd_open(pid)
                    self.descendants.append((pid, pidfd))
                    self.assertTrue(self.slot_is_free())
                    process.send_signal(number)
                    deadline = time.monotonic() + HANG_GUARD
                    receipt = self.root / (phase + ".signalled")
                    while not receipt.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    forwarded = receipt.exists()
                    running = process.poll() is None
                    child_alive = not select.select([pidfd], [], [], 0)[0]
                    slot_free = self.slot_is_free()
                    (self.root / "release").touch()
                    _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                    self.wait_for(lambda: bool(select.select([pidfd], [], [], 0)[0]), "exact child cleanup")
                    try:
                        caller_reaped = os.waitpid(pid, os.WNOHANG)[0] == pid
                    except ChildProcessError:
                        caller_reaped = False
                    calls = self.calls()
                    late_builds = [event for event in calls if event["phase"] == "compiler"]
                    observation = dict(forwarded=forwarded, gate_running=running,
                                       child_alive=child_alive, late_builds=late_builds,
                                       caller_had_to_reap_child=caller_reaped)
                    self.assertTrue(forwarded, observation)
                    self.assertTrue(running, "gate exited before its exact owned child cleaned up")
                    self.assertTrue(child_alive, "fixture cleanup hold was not exercised")
                    self.assertTrue(slot_free, "preflight must not occupy build admission")
                    self.assertEqual(process.returncode, 128 + number, stderr)
                    self.assertEqual(late_builds, [], "cancellation must not reach the compiler")
                    self.assert_preflight_unslotted(calls)
                    self.assertTrue(select.select([pidfd], [], [], 0)[0])
                    self.assertFalse(caller_reaped, "gate left an owned preflight child for its caller to reap")
                    self.assertTrue(self.slot_is_free())

    def test_policy_marker_stdin_descriptor_and_compile_exit_are_preserved(self):
        for config, expected, phases in (
            ({"compiler_exit": 17}, 17, ["metadata", "guard", "seed", "compiler", "compiler-stdin"]),
            ({"guard_exit": 2}, 2, ["metadata", "guard"]),
            ({"seed_exit": 19}, 0, ["metadata", "guard", "seed", "compiler", "compiler-stdin"]),
        ):
            with self.subTest(config=config):
                self.clear()
                self.configure(**config)
                with (self.root / "caller-output").open("w") as output:
                    process = self.start(environment=self.env | {"FIXTURE_OUTPUT_FD": str(output.fileno())},
                                         pass_fds=(output.fileno(),))
                    stdout, stderr = process.communicate("synthetic caller stdin\n", timeout=HANG_GUARD)
                self.assertEqual(process.returncode, expected, stderr)
                calls = self.calls()
                self.assertEqual([event["phase"] for event in calls], phases)
                self.assert_preflight_unslotted(calls)
                if "compiler" in phases:
                    compiler = next(event for event in calls if event["phase"] == "compiler")
                    self.assertTrue(compiler["held"], compiler)
                    self.assertTrue(compiler["marker"], compiler)
                    self.assertEqual((self.root / "caller-output").read_text(), "synthetic caller stdin\n")
                    self.assertEqual(stdout, "private compiler output\n")
                else:
                    self.assertEqual(stdout, "")
                    self.assertEqual((self.root / "caller-output").read_text(), "")
                self.assertTrue(self.slot_is_free())

    def test_a_cargo_reporting_no_build_directory_is_guarded_at_its_target(self):
        target = str(self.root / "owned-target")
        self.configure(guard_exit=2, target_directory=target)
        process = self.start()
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 2, stderr)
        calls = self.calls()
        self.assertEqual([event["phase"] for event in calls], ["metadata", "guard"])
        self.assertEqual(calls[1]["arguments"], [target])

    def test_a_named_cargo_is_the_one_asked_for_the_directory(self):
        (self.root / "named").mkdir()
        named = self.root / "named/cargo"
        (self.root / "bin/cargo").rename(named)
        target = str(self.root / "owned-target")
        self.configure(guard_exit=2, target_directory=target)
        process = self.start(environment=self.env | {"CAGENTS_BUILD_CARGO": str(named)})
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 2, stderr)
        calls = self.calls()
        self.assertEqual([event["phase"] for event in calls], ["metadata", "guard"])
        self.assertEqual(calls[1]["arguments"], [target])

    def test_failed_or_empty_metadata_still_skips_helpers_and_builds(self):
        for config in ({"metadata_exit": 37}, {"metadata_exit_after_output": 37}, {"build_directory": ""}):
            with self.subTest(config=config):
                self.clear()
                self.configure(guard_exit=2, **config)
                process = self.start()
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual([event["phase"] for event in self.calls()],
                                 ["metadata", "compiler", "compiler-stdin"])
                self.assert_preflight_unslotted(self.calls())
                self.assertTrue(self.slot_is_free())

    def test_pinned_cargo_runs_the_seed_metadata_and_build(self):
        # A system Cargo can be first on PATH even though rustup, and its
        # workspace-aware Cargo proxy, are installed later. The gate must
        # select the proxy for both the seed preflight and build-host; mixing
        # them leaves one toolchain unable to read the other's artifacts.
        original = self.root / "bin/cargo"
        fallback = self.root / "fallback"
        rustup_bin = self.root / "rustup-bin"
        fallback.mkdir()
        rustup_bin.mkdir()
        selected = self.root / "selected"
        for directory, name in ((fallback, "system"), (rustup_bin, "pinned")):
            wrapper = directory / "cargo"
            wrapper.write_text(textwrap.dedent(f"""\
                #!/bin/sh
                printf '%s\\n' {name} >>{shlex.quote(str(selected))}
                exec {shlex.quote(str(original))} "$@"
            """))
            wrapper.chmod(0o700)
        rustup = rustup_bin / "rustup"
        rustup.write_text("#!/bin/sh\nexit 99\n")
        rustup.chmod(0o700)
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "pinned"\n')
        self.clear()
        process = self.start(environment=self.env | {
            "PATH": os.pathsep.join([str(fallback), str(rustup_bin), "/usr/bin", "/bin"]),
        })
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual(selected.read_text().splitlines(), ["pinned", "pinned"])


class TestsUnderAMemoryLimit(unittest.TestCase):
    """A stand-in limit beside build-host, recording what it is asked to run."""

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-gate-memory-")
        self.addCleanup(self.scope.close)
        self.root = Path(self.scope.name)
        self.gate = copy_gate(self.root)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CAGENTS_BUILD_") and key != "CARGO_BUILD_JOBS"
        } | {"CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"), "CAGENTS_BUILD_SLOTS": "2",
             "CAGENTS_BUILD_REMOTE": "local", "CAGENTS_BUILD_INSTALLED_GATE": "",
             "CAGENTS_BUILD_SCOPE": "0", "CAGENTS_BUILD_SEED": "0", "TMPDIR": str(self.root)}
        self.ran = self.root / "ran"
        # build-host runs the limit with its own Python, as it does the real one.
        limit = self.root / "memory-capped"
        limit.write_text(textwrap.dedent(f"""\
            #!/usr/bin/python3
            import os, sys
            with open({str(self.ran)!r}, "a") as ran:
                ran.write("limited " + " ".join(sys.argv[1:]) + "\\n")
            os.execvp(sys.argv[4], sys.argv[4:])
        """))
        limit.chmod(0o755)
        cargo = self.root / "cargo"
        cargo.write_text(f'#!/bin/sh\necho "cargo $*" >>"{self.ran}"\n')
        cargo.chmod(0o755)
        self.env["CAGENTS_BUILD_CARGO"] = str(cargo)

    def gate_run(self, *arguments, wrapper=()):
        result = self.scope.run([*wrapper, str(self.gate), *arguments], env=self.env, cwd=self.root,
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 0, result.stderr)
        return [line.replace(str(self.root) + "/", "") for line in self.ran.read_text().splitlines()]

    def test_tests_under_it_keep_the_runner_that_unstamps_them(self):
        offload = self.root / "cargo-offload"
        offload.write_text('#!/bin/sh\n[ "$1" = --stamp ] || exit 75\necho 1.2.3+stamp\n')
        offload.chmod(0o755)
        self.env["CARGO_HOME"] = str(self.root / "cargo-home")
        for route in (("test", "--lib"), ("exec", str(self.gate), "test", "--lib")):
            with self.subTest(route[0]):
                self.ran.unlink(missing_ok=True)
                limited = [line for line in self.gate_run(*route) if line.startswith("limited ")]
                self.assertEqual(len(limited), 1)
                self.assertIn("--slice app-cagents-test.slice --stop-leftovers cargo test --config target.'cfg(all())'.runner=",
                              limited[0])

    def test_tests_and_benchmarks_run_under_it_after_their_build_without_it(self):
        for subcommand in ("test", "bench"):
            with self.subTest(subcommand):
                self.ran.unlink(missing_ok=True)
                self.assertEqual(self.gate_run(subcommand, "--lib", "filter"), [
                    f"cargo {subcommand} --no-run --lib filter",
                    f"limited --slice app-cagents-test.slice --stop-leftovers cargo {subcommand} --lib filter",
                    f"cargo {subcommand} --lib filter",
                ])

    def test_doctests_build_and_run_under_it_in_one(self):
        self.assertEqual(self.gate_run("test", "--doc"), [
            "limited --slice app-cagents-test.slice --stop-leftovers cargo test --doc",
            "cargo test --doc",
        ])

    def test_a_program_or_a_build_does_not(self):
        for arguments in (("run", "--bin", "app"), ("build",), ("test", "--no-run")):
            with self.subTest(arguments):
                self.ran.unlink(missing_ok=True)
                self.assertNotIn("limited", "\n".join(self.gate_run(*arguments)))

    def test_tests_inside_a_held_slot_run_under_it(self):
        for subcommand in ("test", "bench"):
            with self.subTest(subcommand):
                self.ran.unlink(missing_ok=True)
                lines = self.gate_run("exec", str(self.gate), subcommand, "--lib")
                self.assertIn(f"limited --slice app-cagents-test.slice --stop-leftovers cargo {subcommand} --lib", lines)
                self.assertIn(f"cargo {subcommand} --lib", lines)

    def test_an_updated_gate_brings_the_limit_and_build_host_with_it(self):
        synced = (ROOT / "scripts/cargo-gate-sync.py").read_text()
        for name in ("scripts/memory-capped", "scripts/build-host"):
            self.assertIn(f'"{name}"', synced)
        installer = (ROOT / "scripts/install-cargo-gate").read_text()
        for name in ("memory-capped", "build-host"):
            self.assertRegex(installer, rf"(?m)^(files=')?{name}:{name}$")


class SharesByKind(unittest.TestCase):
    """What the gate asks build-host for, read from a stand-in build-host."""

    def setUp(self):
        self.scope = FixtureDirectory(prefix="cagents-gate-shares-")
        self.addCleanup(self.scope.close)
        self.root = Path(self.scope.name)
        self.gate = copy_gate(self.root)
        self.asked = self.root / "asked"
        (self.root / "build-host").write_text(textwrap.dedent(f"""\
            #!/usr/bin/python3
            import json, sys
            with open({str(self.asked)!r}, "a") as asked:
                asked.write(json.dumps(sys.argv[1:]) + "\\n")
        """))
        (self.root / "build-host").chmod(0o755)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_BUILD_")} | {
            "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"), "CAGENTS_BUILD_REMOTE": "local",
            "CAGENTS_BUILD_INSTALLED_GATE": "", "CAGENTS_BUILD_SCOPE": "0",
            "CAGENTS_BUILD_CARGO": "cargo", "TMPDIR": str(self.root)}

    def ask(self, *arguments, env=None, wrapper=()):
        self.asked.unlink(missing_ok=True)
        result = self.scope.run([*wrapper, str(self.gate), *arguments], env=env or self.env, cwd=self.root,
                                capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 0, result.stderr)
        [asked] = [json.loads(line) for line in self.asked.read_text().splitlines()]
        return asked

    def test_a_build_takes_shares_by_its_lane_and_kind(self):
        for arguments, shares in (
                (("build", "--locked"), "1"),
                (("test", "--no-default-features", "--bin", "cagents"), "1"),
                (("check", "--features", "ui"), "1"),
                (("clippy", "--all-features"), "1"),
                (("doc", "--features=host"), "1"),
                (("build", "--features", "host"), "2"),
                (("build", "-Fui", "--bin", "cagents-ui"), "2"),
                (("test", "--features", "integrated-browser"), "2"),
                (("build", "--bin=cagentsd"), "2"),
                (("build", "--all-features"), "2"),
                (("test", "-p", "cagents-ui"), "2"),
                (("test", "-pcagents-ui"), "2"),
                (("test", "--package=cagents-ui"), "2"),
                (("test", "--package", "cagents"), "1"),
                (("build", "--package", "cagents", "--features", "cagents/host"), "2"),
                (("test", "--package", "cagents", "--package", "cagents-ui", "--features", "cagents/ui"), "2"),
                (("build", "--package", "cagents-ui", "--features", "integrated-browser"), "2"),
                (("run", "--", "--features", "ui"), "1")):
            with self.subTest(arguments):
                self.assertEqual(self.ask(*arguments),
                                 ["cargo", "--shares", shares, "--", "cargo", *arguments])

    def test_a_script_of_builds_takes_two_until_it_exits(self):
        asked = self.ask("exec", "true")
        # build-host removes the gate's Cargo shim when the script ends.
        [shims] = self.root.glob("cagents-cargo-shim.*")
        self.assertEqual(asked, ["build", "--shares", "2", "--remove", str(shims), "--", "true"])
        self.assertEqual((shims / "cargo").read_text().splitlines()[-1],
                         f"exec '{self.gate}' cargo \"$@\"")

    def test_a_setting_overrides_the_kind_and_must_be_a_positive_integer(self):
        self.assertEqual(self.ask("build", env=self.env | {"CAGENTS_BUILD_SHARES": "3"})[:3],
                         ["cargo", "--shares", "3"])
        self.asked.unlink()
        for value in ("0", "03", "x", "-1"):
            with self.subTest(value=value):
                result = self.scope.run([str(self.gate), "build"],
                                        env=self.env | {"CAGENTS_BUILD_SHARES": value}, cwd=self.root,
                                        capture_output=True, text=True, timeout=HANG_GUARD)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("CAGENTS_BUILD_SHARES must be a positive integer", result.stderr)
                self.assertFalse(self.asked.exists())

    def test_an_admission_budget_becomes_a_deadline(self):
        before = time.monotonic_ns()
        asked = self.ask("check", env=self.env | {"CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "5"})
        after = time.monotonic_ns()
        self.assertEqual(asked[:3], ["cargo", "--shares", "1"])
        self.assertEqual(asked[3], "--deadline")
        self.assertLessEqual(before + 5 * 10**9, int(asked[4]))
        self.assertLessEqual(int(asked[4]), after + 5 * 10**9)
        self.assertEqual(asked[5:], ["--", "cargo", "check"])

    def test_a_command_under_a_held_share_takes_none(self):
        # Under `flock --close` on the first share, as a caller's own script.
        asked = self.ask("build", wrapper=("flock", "--close", str(self.root / "slot.lock")))
        self.assertEqual(asked, ["cargo", "--shares", "0", "--", "cargo", "build"])


class AdmissionTimeout(unittest.TestCase):
    """Reuse private preflight tools; all flock operations remain real."""

    setUpClass = classmethod(PreflightCancellation.setUpClass.__func__)
    tearDownClass = classmethod(PreflightCancellation.tearDownClass.__func__)
    setting = "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS"
    # A budget that a passing run never spends, however slow the host.
    unreached = str(10 * HANG_GUARD)

    def setUp(self):
        self.fixture = PreflightCancellation("test_failed_or_empty_metadata_still_skips_helpers_and_builds")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        # Adapt the existing inert tool with short delays and fast cooperative
        # cleanup. Ordinary preflight contracts keep their original fixture.
        tool = PREFLIGHT_TOOL.replace(
            '    (root / (phase + ".signalled")).write_text(str(number))\n',
            '    (root / (phase + ".signalled")).write_text(str(number))\n'
            '    if config.get("fast_cancel"):\n        raise SystemExit(128 + number)\n',
        ).replace(
            'record(event)\n',
            'event["order_id"] = os.environ.get("FIXTURE_ORDER_ID", "")\n'
            'event["admission_deadline"] = os.environ.get("FIXTURE_ADMISSION_DEADLINE")\n'
            'record(event)\n'
            'delay = config.get("delays", {}).get(phase, 0)\n'
            'if delay:\n'
            '    receipt = root / (phase + ".delaying")\n'
            '    receipt.with_suffix(".tmp").write_text(json.dumps(event))\n'
            '    receipt.with_suffix(".tmp").replace(receipt)\n'
            # On a pinned admission clock, only a hook moves it, after its receipt.
            'advance = config.get("clock_ns", {}).get(phase)\n'
            'if advance is not None:\n'
            '    clock = root / "admission-clock"\n'
            '    clock.with_suffix(".tmp").write_text(str(advance))\n'
            '    clock.with_suffix(".tmp").replace(clock)\n'
            'if delay:\n'
            '    time.sleep(delay)\n',
        )
        self.assertIn('if config.get("fast_cancel")', tool)
        self.assertIn('phase + ".delaying"', tool)
        for name in ("bin/cargo", "build-disk-guard", "seed-build-dir"):
            (self.fixture.root / name).write_text(tool)
        self.fixture.env[self.setting] = "1"

    def pin_admission_clock(self):
        """Reads admission time, in the gate's helper and in build-host which
        it hands the deadline to, from a file that only the hooks' `clock_ns`
        advance, so that no host scheduling spends the budget. Cleanup's grace
        keeps the real monotonic clock, and so does a kernel lock wait that
        build-host bounds with a timer, so a contended wait still runs in real
        time."""
        self.set_admission_clock(0)
        clock = self.fixture.root / "admission-clock"
        pinned = ("import time\nfrom pathlib import Path\n"
                  f"time.monotonic_ns = lambda: int(Path({str(clock)!r}).read_text())\n")
        host = self.fixture.root / "build-host"
        source = host.read_text()
        self.assertEqual(source.count("import time\n"), 1)
        host.write_text(source.replace("import time\n", pinned))
        helper = self.fixture.root / "cargo-gate-command.py"
        source = helper.read_text()
        self.assertEqual(source.count("import time\n"), 1)
        source = source.replace("import time\n", pinned)
        parsed = "        admission_deadline = positive_integer(arguments[1])\n"
        self.assertEqual(source.count(parsed), 1)
        source = source.replace(parsed, parsed +
            '        os.environ["FIXTURE_ADMISSION_DEADLINE"] = str(admission_deadline)\n')
        helper.write_text(source)

    def set_admission_clock(self, nanoseconds):
        clock = self.fixture.root / "admission-clock"
        temporary = clock.with_suffix(".tmp")
        temporary.write_text(str(nanoseconds))
        temporary.replace(clock)

    def reset(self, **config):
        self.fixture.clear()
        for pattern in ("*.waiting", "*.delaying"):
            for path in self.fixture.root.glob(pattern):
                path.unlink()
        self.fixture.configure(fast_cancel=True, **config)

    def pin(self, receipt, *, process=None):
        if process is None:
            self.fixture.wait_for(receipt.exists, receipt.name)
            pid = json.loads(receipt.read_text())["pid"]
            pidfd = os.pidfd_open(pid)
            self.fixture.descendants.append((pid, pidfd))
            return pid, pidfd
        # The gate has become build-host, which waits on the exact private
        # lock: blocked in the kernel on the queue, or holding the queue and
        # waiting for a share. It has no child of its own to reap.
        queue = os.path.realpath(self.fixture.root / "slot.lock.queue")

        def waiting():
            try:
                arguments = Path(f"/proc/{process.pid}/cmdline").read_bytes().split(b"\0")
                if not any(argument.endswith(b"/build-host") for argument in arguments):
                    return False
                opened = False
                for descriptor in Path(f"/proc/{process.pid}/fd").iterdir():
                    with contextlib.suppress(FileNotFoundError):
                        opened |= os.readlink(descriptor) == queue
                if not opened:
                    return False
                if receipt.name.startswith("queue"):
                    return "lock" in Path(f"/proc/{process.pid}/wchan").read_text()
                with open(queue, "a") as probe:
                    try:
                        fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        return True
                    return False
            except (FileNotFoundError, ProcessLookupError):
                return False

        self.fixture.wait_for(waiting, "build-host waiting for " + receipt.name)
        return None

    def retire(self, owned):
        if owned is None:
            return False
        pid, pidfd = owned
        self.fixture.wait_for(lambda: bool(select.select([pidfd], [], [], 0)[0]), "owned waiter exit")
        try:
            caller_reaped = os.waitpid(pid, os.WNOHANG)[0] == pid
        except ChildProcessError:
            caller_reaped = False
        return caller_reaped

    def finish_blocked(self, process, holder):
        # Baseline gates have no timeout; release only our outside holder to
        # let those owned waits finish, retaining the causal failure result.
        # The holder never lets go by itself, so a gate that would wait on it
        # stays blocked past the hang guard.
        completed_while_held = True
        try:
            stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        except subprocess.TimeoutExpired:
            completed_while_held = False
            fcntl.flock(holder, fcntl.LOCK_UN)
            stdout, stderr = process.communicate(timeout=HANG_GUARD)
        return completed_while_held, stdout, stderr

    def assert_timeout(self, process, stderr):
        self.assertEqual(process.returncode, 124, stderr)
        self.assertIn("admission", stderr.lower())
        self.assertIn("tim", stderr.lower())
        self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])
        self.assertTrue(self.fixture.slot_is_free())

    def test_expiry_in_each_preflight_hook_reaps_without_compiling(self):
        # The one-second budget runs out in this hook, on the pinned clock,
        # however long the hooks before it took; it then waits to be reaped.
        self.pin_admission_clock()
        for phase in ("metadata", "guard", "seed"):
            with self.subTest(phase=phase):
                self.set_admission_clock(0)
                self.reset(clock_ns={phase:1_000_000_000}, delays={phase:2 * HANG_GUARD})
                process = self.fixture.start()
                owned = self.pin(self.fixture.root / (phase + ".delaying"))
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                caller_reaped = self.retire(owned)
                self.assert_timeout(process, stderr)
                self.assertFalse(caller_reaped)
                self.fixture.assert_preflight_unslotted(self.fixture.calls())

    def test_one_budget_covers_cumulative_hooks_and_seed_is_not_fail_soft_on_expiry(self):
        # Advance only admission time, not cleanup's real monotonic clock. The
        # contract is one deadline across hooks, regardless of host scheduling.
        self.pin_admission_clock()
        self.reset(clock_ns={"metadata":700_000_000, "guard":1_400_000_000,
                             "seed":2_600_000_000}, delays={"seed":2 * HANG_GUARD})
        process = self.fixture.start(environment=self.fixture.env | {self.setting:"2"})
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assert_timeout(process, stderr)
        calls = self.fixture.calls()
        self.assertEqual([event["phase"] for event in calls], ["metadata", "guard", "seed"])
        self.assertEqual({event["admission_deadline"] for event in calls}, {"2000000000"})

    def test_queue_and_share_expiry_withdraws_the_waiter_and_preserves_the_holder(self):
        # On the pinned clock, preflight spends none of the budget.
        self.pin_admission_clock()
        for kind in ("queue", "base"):
            with self.subTest(kind=kind):
                self.set_admission_clock(0)
                self.reset()
                root = self.fixture.root
                path = root / ("slot.lock.queue" if kind == "queue" else "slot.lock")
                # A kernel wait on the queue is bounded by a real timer, which
                # must outlast this test seeing it wait. A wait for a share
                # reads the pinned clock, which the test then moves past it.
                budget = "3" if kind == "queue" else self.unreached
                with path.open("a") as holder:
                    fcntl.flock(holder, fcntl.LOCK_EX)
                    process = self.fixture.start(environment=self.fixture.env | {self.setting:budget})
                    self.pin(root / ("queue.waiting" if kind == "queue" else "base.waiting"), process=process)
                    if kind == "base":
                        self.set_admission_clock(int(budget) * 10**9)
                    completed, _stdout, stderr = self.finish_blocked(process, holder)
                    self.assertTrue(completed, "gate required its outside holder to release")
                    self.assertEqual(process.returncode, 124, stderr)
                    self.assertEqual(stderr.count("build admission timed out"), 1, stderr)
                    self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])
                    # The outside holder keeps what it held.
                    with path.open("a") as probe:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertTrue(self.fixture.slot_is_free())
                with (root / "slot.lock.queue").open("a") as queue:
                    fcntl.flock(queue, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_each_direct_signal_cancels_queued_wait_without_timeout_or_orphan(self):
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            with self.subTest(signal=number):
                self.reset()
                with (self.fixture.root / "slot.lock.queue").open("a") as holder:
                    fcntl.flock(holder, fcntl.LOCK_EX)
                    process = self.fixture.start(environment=self.fixture.env | {self.setting:self.unreached})
                    self.pin(self.fixture.root / "queue.waiting", process=process)
                    process.send_signal(number)
                    completed, _stdout, stderr = self.finish_blocked(process, holder)
                    self.assertTrue(completed, "cancellation left a waiter behind its outside holder")
                    # build-host ends as the signal would end it.
                    self.assertEqual(process.returncode, -number, stderr)
                    self.assertNotIn("timed out", stderr.lower())
                    self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])

    def test_private_kernel_queue_preserves_arrival_order(self):
        self.reset()
        root = self.fixture.root
        processes = []
        with (root / "slot.lock.queue").open("a") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            for identifier in ("first", "second"):
                process = self.fixture.start(environment=self.fixture.env | {
                    self.setting:self.unreached, "FIXTURE_ORDER_ID":identifier})
                processes.append(process)
                self.pin(root / ("queue-" + identifier + ".waiting"), process=process)
            fcntl.flock(holder, fcntl.LOCK_UN)
            for process in processes:
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual([event["order_id"] for event in self.fixture.calls() if event["phase"] == "compiler"],
                         ["first", "second"])

    def test_invalid_timeout_is_refused_before_hooks(self):
        for value in ("", "0", "00", "-1", "+1", "1.0", "1s", " 1", "1 ", "١", "１", "1\n"):
            with self.subTest(value=value):
                self.reset()
                process = self.fixture.start(environment=self.fixture.env | {self.setting:value})
                _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
                self.assertEqual(process.returncode, 2, stderr)
                self.assertIn(self.setting, stderr)
                self.assertEqual(self.fixture.calls(), [])

    def test_ordinary_guard_exit124_is_not_relabelled_as_timeout(self):
        self.reset(guard_exit=124)
        process = self.fixture.start(environment=self.fixture.env | {self.setting:self.unreached})
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 124, stderr)
        self.assertNotIn("timed out", stderr.lower())
        self.assertEqual([event["phase"] for event in self.fixture.calls()], ["metadata", "guard"])

    def test_leading_zero_budget_does_not_limit_an_admitted_build(self):
        # On the pinned clock, preflight spends all but a tenth of the three
        # seconds; the compiler then moves past their end and runs on for a
        # second more.
        self.pin_admission_clock()
        self.reset(clock_ns={"seed":2_900_000_000, "compiler":4_000_000_000},
                   delays={"compiler":1}, compiler_exit=17)
        process = self.fixture.start(environment=self.fixture.env | {self.setting:"0003"})
        _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 17, stderr)
        self.assertNotIn("timed out", stderr.lower())
        self.assertEqual({event["admission_deadline"] for event in self.fixture.calls()
                          if event["phase"] in ("metadata", "guard", "seed")}, {"3000000000"})
        compiler = next(event for event in self.fixture.calls() if event["phase"] == "compiler")
        self.assertTrue(compiler["held"])
        self.assertTrue(compiler["marker"])
        self.assertTrue(self.fixture.slot_is_free())

    def test_unset_budget_waits_until_private_holder_releases(self):
        self.reset()
        environment = self.fixture.env.copy()
        environment.pop(self.setting)
        with (self.fixture.root / "slot.lock.queue").open("a") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            process = self.fixture.start(environment=environment)
            self.pin(self.fixture.root / "queue.waiting", process=process)
            time.sleep(0.35)
            self.assertIsNone(process.poll())
            self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])
            fcntl.flock(holder, fcntl.LOCK_UN)
            _stdout, stderr = process.communicate("", timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertNotIn("timed out", stderr.lower())

class FixtureCustody(unittest.TestCase):
    def test_a_timed_out_command_and_its_new_session_child_end_before_the_directory_goes(self):
        scope = FixtureDirectory()
        self.addCleanup(scope.close)
        root = Path(scope.name)
        with self.assertRaises(subprocess.TimeoutExpired):
            # The command execs so nothing but the sweep can reap its children.
            scope.run(["sh", "-c", f"sleep 60 & echo $! >{root}/ordinary; "
                       f"setsid sleep 60 & echo $! >{root}/detached; exec sleep 60"], timeout=1)
        pidfds = [os.pidfd_open(int((root / name).read_text())) for name in ("ordinary", "detached")]
        self.addCleanup(lambda: [os.close(pidfd) for pidfd in pidfds])
        # The command's group is gone; the child in its own session is not.
        self.assertFalse(select.select([pidfds[1]], [], [], 0)[0])
        settled = fixture_custody.settle
        survivors = None

        def settle():
            nonlocal survivors
            self.assertTrue(root.is_dir())
            settled()
            survivors = [pidfd for pidfd in pidfds if not select.select([pidfd], [], [], 0)[0]]

        with unittest.mock.patch.object(fixture_custody, "settle", settle):
            scope.close()
        self.assertEqual(survivors, [])
        self.assertFalse(root.exists())


if __name__ == "__main__":
    unittest.main()
