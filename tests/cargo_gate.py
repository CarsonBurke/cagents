#!/usr/bin/env python3
"""Exercise held-command lifetime with private locks and owned processes only."""

import contextlib
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CargoGateApplicability(unittest.TestCase):
    """Run CI's exact inline Cargo block against private source-head fixtures."""

    paths = ("scripts/cargo-gate", "scripts/cargo-gate-command.py", "tests/cargo_gate.py")

    @classmethod
    def setUpClass(cls):
        lines = (ROOT / ".github/workflows/script-contracts.yml").read_text().splitlines(keepends=True)
        step = "      - name: Check Cargo gate source and contracts\n"
        starts = [index for index, line in enumerate(lines) if line == step]
        if len(starts) != 1 or lines[starts[0] + 1] != "        run: |\n":
            raise AssertionError("expected exactly one inline Cargo applicability step")
        body = []
        for line in lines[starts[0] + 2:]:
            if line.startswith("          "):
                body.append(line[10:])
            elif not line.strip():
                body.append("\n")
            else:
                break
        cls.run_block = "".join(body)
        if not cls.run_block.startswith("python3 -I -B - <<'PY'\n"):
            raise AssertionError("Cargo applicability must run inline isolated Python")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cargo-contract-applicability-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def case(self, name):
        root = self.root / name
        root.mkdir(mode=0o700)
        return root

    def sources(self, root, *, legacy=False, exit_code=0):
        gate, source, fixture = (root / name for name in self.paths)
        gate.parent.mkdir(parents=True)
        gate.write_text("#!/bin/sh\n" + ("" if legacy else "# cargo-gate-command.py\n") + "exit 0\n")
        if legacy:
            return
        source.write_text("fixture supervisor placeholder; never execute this file\n")
        fixture.parent.mkdir()
        fixture.write_text(
            "import json\nfrom pathlib import Path\nimport sys\n"
            "Path('calls.json').write_text(json.dumps({'arguments': sys.argv[1:], "
            "'isolated': sys.flags.isolated, 'bytecode': sys.flags.dont_write_bytecode}))\n"
            f"raise SystemExit({exit_code})\n"
        )

    def invoke(self, root):
        # No Cargo, Git, production script, environment override or global lock
        # is involved. Only the private fixture test file can be executed.
        return subprocess.run(
            ["/bin/sh", "-e", "-c", self.run_block], cwd=root,
            capture_output=True, text=True, timeout=15,
        )

    def assert_hash(self, result, root, name):
        digest = hashlib.sha256((root / name).read_bytes()).hexdigest()
        self.assertIn(f"{digest}  {name}\n", result.stdout)

    def test_old_head_without_gate_files_is_explicitly_not_applicable(self):
        root = self.case("old")
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "Not applicable: Cargo gate (source absent at this head)",
            "Not applicable: held-command cancellation (source and fixture both absent at this head)",
        ])
        self.assertEqual(list(root.iterdir()), [])

    def test_legacy_gate_is_hashed_and_missing_supervisor_pair_is_not_applicable(self):
        root = self.case("legacy")
        self.sources(root, legacy=True)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_hash(result, root, self.paths[0])
        self.assertIn("Not applicable: held-command cancellation", result.stdout)
        self.assertFalse((root / "calls.json").exists())

    def test_complete_source_hashes_all_members_and_runs_the_isolated_fixture(self):
        root = self.case("current")
        self.sources(root)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in self.paths:
            self.assert_hash(result, root, name)
        self.assertIn("Checking: held-command cancellation\n", result.stdout)
        self.assertEqual(json.loads((root / "calls.json").read_text()), {
            "arguments": ["-v"], "isolated": 1, "bytecode": 1,
        })

    def test_partial_or_inconsistent_current_source_is_refused_before_execution(self):
        for index, missing in enumerate((
            (self.paths[0],), (self.paths[1],), (self.paths[2],), self.paths[1:],
        )):
            with self.subTest(missing=missing):
                root = self.case(f"partial-{index}")
                self.sources(root)
                for name in missing:
                    (root / name).unlink()
                result = self.invoke(root)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("Incomplete ", result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertFalse((root / "calls.json").exists())

    def test_fixture_failure_status_is_preserved(self):
        root = self.case("failure")
        self.sources(root, exit_code=19)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 19, result.stderr)
        self.assertTrue((root / "calls.json").exists())

    def test_runner_requiring_an_absent_gate_is_refused(self):
        root = self.case("current-runner-without-gate")
        runner = root / "scripts/run-development"
        runner.parent.mkdir()
        runner.write_text('#!/bin/sh\n"$root/scripts/cargo-gate" build\n')
        result = self.invoke(root)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("references missing scripts/cargo-gate", result.stderr)
        self.assertFalse((root / "calls.json").exists())

    def test_legacy_shell_syntax_is_still_required(self):
        root = self.case("bad-syntax")
        self.sources(root, legacy=True)
        (root / self.paths[0]).write_text("#!/bin/sh\nif\n")
        result = self.invoke(root)
        self.assertNotEqual(result.returncode, 0)
        self.assert_hash(result, root, self.paths[0])
        self.assertNotIn("Not applicable: held-command cancellation", result.stdout)

    def test_symlink_dangling_and_nonregular_members_fail_without_execution(self):
        for index, member in enumerate(self.paths):
            for kind in ("symlink", "dangling", "directory"):
                with self.subTest(member=member, kind=kind):
                    root = self.case(f"invalid-{index}-{kind}")
                    self.sources(root)
                    path = root / member
                    contents = path.read_bytes()
                    path.unlink()
                    if kind == "directory":
                        path.mkdir()
                    else:
                        target = root / "private-target"
                        if kind == "symlink":
                            target.write_bytes(contents)
                        path.symlink_to(target)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assertFalse((root / "calls.json").exists())

    def test_nonregular_parent_components_fail_closed(self):
        for parent in ("scripts", "tests"):
            for kind in ("file", "symlink"):
                with self.subTest(parent=parent, kind=kind):
                    root = self.case(f"parent-{parent}-{kind}")
                    path = root / parent
                    if kind == "file":
                        path.write_text("non-directory fixture parent\n")
                    else:
                        target = root / "private-directory"
                        target.mkdir()
                        path.symlink_to(target, target_is_directory=True)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assertFalse((root / "calls.json").exists())


