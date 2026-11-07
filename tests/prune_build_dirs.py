#!/usr/bin/env python3
"""Exercise scripts/prune-build-dirs on private directories only."""

import fcntl
import json
import os
import pwd
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
PRUNER = ROOT / "scripts/prune-build-dirs"
# Build output counts as idle after IDLE seconds here, so tests wait
# SETTLE seconds instead of faking times: a change time cannot be faked.
IDLE = 1.0
SETTLE = 2.0


class PruneBuildDirs(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-prune-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.roots = self.root / "roots"
        self.roots.mkdir(parents=True)
        self.home.mkdir()
        self.log = self.root / "removed.tsv"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_BUILD_")}
        # Only private directories are ever searched.
        self.env |= {"HOME": str(self.home), "CAGENTS_BUILD_PRUNE_ROOTS": str(self.roots)}

    def prune(self, *arguments, apply=True, status=0):
        command = [str(PRUNER), "--lock", str(self.root / "lock"), "--log", str(self.log),
                   "--mirror", str(self.roots / "cargo-build"), "--verbose",
                   "--max-idle", str(IDLE / 86400), "--orphan-hours", str(IDLE / 3600)]
        result = subprocess.run(command + (["--apply"] if apply else []) + list(arguments),
                                env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, status, result.stderr)
        return result.stdout

    def target(self, path, marker="rustc"):
        """A Cargo target directory, idle once settled."""
        path.mkdir(parents=True)
        if marker == "rustc":
            (path / ".rustc_info.json").write_text("{}")
        else:
            # A separate build directory: Cargo tags it.
            (path / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
        (path / "debug/.fingerprint/crate-0123").mkdir(parents=True)
        (path / "debug/.cargo-lock").write_text("")
        (path / "debug/deps").mkdir()
        (path / "debug/deps/libcrate.rlib").write_bytes(b"x" * 8192)
        return path

    def settle(self):
        time.sleep(SETTLE)

    def test_removes_idle_build_output_wherever_it_is(self):
        worktree = self.target(self.roots / "repo/.worktrees/old/target")
        lane = self.target(self.roots / "lane/target-after")
        separate = self.target(self.roots / "build/target", marker="fingerprint")
        self.settle()
        output = self.prune()
        for path in (worktree, lane, separate):
            self.assertFalse(path.exists(), path)
            self.assertIn(f"removed {path}", output)
        # The worktree that held it stays.
        self.assertTrue((self.roots / "repo/.worktrees/old").is_dir())
        self.assertEqual(len(self.log.read_text().splitlines()), 3)

    def test_a_recent_write_anywhere_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        time.sleep(7)
        (path / "debug/deps/libcrate.rlib").touch()
        # Older entries are idle at 6 seconds; the one just written is not.
        self.prune("--max-idle", str(6 / 86400))
        self.assertTrue(path.exists())

    def test_a_copy_that_kept_its_sources_times_is_recent(self):
        path = self.target(self.roots / "lane/target")
        time.sleep(7)
        # As `cp -p` leaves it: an old modification time, a new change time.
        copied = path / "debug/deps/libcopied.rlib"
        copied.write_bytes(b"x")
        os.utime(copied, (time.time() - 30 * 86400,) * 2)
        # Older entries are idle at 6 seconds; the copy is not.
        self.prune("--max-idle", str(6 / 86400))
        self.assertTrue(path.exists())

    def test_idle_days_are_configurable(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        self.prune("--max-idle", str(60 / 86400))
        self.assertTrue(path.exists())
        self.prune()
        self.assertFalse(path.exists())

    def test_a_held_cargo_lock_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        with open(path / "debug/.cargo-lock") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.settle()
            output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn("a build holds its lock", output)

    def test_a_process_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        process = subprocess.Popen(["sleep", "30"], cwd=path / "debug")
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        self.settle()
        output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn(f"process {process.pid} uses it", output)

    def test_an_open_file_inside_keeps_it(self):
        path = self.target(self.roots / "lane/target")
        process = subprocess.Popen(
            ["python3", "-c", "import sys, time; f = open(sys.argv[1]); print(flush=True); "
             "time.sleep(30)", str(path / "debug/deps/libcrate.rlib")],
            stdout=subprocess.PIPE, text=True)
        self.addCleanup(process.stdout.close)
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        process.stdout.readline()
        self.settle()
        output = self.prune()
        self.assertTrue(path.exists())
        self.assertIn(f"process {process.pid} uses it", output)

    def test_other_caches_are_never_taken_for_build_output(self):
        venv = self.roots / "state/.venv"
        venv.mkdir(parents=True)
        (venv / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
        (venv / "debug").mkdir()
        evidence = self.roots / "evidence"
        evidence.mkdir()
        (evidence / "handoff.md").write_text("notes")
        self.settle()
        self.prune()
        self.assertTrue(venv.exists())
        self.assertTrue(evidence.exists())

    def evidence_task(self, path):
        """A native-evidence task whose after side was built in a shared slot:
        its target holds only the executables that came back."""
        (path / "shots").mkdir(parents=True)
        (path / ".native-evidence").write_text("")
        (path / "shots/after-board.webp").write_bytes(b"webp")
        (path / "fixture/home").mkdir(parents=True)
        target = path / "target-after"
        (target / "dev-runtime").mkdir(parents=True)
        for name in ("cagents", "cagentsd", "cagents-ui"):
            (target / "dev-runtime" / name).write_bytes(b"x" * 8192)
        (target / "evidence-source").write_text("checkout\n")
        return target

    def test_a_native_evidence_build_goes_after_its_hours_and_its_task_stays(self):
        task = self.roots / "cev/task"
        target = self.evidence_task(task)
        # One built in the task's own target, as older tools and
        # CAGENTS_BUILD_REMOTE=local leave it.
        private = self.target(task / "target-before")
        self.settle()
        # A week's grace for other build output, an hour for these.
        self.prune("--max-idle", "7", "--evidence-hours", "1")
        self.assertTrue(target.exists())
        output = self.prune("--max-idle", "7", "--evidence-hours", str(IDLE / 3600))
        for path in (target, private):
            self.assertFalse(path.exists(), path)
            self.assertIn(f"removed {path}", output)
        self.assertIn("native-evidence build", output)
        # Its screenshots, fixture and marker are not build output.
        self.assertTrue((task / "shots/after-board.webp").exists())
        self.assertTrue((task / "fixture/home").is_dir())
        self.assertTrue((task / ".native-evidence").exists())

    def test_a_native_evidence_build_waits_no_longer_than_other_build_output(self):
        target = self.evidence_task(self.roots / "cev/task")
        self.settle()
        self.prune("--evidence-hours", "24")
        self.assertFalse(target.exists())

    def test_only_a_marked_tasks_sides_are_native_evidence_builds(self):
        unmarked = self.evidence_task(self.roots / "cev/unmarked")
        (unmarked.parent / ".native-evidence").unlink()
        task = self.roots / "cev/task"
        other = self.evidence_task(task).with_name("target-other")
        (task / "target-after").rename(other)
        (task / ".native-evidence").unlink()
        (task / ".native-evidence").mkdir()
        checkout = self.evidence_task(self.roots / "cev/checkout")
        (checkout / ".git").mkdir()
        self.settle()
        self.prune("--evidence-hours", "0")
        for path in (unmarked, other, checkout):
            self.assertTrue(path.exists(), path)

    def test_a_session_holding_a_native_evidence_build_open_keeps_it(self):
        target = self.evidence_task(self.roots / "cev/task")
        # As the session's supervisor holds it while it waits for the native lock.
        descriptor = os.open(target, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, descriptor)
        self.settle()
        output = self.prune("--evidence-hours", "0")
        self.assertTrue(target.exists())
        self.assertIn(f"keep {target}: process", output)

    def test_running_a_native_evidence_build_keeps_it(self):
        target = self.evidence_task(self.roots / "cev/task")
        executable = target / "dev-runtime/cagents-ui"
        executable.write_bytes(Path("/bin/sleep").read_bytes())
        executable.chmod(0o755)
        self.settle()
        process = subprocess.Popen([str(executable), "30"])
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        output = self.prune("--evidence-hours", "0")
        self.assertTrue(target.exists())
        self.assertIn(f"process {process.pid} uses it", output)

    def test_a_session_naming_its_task_keeps_both_its_builds(self):
        # A session started by an older native-evidence, which holds neither
        # target open, waiting for the native lock before its application runs.
        task = self.roots / "cev/task"
        after = self.evidence_task(task)
        before = self.target(task / "target-before")
        other = self.evidence_task(self.roots / "cev/other")
        script = self.root / "worktree/scripts/native-evidence"
        script.parent.mkdir(parents=True)
        script.write_text("sleep 30\n")
        unrelated = self.root / "worktree/scripts/other-tool"
        unrelated.write_text("sleep 30\n")
        def run(*command):
            process = subprocess.Popen(command, cwd=self.root)
            self.addCleanup(process.wait)
            self.addCleanup(process.kill)
            return process
        session = run("bash", str(script), "--task", str(task), "__session", "after")
        # As a task named from its own working directory.
        relative = run("bash", str(script), "--task", str(task.relative_to(self.root)), "start")
        # Another tool's --task, and another task's session, keep nothing.
        run("bash", str(unrelated), "--task", str(other.parent))
        run("bash", str(script), "--task", str(self.roots / "cev/elsewhere"), "__session")
        self.settle()
        output = self.prune("--evidence-hours", "0")
        for path in (after, before):
            self.assertTrue(path.exists(), path)
            self.assertIn(f"keep {path}: process {session.pid} uses it", output)
        self.assertFalse(other.exists())
        session.kill()
        session.wait()
        self.settle()
        output = self.prune("--evidence-hours", "0")
        self.assertIn(f"keep {after}: process {relative.pid} uses it", output)
        relative.kill()
        relative.wait()
        self.prune("--evidence-hours", "0")
        self.assertFalse(after.exists())
        self.assertFalse(before.exists())

    def test_a_moved_home_still_searches_the_accounts_own(self):
        # A fixture's builds, and so the pruner they start, run under a
        # private HOME that holds none of the account's builds. Searching only
        # that would find nothing, yet stamp the schedule.
        account = Path(pwd.getpwuid(os.geteuid()).pw_dir)
        self.assertNotEqual(account, self.home)
        roots, never = self.homes()
        self.assertEqual(roots, [str(account / ".cache"), str(account / ".local/state"),
                                 str(self.home / ".cache"), str(self.home / ".local/state"),
                                 "/var/tmp"])
        # Both homes' installs and offload slots stay untouched.
        for home in (account, self.home):
            for place in (".local/share", ".cache/cagents-offload"):
                self.assertIn(str(home / place), never)

    def test_an_account_without_a_password_entry_searches_its_home(self):
        # As a container's arbitrary user is.
        roots, never = self.homes(registered=False)
        self.assertEqual(roots, [str(self.home / ".cache"), str(self.home / ".local/state"),
                                 "/var/tmp"])
        self.assertEqual(never, [str(self.home / ".local/share"),
                                 str(self.home / ".cache/cagents-offload")])

    def homes(self, registered=True):
        """The pruner's default roots and never-touched directories."""
        probe = """
import importlib.machinery, importlib.util, json, pwd, sys
def unregistered(uid):
    raise KeyError(uid)
if sys.argv[2] == "unregistered":
    pwd.getpwuid = unregistered
loader = importlib.machinery.SourceFileLoader("pruner", sys.argv[1])
pruner = importlib.util.module_from_spec(importlib.util.spec_from_loader("pruner", loader))
loader.exec_module(pruner)
print(json.dumps([[str(path) for path in pruner.default_roots()],
                  [str(path) for path in pruner.never()]]))
"""
        result = subprocess.run(
            [sys.executable, "-c", probe, str(PRUNER), "registered" if registered else "unregistered"],
            env=self.env, capture_output=True, text=True, check=True, timeout=30)
        return json.loads(result.stdout)

    def test_kept_and_protected_directories_stay(self):
        kept = self.target(self.roots / "repo/target")
        self.settle()
        self.prune("--keep", str(self.roots / "repo/target"))
        self.assertTrue(kept.exists())
        # The user's installs and the offload slots are never searched, even
        # when a root contains them.
        self.env["CAGENTS_BUILD_PRUNE_ROOTS"] = str(self.home)
        share = self.target(self.home / ".local/share/cagents/target")
        slot = self.target(self.home / ".cache/cagents-offload/cagents/slot-0/src/target")
        self.settle()
        self.prune()
        self.assertTrue(share.exists())
        self.assertTrue(slot.exists())

    def test_the_search_depth_is_bounded_except_in_the_mirror(self):
        deep = self.target(self.roots / "a/b/c/d/e/f/g/target")
        mirrored = self.target(self.roots / "cargo-build/a/b/c/d/e/f/g/target")
        self.settle()
        self.prune()
        self.assertTrue(deep.exists())
        self.assertFalse(mirrored.exists())
        self.prune("--max-depth", "8")
        self.assertFalse(deep.exists())

    def test_the_main_checkouts_build_stays_wherever_cargo_puts_it(self):
        checkout = self.root / "repo"
        in_tree = self.target(checkout / "target")
        mirrored = self.target(self.roots / "cargo-build" / str(checkout).lstrip("/") / "target")
        self.settle()
        self.prune("--root", str(checkout), "--keep-checkout", str(checkout))
        self.assertTrue(in_tree.exists())
        self.assertTrue(mirrored.exists())

    def test_another_main_checkouts_build_waits_longer_and_a_worktrees_goes(self):
        main = self.roots / "other-repo"
        (main / ".git").mkdir(parents=True)
        kept = self.target(main / "target")
        worktree = self.roots / "other-repo/.worktrees/w"
        worktree.mkdir(parents=True)
        (worktree / ".git").write_text("gitdir: elsewhere\n")
        removed = self.target(worktree / "target")
        mirrored = self.target(self.roots / "cargo-build" / str(main).lstrip("/") / "target")
        self.settle()
        self.prune()
        self.assertTrue(kept.exists())
        self.assertTrue(mirrored.exists())
        self.assertFalse(removed.exists())
        # Four times the idle limit later, it goes too.
        output = self.prune("--max-idle", str(IDLE / 8 / 86400))
        self.assertFalse(kept.exists())
        self.assertFalse(mirrored.exists())
        self.assertIn("main checkout's", output)

    def test_a_budget_bounds_the_run(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        output = self.prune("--budget", "0", status=75)
        self.assertTrue(path.exists())
        self.assertIn("out of time", output)
        self.assertIn("removed 0 build directories", output)
        # A run that searched everything says so.
        self.prune("--budget", "60")
        self.assertFalse(path.exists())

    def test_it_leaves_the_search_to_another_run(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        with open(self.root / "lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            started = time.monotonic()
            output = self.prune(status=75)
        self.assertLess(time.monotonic() - started, 10)
        self.assertIn("another run is pruning", output)
        self.assertTrue(path.exists())

    def test_the_checkout_and_the_mirror_are_searched_first(self):
        elsewhere = self.target(self.roots / "a/target")
        mirrored = self.target(self.roots / "cargo-build/gone/repo/target")
        checkout = self.root / "repo"
        worktree = self.target(checkout / ".worktrees/w/target")
        self.settle()
        output = self.prune("--root", str(checkout), apply=False)
        order = [line.split()[2] for line in output.splitlines() if line.startswith("would remove /")]
        self.assertEqual(order, [str(worktree), str(mirrored), str(elsewhere)])

    def test_a_checkouts_committed_sources_are_not_searched(self):
        repository = self.root / "repository"
        (repository / "crates/inner").mkdir(parents=True)
        (repository / "Cargo.toml").write_text("[workspace]\n")
        (repository / "crates/inner/lib.rs").write_text("")
        git = ["git", "-C", str(repository), "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run(["git", "init", "-q", str(repository)], check=True)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "sources"], check=True)
        worktree = self.roots / "lane/checkout"
        subprocess.run(git + ["worktree", "add", "-q", "--detach", str(worktree)], check=True)
        hidden = self.target(worktree / "crates/inner/target")
        built = self.target(worktree / "target")
        self.settle()
        self.prune()
        self.assertTrue(hidden.exists())
        self.assertFalse(built.exists())

    def test_symlinks_are_not_followed(self):
        outside = self.target(self.root / "outside/target")
        (self.roots / "link").symlink_to(self.root / "outside")
        self.settle()
        self.prune()
        self.assertTrue(outside.exists())

    def test_an_unreadable_directory_is_skipped_not_fatal(self):
        private = self.roots / "private/target"
        (private / "debug").mkdir(parents=True)
        private.chmod(0)
        self.addCleanup(private.chmod, 0o755)
        path = self.target(self.roots / "lane/target")
        self.settle()
        self.prune()
        self.assertFalse(path.exists())

    def test_a_dry_run_removes_nothing(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        output = self.prune(apply=False)
        self.assertTrue(path.exists())
        self.assertIn(f"would remove {path}", output)

    def test_a_removed_workspaces_mirrored_build_directory_goes_sooner(self):
        mirror = self.roots / "cargo-build"
        workspace = self.root / "workspaces/live"
        workspace.mkdir(parents=True)
        live = self.target(mirror / str(workspace).lstrip("/") / "target")
        gone = self.target(mirror / str(self.root / "workspaces/gone").lstrip("/") / "target")
        self.settle()
        self.prune("--max-idle", str(60 / 86400))
        self.assertTrue(live.exists())
        self.assertFalse(gone.exists())
        self.assertIn("workspace removed", self.log.read_text())

    def test_an_interrupted_removal_is_finished(self):
        leftover = self.roots / "lane/.target.cagents-pruned-999999999"
        (leftover / "debug").mkdir(parents=True)
        (leftover / "debug/partial").write_text("")
        live = self.target(self.roots / "lane/target")
        self.settle()
        self.prune("--max-idle", str(60 / 86400))
        self.assertFalse(leftover.exists())
        self.assertTrue(live.exists())

    def test_a_checkout_is_never_taken_for_build_output(self):
        # As a target directory configured to be the checkout would leave it.
        checkout = self.target(self.roots / "lane/checkout")
        (checkout / "Cargo.toml").write_text("[workspace]\n")
        (checkout / "notes.md").write_text("unsaved work")
        self.settle()
        self.prune()
        self.assertTrue((checkout / "notes.md").exists())

    def test_a_running_removals_renamed_copy_stays(self):
        leftover = self.roots / f"lane/.target.cagents-pruned-{os.getpid()}"
        (leftover / "debug").mkdir(parents=True)
        self.settle()
        self.prune()
        self.assertTrue(leftover.exists())

    def test_a_complete_run_stamps_and_one_cut_short_does_not(self):
        stamp = self.root / "stamp"
        self.prune("--stamp", str(stamp), "--budget", "0", status=75)
        self.assertFalse(stamp.exists())
        self.prune("--stamp", str(stamp))
        self.assertTrue(stamp.exists())

    def test_its_output_goes_to_a_file_keeping_the_last_runs(self):
        output = self.root / "prune.log"
        output.write_text("the last run\n")
        self.assertEqual(self.prune("--output", str(output)), "")
        self.assertEqual(Path(f"{output}.1").read_text(), "the last run\n")
        self.assertIn("removed 0 build directories", output.read_text())

    def test_it_can_wait_for_another_run_and_then_search_nothing(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        lock = open(self.root / "lock", "a")
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX)
        threading.Timer(2, lambda: fcntl.flock(lock, fcntl.LOCK_UN)).start()
        started = time.monotonic()
        output = self.prune("--wait", "--budget", "30", status=75)
        self.assertGreaterEqual(time.monotonic() - started, 1.5)
        self.assertIn("the other run has finished", output)
        self.assertTrue(path.exists())

    # Runs the pruner with in_use and survey replaced, to reach the moments
    # between its checks that a real build could only hit by chance.
    DRIVER = """
import importlib.machinery, importlib.util, os, sys
from pathlib import Path
loader = importlib.machinery.SourceFileLoader("pruner", sys.argv[1])
pruner = importlib.util.module_from_spec(importlib.util.spec_from_loader("pruner", loader))
loader.exec_module(pruner)
in_use, survey, recreate = pruner.in_use, pruner.survey, sys.argv[2]
def late_user(directory, task=None):
    if ".cagents-pruned-" not in directory.name:
        return in_use(directory, task)
    if recreate == "recreate":
        # As a build starting there would: a rename replaces only an empty one.
        (directory.parent / "target/debug").mkdir(parents=True)
    return 1
def late_writer(directory, cutoff):
    return None if ".cagents-pruned-" in directory.name else survey(directory, cutoff)
if sys.argv[3] == "user":
    pruner.in_use = late_user
else:
    pruner.survey = late_writer
sys.argv = [sys.argv[1]] + sys.argv[4:]
sys.exit(pruner.main())
"""

    def race(self, moment, recreate=""):
        path = self.target(self.roots / "lane/target")
        self.settle()
        command = [sys.executable, "-I", "-B", "-c", self.DRIVER, str(PRUNER), recreate, moment,
                   "--lock", str(self.root / "lock"), "--mirror", str(self.roots / "cargo-build"),
                   "--verbose", "--apply", "--max-idle", str(IDLE / 86400)]
        result = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return path, result

    def test_a_process_that_opens_it_as_it_is_renamed_keeps_it(self):
        path, result = self.race("user")
        self.assertIn("used while it was being removed", result.stdout)
        self.assertEqual(os.listdir(self.roots / "lane"), ["target"])
        self.assertTrue((path / "debug/deps/libcrate.rlib").exists())

    def test_a_write_as_it_is_renamed_keeps_it(self):
        path, result = self.race("writer")
        self.assertIn("written while it was being removed", result.stdout)
        self.assertEqual(os.listdir(self.roots / "lane"), ["target"])
        self.assertTrue((path / "debug/deps/libcrate.rlib").exists())

    def test_a_copy_that_cannot_be_put_back_is_kept_aside(self):
        path, result = self.race("user", recreate="recreate")
        self.assertIn("cannot put it back", result.stderr)
        # The build's new directory, and the copy that a later run removes.
        names = sorted(os.listdir(self.roots / "lane"))
        self.assertEqual(len(names), 2)
        self.assertTrue(names[0].startswith(".target.cagents-pruned-"))
        self.assertTrue((self.roots / "lane" / names[0] / "debug/deps/libcrate.rlib").exists())
        self.assertEqual(os.listdir(path), ["debug"])

    def test_a_build_that_starts_after_removal_finds_no_directory(self):
        path = self.target(self.roots / "lane/target")
        self.settle()
        self.prune()
        self.assertFalse(path.exists())
        self.assertEqual(list((self.roots / "lane").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