class InstalledGateDeferral(unittest.TestCase):
    """A checkout whose gate has fallen behind the installed one runs that."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-deferral-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = self.root / "checkout" / "cargo-gate"
        self.gate.parent.mkdir()
        shutil.copy2(ROOT / "scripts/cargo-gate", self.gate)
        shutil.copy2(ROOT / "scripts/cargo-gate-command.py", self.gate.parent)
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
                              cwd=self.root, capture_output=True, text=True, timeout=30)

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


class InstalledGateSync(unittest.TestCase):
    """A gate change merged into main is installed by the next gate run."""

    files = ("cargo-gate", "cargo-gate-command.py", "cargo-gate-sync.py",
             "cargo-gate-rustc-guard", "cargo-offload",
             "seed-build-dir", "build-disk-guard", "install-cargo-gate")

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
            "CAGENTS_BUILD_REMOTE": "local",
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


class HeldCommand(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise unittest.SkipTest("Linux pidfds are required")
        cls.libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        if cls.libc.prctl(37, ctypes.byref(previous), 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot inspect child subreaper")
        cls.previous_subreaper = previous.value
        if cls.libc.prctl(36, 1, 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot own orphaned fixture descendants")

    @classmethod
    def tearDownClass(cls):
        cls.libc.prctl(36, cls.previous_subreaper, 0, 0, 0)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-held-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = self.root / "cargo-gate"
        shutil.copy2(ROOT / "scripts/cargo-gate", self.gate)
        helper = ROOT / "scripts/cargo-gate-command.py"
        if helper.exists():
            shutil.copy2(helper, self.root / helper.name)
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
            child.wait(timeout=5)
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

    def wait_for(self, predicate, label, timeout=5):
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

    def assert_reaped(self, pidfd):
        self.assertTrue(select.select([pidfd], [], [], 0)[0])
        pid = next(pid for pid, owned in self.descendants if owned == pidfd)
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return
        self.fail("supervisor exited without reaping its descendant")

    def launch_descendant(self, *, detached=False, background=False, resistant_primary=False):
        # The shell is the supported `exec COMMAND`; its child deliberately
        # takes time to finish after TERM so an early slot release is observable.
        worker = self.root / "worker.py"
        worker.write_text("""
import json, os, pathlib, signal, sys, time
root = pathlib.Path(sys.argv[1])
if sys.argv[2] == 'detached':
    os.setsid()
def stop(_signal, _frame):
    (root / 'terminated').touch()
    while not (root / 'release').exists():
        time.sleep(0.01)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
(root / 'ready.tmp').write_text(json.dumps({'pid': os.getpid()}))
(root / 'ready.tmp').rename(root / 'ready')
while True:
    time.sleep(1)
""")
        command = '"$1" "$2" "$3" "$4" & wait'
        if background:
            command = '"$1" "$2" "$3" "$4" & while [ ! -f "$3/ready" ]; do sleep 0.01; done'
        if resistant_primary:
            command = 'trap \'touch "$3/parent-terminated"\' TERM; "$1" "$2" "$3" "$4" & while :; do wait; done'
        child = subprocess.Popen(
            [str(self.gate), "exec", "/bin/sh", "-c", command, "fixture",
             sys.executable, str(worker), str(self.root),
             "detached" if detached else "ordinary"],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.children.append(child)
        self.wait_for(lambda: (self.root / "ready").exists(), "descendant ready")
        pid = json.loads((self.root / "ready").read_text())["pid"]
        pidfd = os.pidfd_open(pid)
        self.descendants.append((pid, pidfd))
        if not background:
            self.assertFalse(self.slot_is_free(), "live command owns its slot")
        return child, pidfd

    def test_cancellation_keeps_slot_until_descendant_exits(self):
        child, pidfd = self.launch_descendant()
        child.send_signal(signal.SIGTERM)
        self.wait_for(
            lambda: (self.root / "terminated").exists() or child.poll() is not None,
            "forwarded cancellation",
        )
        self.assertIsNone(child.poll(), "gate exited with an owned descendant alive")
        self.assertFalse(self.slot_is_free(), "cleanup must retain the slot")
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        (self.root / "release").touch()
        self.assertEqual(child.wait(timeout=5), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def test_cancellation_reaps_a_descendant_that_left_the_command_group(self):
        child, pidfd = self.launch_descendant(detached=True)
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=6), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def test_success_leaves_background_cache_services_outside_the_slot(self):
        child, pidfd = self.launch_descendant(background=True)
        self.assertEqual(child.wait(timeout=5), 0)
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        self.assertTrue(self.slot_is_free())

    def test_cancellation_drains_children_after_a_resistant_parent_is_killed(self):
        child, pidfd = self.launch_descendant(resistant_primary=True)
        child.send_signal(signal.SIGTERM)
        self.wait_for((self.root / "parent-terminated").exists, "parent cancellation handler")
        self.assertIsNone(child.poll())
        self.assertFalse(self.slot_is_free())
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        self.assertEqual(child.wait(timeout=6), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def run_gate(self, *arguments, **options):
        return subprocess.run(
            [str(self.gate), *arguments], env=self.env,
            capture_output=True, text=True, timeout=5, **options,
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
            deadline = time.monotonic() + 5
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
                self.assertEqual(child.wait(timeout=5), 128 + number)
                expected = "build --locked" if command == "run" else f"{command} --no-run --locked"
                self.assertEqual(calls.read_text().splitlines(), [expected])
                self.assertTrue(self.slot_is_free())
                ready.unlink()
                calls.unlink()

    def leftover_daemon_fixture(self):
        # A Cargo that leaves a daemon behind, in the build phase or the test run,
        # as a fixture cagentsd did when its `cargo test` was killed.
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!/bin/sh
root={str(self.root)!r}
case " $* " in
    *" --no-run "*) [ -e "$root/in-build" ] || exit 0 ;;
esac
setsid sleep 60 &
printf '%s %s' "$!" "$$" > "$root/ready.new"
mv "$root/ready.new" "$root/ready"
exec sleep 60
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
        child.wait(timeout=5)
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
            env=self.env, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def under_flock(self, slot, *inner, env=None):
        """A gated `exec true` under `flock --close slot`, through `inner`, without waiting long."""
        return subprocess.run(
            ["flock", "--close", str(slot), *inner, str(self.gate), "exec", "true"],
            env=env or self.env, capture_output=True, text=True, timeout=10,
        )

    def hold_slot_elsewhere(self, slot=None):
        holder = subprocess.Popen(["flock", "--close", str(slot or self.lock), "sleep", "60"],
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
            env=self.env, capture_output=True, text=True, timeout=10,
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
            env=self.env | {"CAGENTS_BUILD_SLOTS": "12"}, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.hold_slot_elsewhere()
        self.assert_gate_queues("flock", "--close", str(second))

    def test_flock_on_the_queue_or_cost_file_is_not_a_slot(self):
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
        """Pin this private gate's waiting tree until its lock waiter blocks."""
        pinned = {pid for pid, _pidfd in self.descendants}

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
                        arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                        found |= b"--wait-lock" in arguments and "lock" in Path(f"/proc/{pid}/wchan").read_text()
                    except (FileNotFoundError, ProcessLookupError):
                        pass
                    finally:
                        if pidfd is not None:
                            os.close(pidfd)
            return found

        self.wait_for(requesting, "the gate's owned lock request")

    def assert_killed_waiter_releases_callers(self, *, behind_head):
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
                        [str(self.gate), "exec", "true"], env=self.env,
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
        deadline = time.monotonic() + 1
        gate.wait(timeout=1)
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
                         "killed waiter retained caller resources beyond one second")

    def test_killed_queue_head_releases_extra_descriptors_without_releasing_holder(self):
        self.assert_killed_waiter_releases_callers(behind_head=False)

    def test_killed_waiter_behind_live_head_releases_all_caller_descriptors(self):
        self.assert_killed_waiter_releases_callers(behind_head=True)

    def test_a_killed_gate_leaves_nobody_holding_the_queue(self):
        self.hold_slot_elsewhere()
        gate = subprocess.Popen(
            [str(self.gate), "exec", "true"], env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        self.children.append(gate)
        self.addCleanup(self.end_group, gate.pid)
        queue = Path(f"{self.lock}.queue")
        self.wait_for(lambda: self.is_locked(queue), "the gate at the head of the queue")
        self.wait_for_gate_request(gate)
        gate.kill()
        gate.wait(timeout=5)
        self.wait_for(lambda: not self.is_locked(queue), "the queue to free")
        # Read to EOF without blocking: anything the gate wrote first also reads ready.
        for stream in (gate.stdout, gate.stderr):
            deadline = time.monotonic() + 5
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

def interrupted(number, _frame):
    (root / (phase + ".signalled")).write_text(str(number))
    deadline = time.monotonic() + 4
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
    deadline = time.monotonic() + 6
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
        cls.libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        if cls.libc.prctl(37, ctypes.byref(previous), 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot inspect child subreaper")
        cls.previous_subreaper = previous.value
        if cls.libc.prctl(36, 1, 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot own orphaned fixture descendants")

    @classmethod
    def tearDownClass(cls):
        cls.libc.prctl(36, cls.previous_subreaper, 0, 0, 0)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="gate-preflight-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in ("cargo-gate", "cargo-gate-command.py"):
            shutil.copy2(ROOT / "scripts" / name, self.root / name)
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
            process.communicate(timeout=5)
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

    def wait_for(self, predicate, label, timeout=5):
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
                    deadline = time.monotonic() + 1
                    receipt = self.root / (phase + ".signalled")
                    while not receipt.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    forwarded = receipt.exists()
                    running = process.poll() is None
                    child_alive = not select.select([pidfd], [], [], 0)[0]
                    slot_free = self.slot_is_free()
                    (self.root / "release").touch()
                    _stdout, stderr = process.communicate("", timeout=5)
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
                    stdout, stderr = process.communicate("synthetic caller stdin\n", timeout=5)
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
        _stdout, stderr = process.communicate("", timeout=5)
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
        _stdout, stderr = process.communicate("", timeout=5)
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
                _stdout, stderr = process.communicate("", timeout=5)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual([event["phase"] for event in self.calls()],
                                 ["metadata", "compiler", "compiler-stdin"])
                self.assert_preflight_unslotted(self.calls())
                self.assertTrue(self.slot_is_free())


class TwoSlots(unittest.TestCase):
    """Two private slots, one of which a gated build can hold."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-memory-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = self.root / "cargo-gate"
        shutil.copy2(ROOT / "scripts/cargo-gate", self.gate)
        shutil.copy2(ROOT / "scripts/cargo-gate-command.py", self.root / "cargo-gate-command.py")
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CAGENTS_BUILD_") and key != "CARGO_BUILD_JOBS"
        } | {"CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"), "CAGENTS_BUILD_SLOTS": "2",
              "CAGENTS_BUILD_CPU_PRESSURE": "0", "CAGENTS_BUILD_REMOTE": "local",
              "CAGENTS_BUILD_INSTALLED_GATE": ""}

    def hold(self, cost):
        holder = subprocess.Popen(
            [str(self.gate), "exec", "sh", "-c", f"touch {self.root}/held; "
             f"while [ ! -e {self.root}/release ]; do sleep 0.05; done"],
            env=self.env | {"CAGENTS_BUILD_COST_GIB": str(cost)}, cwd=self.root,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(holder.wait, 10)
        self.holder = holder
        self.addCleanup((self.root / "release").touch)
        deadline = time.monotonic() + 10
        while not (self.root / "held").exists():
            self.assertLess(time.monotonic(), deadline, "holder never admitted")
            time.sleep(0.02)
        return holder

    def build(self, cost, timeout):
        return subprocess.run(
            [str(self.gate), "exec", "sh", "-c", 'printf "%s" "$CARGO_BUILD_JOBS"'],
            env=self.env | {"CAGENTS_BUILD_COST_GIB": str(cost)}, cwd=self.root,
            capture_output=True, text=True, timeout=timeout,
        )


class MemoryAdmission(TwoSlots):
    """One slot held by a gated build that records its expected cost."""

    def test_a_build_that_fits_starts_beside_a_running_one(self):
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)
        # It withdrew its request for the held slot; dash would report that.
        self.assertNotIn("Terminated", result.stderr)
        cpus = os.cpu_count()
        self.assertEqual(int(result.stdout), max(4, min(8, cpus // 2)))
        # Released, the slot no longer prices its last holder.
        (self.root / "release").touch()
        self.holder.wait(timeout=10)
        self.assertFalse(any(self.root.glob("slot.lock*.cost")))

    def test_a_build_that_does_not_fit_waits_for_the_running_one(self):
        self.hold(1)
        with self.assertRaises(subprocess.TimeoutExpired):
            self.build(10**6, timeout=2)
        (self.root / "release").touch()
        result = self.build(10**6, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_running_builds_expected_peak_is_reserved(self):
        # The holder has claimed almost none of an expected peak larger than
        # any host's memory, so even a small newcomer must wait.
        self.hold(10**6)
        with self.assertRaises(subprocess.TimeoutExpired):
            self.build(1, timeout=2)

    def test_a_lone_build_gets_half_the_cpus(self):
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(int(result.stdout), max(4, min(8, os.cpu_count() // 2)))

    def test_a_slot_held_by_a_plain_flock_is_priced_not_fatal(self):
        holder = subprocess.Popen(
            ["flock", self.root / "slot.lock", "sh", "-c",
             f"touch {self.root}/held; while [ ! -e {self.root}/release ]; do sleep 0.05; done"])
        self.addCleanup(holder.wait, 10)
        self.addCleanup((self.root / "release").touch)
        while not (self.root / "held").exists():
            time.sleep(0.02)
        # Priced as the largest build, so whether a small build fits depends
        # on this host; either way the gate admits or keeps waiting.
        waiter = subprocess.Popen(
            [str(self.gate), "exec", "true"], env=self.env | {"CAGENTS_BUILD_COST_GIB": "1"},
            cwd=self.root, stderr=subprocess.PIPE, text=True)
        try:
            status = waiter.wait(timeout=2)
        except subprocess.TimeoutExpired:
            waiter.kill()
            status = waiter.wait()
            stderr = waiter.stderr.read()
            self.assertNotIn("unbound", stderr)
        else:
            stderr = waiter.stderr.read()
            self.assertEqual(status, 0, stderr)
        # Its missing cost file is expected, not an error to report.
        self.assertNotIn("No such file", stderr)
        waiter.stderr.close()

    def test_an_invalid_cost_is_refused(self):
        result = self.build("08", timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn("CAGENTS_BUILD_COST_GIB", result.stderr)


ADMISSION_FLOCK = r'''#!/usr/bin/python3
import os
from pathlib import Path
import subprocess
import sys
import time
root = Path(os.environ["FIXTURE_ROOT"])
arguments = sys.argv[1:]
if arguments == ["-n", "6"] and os.environ.get("FIXTURE_GRANT_DELAY"):
    info = os.fstat(6)
    base = root / "slot.lock"
    if base.exists() and info.st_ino == base.stat().st_ino:
        status = subprocess.call(["/usr/bin/flock", *arguments], close_fds=False)
        if status == 0:
            time.sleep(float(os.environ["FIXTURE_GRANT_DELAY"]))
        raise SystemExit(status)
os.execv("/usr/bin/flock", ["flock", *arguments])
'''


class AdmissionTimeout(unittest.TestCase):
    """Reuse private preflight tools; all flock operations remain real."""

    setUpClass = classmethod(PreflightCancellation.setUpClass.__func__)
    tearDownClass = classmethod(PreflightCancellation.tearDownClass.__func__)
    setting = "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS"

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
            'event["order_id"] = os.environ.get("FIXTURE_ORDER_ID", "")\nrecord(event)\n'
            'delay = config.get("delays", {}).get(phase, 0)\n'
            'if delay:\n'
            '    receipt = root / (phase + ".delaying")\n'
            '    receipt.with_suffix(".tmp").write_text(json.dumps(event))\n'
            '    receipt.with_suffix(".tmp").replace(receipt)\n'
            '    time.sleep(delay)\n',
        )
        self.assertIn('if config.get("fast_cancel")', tool)
        self.assertIn('phase + ".delaying"', tool)
        for name in ("bin/cargo", "build-disk-guard", "seed-build-dir"):
            (self.fixture.root / name).write_text(tool)
        flock = self.fixture.root / "bin/flock"
        flock.write_text(ADMISSION_FLOCK)
        flock.chmod(0o700)
        self.fixture.env[self.setting] = "1"

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
        # Observe the actual helper blocked on the exact private lock. No
        # receipt from a PATH-interposed flock establishes helper readiness.
        lock = self.fixture.root / ("slot.lock.queue" if receipt.name.startswith("queue") else "slot.lock")
        owned = None

        def waiting():
            nonlocal owned
            children = Path(f"/proc/{process.pid}/task/{process.pid}/children").read_text().split()
            for value in children:
                pid = int(value)
                try:
                    pidfd = os.pidfd_open(pid)
                except ProcessLookupError:
                    continue
                try:
                    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                    arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                    if int(stat[1]) != process.pid or b"--wait-lock" not in arguments:
                        continue
                    index = arguments.index(b"--wait-lock")
                    descriptor = int(arguments[index + 2])
                    observed = Path(f"/proc/{pid}/fd/{descriptor}").stat()
                    expected = lock.stat()
                    if ((observed.st_dev, observed.st_ino) == (expected.st_dev, expected.st_ino)
                            and "lock" in Path(f"/proc/{pid}/wchan").read_text()):
                        owned = pid, pidfd
                        self.fixture.descendants.append(owned)
                        pidfd = None
                        return True
                except (FileNotFoundError, ProcessLookupError):
                    pass
                finally:
                    if pidfd is not None:
                        os.close(pidfd)
            return False

        self.fixture.wait_for(waiting, "native lock waiter for " + receipt.name)
        return owned

    def retire(self, owned):
        pid, pidfd = owned
        self.fixture.wait_for(lambda: bool(select.select([pidfd], [], [], 0)[0]), "owned waiter exit")
        try:
            caller_reaped = os.waitpid(pid, os.WNOHANG)[0] == pid
        except ChildProcessError:
            caller_reaped = False
        return caller_reaped

    def finish_blocked(self, process, holder, *, seconds=2):
        # Baseline gates have no timeout; release only our outside holder to
        # let those owned waits finish, retaining the causal failure result.
        completed_while_held = True
        try:
            stdout, stderr = process.communicate("", timeout=seconds)
        except subprocess.TimeoutExpired:
            completed_while_held = False
            fcntl.flock(holder, fcntl.LOCK_UN)
            stdout, stderr = process.communicate(timeout=5)
        return completed_while_held, stdout, stderr

    def assert_timeout(self, process, stderr):
        self.assertEqual(process.returncode, 124, stderr)
        self.assertIn("admission", stderr.lower())
        self.assertIn("tim", stderr.lower())
        self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])
        self.assertTrue(self.fixture.slot_is_free())

    def test_expiry_in_each_preflight_hook_reaps_without_compiling(self):
        for phase in ("metadata", "guard", "seed"):
            with self.subTest(phase=phase):
                self.reset(delays={phase:2})
                process = self.fixture.start()
                owned = self.pin(self.fixture.root / (phase + ".delaying"))
                _stdout, stderr = process.communicate("", timeout=5)
                caller_reaped = self.retire(owned)
                self.assert_timeout(process, stderr)
                self.assertFalse(caller_reaped)
                self.fixture.assert_preflight_unslotted(self.fixture.calls())

    def test_one_budget_covers_cumulative_hooks_and_seed_is_not_fail_soft_on_expiry(self):
        # Two seconds, so a loaded host still reaches the seed hook within the budget.
        self.reset(delays={"metadata":0.7, "guard":0.7, "seed":1.2})
        process = self.fixture.start(environment=self.fixture.env | {self.setting:"2"})
        _stdout, stderr = process.communicate("", timeout=8)
        self.assert_timeout(process, stderr)
        self.assertEqual([event["phase"] for event in self.fixture.calls()], ["metadata", "guard", "seed"])

    def test_queue_base_and_no_fit_expiry_withdraws_owned_waiter_and_preserves_holder(self):
        for kind in ("queue", "base", "memory", "cpu-pressure", "cpu-settle"):
            with self.subTest(kind=kind):
                self.reset()
                root = self.fixture.root
                path = root / ("slot.lock.queue" if kind == "queue" else "slot.lock")
                cost = root / "slot.lock.cost"
                if kind != "queue":
                    cost.write_text("outside-holder-sentinel\n")
                environment = self.fixture.env.copy()
                if kind == "memory":
                    environment |= {"CAGENTS_BUILD_SLOTS":"2", "CAGENTS_BUILD_COST_GIB":"1000000",
                                    "CAGENTS_BUILD_CPU_PRESSURE":"0"}
                if kind.startswith("cpu-"):
                    pressure = root / "cpu-pressure"
                    pressure.write_text("some avg10=" + ("99.00" if kind == "cpu-pressure" else "0.00")
                                        + " avg60=0.00 avg300=0.00 total=0\n")
                    since = Path("/proc/uptime").read_text().split()[0].split(".")[0]
                    cost.write_text(f"{os.getpid()} 0\n{since}\n")
                    environment |= {"CAGENTS_BUILD_SLOTS":"2", "CAGENTS_BUILD_COST_GIB":"1",
                                    "CAGENTS_BUILD_CPU_PRESSURE_FILE":str(pressure),
                                    "CAGENTS_BUILD_CPU_PRESSURE":"60",
                                    "CAGENTS_BUILD_CPU_SETTLE_SECS":"0" if kind == "cpu-pressure" else "3600"}
                expected_cost = cost.read_text() if kind != "queue" else None
                with path.open("a") as holder:
                    fcntl.flock(holder, fcntl.LOCK_EX)
                    process = self.fixture.start(environment=environment)
                    owned = self.pin(root / ("queue.waiting" if kind == "queue" else "base.waiting"), process=process)
                    completed, _stdout, stderr = self.finish_blocked(process, holder)
                    caller_reaped = self.retire(owned)
                    self.assertTrue(completed, "gate required its outside holder to release")
                    self.assertEqual(process.returncode, 124, stderr)
                    self.assertIn("admission", stderr.lower())
                    self.assertEqual(stderr.count("build admission timed out"), 1, stderr)
                    self.assertFalse(caller_reaped)
                    if kind != "queue":
                        self.assertEqual(cost.read_text(), expected_cost)
                    if kind == "cpu-pressure":
                        self.assertIn("CPUs busy (99% pressure, limit 60%)", stderr)
                    self.assertNotIn("compiler", [event["phase"] for event in self.fixture.calls()])
                self.assertTrue(self.fixture.slot_is_free())
                with (root / "slot.lock.queue").open("a") as queue:
                    fcntl.flock(queue, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertFalse((root / "slot.lock.1.cost").exists())

    def test_each_direct_signal_cancels_queued_wait_without_timeout_or_orphan(self):
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            with self.subTest(signal=number):
                self.reset()
                with (self.fixture.root / "slot.lock.queue").open("a") as holder:
                    fcntl.flock(holder, fcntl.LOCK_EX)
                    process = self.fixture.start(environment=self.fixture.env | {self.setting:"5"})
                    owned = self.pin(self.fixture.root / "queue.waiting", process=process)
                    process.send_signal(number)
                    completed, _stdout, stderr = self.finish_blocked(process, holder, seconds=1)
                    caller_reaped = self.retire(owned)
                    self.assertTrue(completed, "cancellation left a waiter behind its outside holder")
                    self.assertEqual(process.returncode, 128 + number, stderr)
                    self.assertFalse(caller_reaped)
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
                    self.setting:"5", "FIXTURE_ORDER_ID":identifier})
                processes.append(process)
                self.pin(root / ("queue-" + identifier + ".waiting"), process=process)
            fcntl.flock(holder, fcntl.LOCK_UN)
            for process in processes:
                _stdout, stderr = process.communicate("", timeout=5)
                self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual([event["order_id"] for event in self.fixture.calls() if event["phase"] == "compiler"],
                         ["first", "second"])

    def test_invalid_timeout_is_refused_before_hooks(self):
        for value in ("", "0", "00", "-1", "+1", "1.0", "1s", " 1", "1 ", "١", "１", "1\n"):
            with self.subTest(value=value):
                self.reset()
                process = self.fixture.start(environment=self.fixture.env | {self.setting:value})
                _stdout, stderr = process.communicate("", timeout=5)
                self.assertEqual(process.returncode, 2, stderr)
                self.assertIn(self.setting, stderr)
                self.assertEqual(self.fixture.calls(), [])

    def test_ordinary_guard_exit124_is_not_relabelled_as_timeout(self):
        self.reset(guard_exit=124)
        process = self.fixture.start(environment=self.fixture.env | {self.setting:"5"})
        _stdout, stderr = process.communicate("", timeout=5)
        self.assertEqual(process.returncode, 124, stderr)
        self.assertNotIn("timed out", stderr.lower())
        self.assertEqual([event["phase"] for event in self.fixture.calls()], ["metadata", "guard"])

    def test_leading_zero_budget_does_not_limit_an_admitted_build(self):
        # Admission must fit in the budget on a loaded host; the compiler then outlasts it.
        self.reset(delays={"compiler":3.25}, compiler_exit=17)
        process = self.fixture.start(environment=self.fixture.env | {self.setting:"0003"})
        _stdout, stderr = process.communicate("", timeout=10)
        self.assertEqual(process.returncode, 17, stderr)
        self.assertNotIn("timed out", stderr.lower())
        compiler = next(event for event in self.fixture.calls() if event["phase"] == "compiler")
        self.assertTrue(compiler["held"])
        self.assertTrue(compiler["marker"])
        self.assertTrue(self.fixture.slot_is_free())

    def test_grant_after_deadline_does_not_create_a_claim_or_start_compiler(self):
        self.reset()
        process = self.fixture.start(environment=self.fixture.env | {"FIXTURE_GRANT_DELAY":"1.15"})
        _stdout, stderr = process.communicate("", timeout=5)
        self.assert_timeout(process, stderr)
        self.assertFalse(any(self.fixture.root.glob("slot.lock*.cost")))

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
            _stdout, stderr = process.communicate("", timeout=5)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertNotIn("timed out", stderr.lower())

class CpuPressureAdmission(TwoSlots):
    """Two slots and memory to spare; the CPU pressure comes from a fixture."""

    def setUp(self):
        super().setUp()
        self.pressure = self.root / "pressure"
        self.env |= {
            "CAGENTS_BUILD_CPU_PRESSURE": "60", "CAGENTS_BUILD_CPU_SETTLE_SECS": "0",
            "CAGENTS_BUILD_CPU_PRESSURE_FILE": str(self.pressure),
        }

    def report(self, some, full="0.00"):
        self.pressure.write_text(
            f"some avg10={some} avg60=40.12 avg300=38.40 total=30831090039\n"
            f"full avg10={full} avg60=0.00 avg300=0.00 total=0\n")

    def test_a_build_starts_beside_a_running_one_while_cpus_have_room(self):
        self.report("59.99", full="99.00")
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)

    def test_a_build_waits_while_cpu_pressure_is_at_the_limit(self):
        self.report("60.00")
        self.hold(1)
        waiter = subprocess.Popen(
            [str(self.gate), "exec", "true"], env=self.env | {"CAGENTS_BUILD_COST_GIB": "1"},
            cwd=self.root, stderr=subprocess.PIPE, text=True)
        self.addCleanup(waiter.stderr.close)
        with self.assertRaises(subprocess.TimeoutExpired):
            waiter.wait(timeout=2)
        # Pressure falling admits it beside the running build.
        self.report("12.50")
        self.assertEqual(waiter.wait(timeout=10), 0)
        self.assertIsNone(self.holder.poll())
        self.assertIn("CPUs busy (60% pressure, limit 60%)", waiter.stderr.read())

    def test_a_lone_build_starts_whatever_the_pressure(self):
        self.report("100.00")
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_build_waits_until_the_newest_one_can_show_in_the_pressure(self):
        self.report("0.00")
        self.hold(1)
        with self.assertRaises(subprocess.TimeoutExpired):
            subprocess.run(
                [str(self.gate), "exec", "true"],
                env=self.env | {"CAGENTS_BUILD_COST_GIB": "1", "CAGENTS_BUILD_CPU_SETTLE_SECS": "3600"},
                cwd=self.root, capture_output=True, timeout=2)

    def test_a_host_without_pressure_reports_has_room(self):
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_zero_disables_the_pressure_limit(self):
        self.report("100.00")
        self.env["CAGENTS_BUILD_CPU_PRESSURE"] = "0"
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_invalid_limits_are_refused(self):
        for name, value in (("CAGENTS_BUILD_CPU_PRESSURE", "060"),
                            ("CAGENTS_BUILD_CPU_PRESSURE", "101"),
                            ("CAGENTS_BUILD_CPU_PRESSURE", "high"),
                            ("CAGENTS_BUILD_CPU_SETTLE_SECS", "-1")):
            self.env[name] = value
            result = self.build(1, timeout=10)
            self.assertEqual(result.returncode, 2, (name, value))
            self.assertIn(name, result.stderr)
            del self.env[name]

    def held_slot(self, index, sidecar):
        """Holds slot `index` with a plain `flock` and prices it with `sidecar`,
        a gate's cost file, whose holder is this test."""
        path = self.root / ("slot.lock" if index == 0 else f"slot.lock.{index}")
        holder = subprocess.Popen(
            ["flock", path, "sh", "-c",
             f"touch {path}.held; while [ ! -e {self.root}/release ]; do sleep 0.05; done"])
        self.addCleanup(holder.wait, 10)
        self.addCleanup((self.root / "release").touch)
        while not Path(f"{path}.held").exists():
            time.sleep(0.02)
        Path(f"{path}.cost").write_text(sidecar.format(pid=os.getpid()))

    @staticmethod
    def uptime():
        return int(float(Path("/proc/uptime").read_text().split()[0]))

    def test_the_newest_of_several_running_builds_must_have_settled(self):
        self.report("0.00")
        self.env |= {"CAGENTS_BUILD_SLOTS": "3", "CAGENTS_BUILD_CPU_SETTLE_SECS": "60"}
        self.held_slot(1, f"{{pid}} 1\n{self.uptime() - 600}\n")
        self.held_slot(2, f"{{pid}} 1\n{self.uptime()}\n")
        with self.assertRaises(subprocess.TimeoutExpired):
            self.build(1, timeout=2)

    def test_a_start_from_before_a_reboot_has_settled(self):
        self.report("0.00")
        self.env |= {"CAGENTS_BUILD_SLOTS": "3", "CAGENTS_BUILD_CPU_SETTLE_SECS": "60"}
        self.held_slot(1, f"{{pid}} 1\n{self.uptime() + 10**6}\n")
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def assert_start_selection(self, starts, *, waits, disabled=False, readable=True):
        # Redirect only meminfo through the actual sed executable. The tested
        # 7 GiB cost always fits this synthetic 64 GiB; no RAM is allocated.
        binary = self.root / "start-tools"
        binary.mkdir(exist_ok=True)
        memory = self.root / "start-meminfo"
        memory.write_text("MemTotal:       67108864 kB\nMemAvailable:   67108864 kB\n")
        sed = binary / "sed"
        sed.write_text(
            "#!/usr/bin/python3\nimport os,sys\n"
            "arguments = [os.environ['FIXTURE_MEMINFO'] if argument == '/proc/meminfo' "
            "else argument for argument in sys.argv[1:]]\n"
            "os.execv('/usr/bin/sed', ['sed', *arguments])\n"
        )
        sed.chmod(0o700)
        temporary = self.root / "start-tmp"
        temporary.mkdir(exist_ok=True)
        command = self.root / "start-command"
        command.unlink(missing_ok=True)
        self.report("0.00")
        if not readable:
            self.pressure.write_text("some total=12\n")
        environment = {
            "PATH": str(binary) + ":/usr/bin:/bin", "TMPDIR": str(temporary), "LC_ALL": "C",
            "FIXTURE_MEMINFO": str(memory), "CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"),
            "CAGENTS_BUILD_SLOTS": "3", "CARGO_BUILD_JOBS": "1",
            "CAGENTS_BUILD_CPU_PRESSURE": "0" if disabled else "60",
            "CAGENTS_BUILD_CPU_SETTLE_SECS": "60",
            "CAGENTS_BUILD_CPU_PRESSURE_FILE": str(self.pressure),
            "CAGENTS_BUILD_COST_GIB": "7", "CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS": "1",
            "CAGENTS_BUILD_REMOTE": "local", "CAGENTS_BUILD_INSTALLED_GATE": "",
        }
        now = self.uptime()
        dates = {"future":now + 10**6, "fresh":now, "old":now - 600}
        holders, costs = [], {}
        try:
            for index, start in enumerate(starts, 1):
                path = self.root / f"slot.lock.{index}"
                holder = path.open("a")
                holders.append(holder)
                fcntl.flock(holder, fcntl.LOCK_EX)
                cost = Path(str(path) + ".cost")
                cost.write_text(f"{os.getpid()} 1\n{dates[start]}\n")
                costs[cost] = cost.read_bytes()
            process = subprocess.Popen(
                [str(self.gate), "exec", "/bin/sh", "-c", 'printf admitted > "$1"',
                 "fixture", str(command)], cwd=self.root, env=environment,
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            try:
                _stdout, stderr = process.communicate(timeout=5)
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                process.communicate(timeout=5)
            for cost, expected in costs.items():
                self.assertEqual(cost.read_bytes(), expected)
                with Path(str(cost).removesuffix(".cost")).open("a") as probe:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            for name in ("slot.lock", "slot.lock.queue"):
                with (self.root / name).open("a") as probe:
                    fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse((self.root / "slot.lock.cost").exists())
            self.assertEqual((process.returncode, command.exists()), (124, False) if waits else (0, True), stderr)
            if waits:
                self.assertIn("admission", stderr.lower())
        finally:
            for holder in holders:
                holder.close()

    def test_a_future_start_cannot_mask_a_fresh_holder_in_either_scan_order(self):
        for starts in (("future", "fresh"), ("fresh", "future")):
            with self.subTest(starts=starts):
                self.assert_start_selection(starts, waits=True)

    def test_start_selection_preserves_admission_and_pressure_read_error_controls(self):
        for starts, waits, disabled, readable in (
            (("future",), False, False, True), (("old",), False, False, True),
            (("fresh",), True, False, True), (("future", "fresh"), False, True, True),
            (("old",), False, False, False), (("future", "fresh"), True, False, False),
        ):
            with self.subTest(starts=starts, disabled=disabled, readable=readable):
                self.assert_start_selection(starts, waits=waits, disabled=disabled, readable=readable)

    def test_settled_and_undated_running_builds_leave_room(self):
        # A build priced by a gate that does not date its start is not waited for.
        self.report("0.00")
        self.env |= {"CAGENTS_BUILD_SLOTS": "3", "CAGENTS_BUILD_CPU_SETTLE_SECS": "60"}
        self.held_slot(1, f"{{pid}} 1\n{self.uptime() - 600}\n")
        self.held_slot(2, "{pid} 1\n")
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_dated_cost_still_reads_as_a_cost_to_gates_that_only_price(self):
        self.report("0.00")
        self.hold(1)
        cost = next(self.root.glob("slot.lock*.cost")).read_text()
        # The one-line read of gates that predate dated starts.
        result = subprocess.run(
            ["sh", "-c", 'read -r holder claimed; printf "%s:%s" "$holder" "$claimed"'],
            input=cost, capture_output=True, text=True, check=True)
        holder, claimed = result.stdout.split(":")
        self.assertEqual(int(holder), self.holder.pid)
        self.assertEqual(int(claimed), 2**20)

    def test_an_unreadable_pressure_report_leaves_room(self):
        self.pressure.write_text("some total=12\n")
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_default_slots_are_capped_at_four(self):
        del self.env["CAGENTS_BUILD_SLOTS"]
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        slots = {path.name for path in self.root.glob("slot.lock*")
                 if not path.name.endswith((".queue", ".cost"))}
        self.assertLessEqual(slots, {"slot.lock", "slot.lock.1", "slot.lock.2", "slot.lock.3"})
        memory_gib = os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE") / 2**30
        if os.cpu_count() >= 20 and memory_gib >= 28:
            self.assertEqual(len(slots), 4)



if __name__ == "__main__":
    unittest.main()
