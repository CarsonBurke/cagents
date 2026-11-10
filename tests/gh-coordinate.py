#!/usr/bin/python3
"""Offline contracts for the explicitly invoked, repository-local content helper."""
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/gh-coordinate.py"
SPEC = importlib.util.spec_from_file_location("gh_coordinate", SCRIPT)
coordinate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coordinate)
FAKE_GH = r'''#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
started = time.monotonic()
body = sys.stdin.buffer.read()
entry = {"started": started, "argv": sys.argv[1:], "stdin": body.hex()}
fd = os.open(os.environ["FIXTURE_LOG"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, json.dumps(entry).encode() + b"\n")
os.close(fd)
if os.environ.get("FIXTURE_RELEASE"):
    while not Path(os.environ["FIXTURE_RELEASE"]).exists():
        time.sleep(0.01)
time.sleep(float(os.environ.get("FIXTURE_HOLD", "0")))
sys.stdout.buffer.write(bytes.fromhex(os.environ.get("FIXTURE_STDOUT", "6f75740a")))
sys.stderr.buffer.write(bytes.fromhex(os.environ.get("FIXTURE_STDERR", "6572720a")))
sys.exit(int(os.environ.get("FIXTURE_EXIT", "0")))
'''


class CoordinationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "project"
        self.repo.mkdir()
        self.log = self.root / "calls.jsonl"
        self.real = self.root / "fake gh"
        self.real.write_text(FAKE_GH)
        self.real.chmod(0o700)
        self.environment = dict(os.environ, FIXTURE_LOG=str(self.log), GH_HOST="github.com",
                                GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                                GIT_TEMPLATE_DIR=str(self.root / "empty-templates"),
                                XDG_STATE_HOME=str(self.root / "old-state"))
        Path(self.environment["GIT_TEMPLATE_DIR"]).mkdir()
        self.environment.pop("CAGENTS_GH_NONBLOCKING", None)
        for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE"):
            self.environment.pop(name, None)
        self.git("init", "-q", str(self.repo))
        self.children = []

    def tearDown(self):
        for process in self.children:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream:
                    stream.close()
        self.temporary.cleanup()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, env=self.environment,
                              capture_output=True, check=True, timeout=5)

    def command(self, args, wait=30, real=None):
        return [str(SCRIPT), "--real", str(real or self.real), "--wait", str(wait), "--", *args]

    def run_helper(self, args, wait=30, body=b"", cwd=None, extra=None, timeout=3, real=None):
        return subprocess.run(self.command(args, wait, real), cwd=cwd or self.repo,
                              env=dict(self.environment, **(extra or {})), input=body,
                              capture_output=True, timeout=timeout)

    def start(self, args, wait=30, cwd=None, extra=None):
        child = subprocess.Popen(self.command(args, wait), cwd=cwd or self.repo,
                                 env=dict(self.environment, **(extra or {})), stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        return child

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def wait_for_calls(self, count):
        deadline = time.monotonic() + 3
        while len(self.calls()) < count and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertGreaterEqual(len(self.calls()), count, "fake gh did not start")

    def held_writer(self):
        release = self.root / "release"
        child = self.start(["pr", "comment", "1", "-R", "owner/repo", "--body", "fixture"],
                           wait=0, extra={"FIXTURE_RELEASE": str(release)})
        self.wait_for_calls(1)
        self.assertIsNone(child.poll())
        return child, release

    def assert_deferred(self, result):
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertTrue(any(line.startswith(b"cagents-gh: deferred ")
                            for line in result.stderr.splitlines()), result.stderr)
        self.assertNotIn(b"Retry-After", result.stderr)
        self.assertEqual(result.stdout, b"")

    def test_reads_local_and_unknown_commands_never_touch_git_or_old_global_state(self):
        writer, release = self.held_writer()
        old = self.root / "old-state/cagents/gh-observe/coordination"
        old.mkdir(parents=True)
        cooldown = old / "old.json"
        cooldown.write_text(json.dumps({"cooldown_until": time.time() + 3600,
                                        "next_write": time.time() + 3600}))
        before = cooldown.stat().st_mtime_ns
        fakebin = self.root / "fakebin"
        fakebin.mkdir()
        marker = self.root / "unexpected-git"
        fakegit = fakebin / "git"
        fakegit.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 99\n")
        fakegit.chmod(0o700)
        commands = [
            ["api", "repos/owner/repo/pulls/1"],
            ["api", "graphql", "--input", "-"],
            ["api", "repos/owner/repo/issues", "--method", "POST", "--input", "-"],
            ["pr", "checks", "1", "--watch"], ["pr", "view", "1"],
            ["auth", "token"], ["--version"], ["pr", "comment", "--help"],
            ["repo", "create", "unclassified"], ["unknown", "comment", "1"],
        ]
        for args in commands:
            with self.subTest(args=args):
                started = time.monotonic()
                result = self.run_helper(args, cwd=self.root, body=b'{"query":"mutation Unclassified{x}"}',
                                         extra={"PATH": str(fakebin)})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, b"err\n")
                self.assertLess(time.monotonic() - started, 1)
                self.assertEqual(self.calls()[-1]["argv"], args)
        self.assertFalse(marker.exists())
        self.assertEqual(cooldown.stat().st_mtime_ns, before)
        self.assertIsNone(writer.poll(), "a read disturbed the still-running writer")
        release.touch()
        writer.communicate(timeout=3)

    def test_busy_write_is_bounded_and_never_executes_real_gh(self):
        writer, release = self.held_writer()
        # Expire spacing without releasing the inode's lock. Admission must
        # still wait for actual gh, rather than only its former wrapper.
        record = next((self.repo / ".git/cagents-gh-content").glob("*.lock"))
        record.write_text(json.dumps({"boot": "an-expired-boot", "started": 0}))
        for wait in (0, 0.15):
            with self.subTest(wait=wait):
                started = time.monotonic()
                result = self.run_helper(["pr", "comment", "2", "-R", "owner/repo"], wait=wait)
                self.assert_deferred(result)
                self.assertEqual(result.stderr.count(b"waiting for content admission"), int(wait > 0))
                self.assertLess(time.monotonic() - started, wait + 0.7)
                self.assertEqual(len(self.calls()), 1)
        release.touch()
        writer.communicate(timeout=3)

    def test_nonblocking_environment_overrides_wait_without_notice_or_execution(self):
        writer, release = self.held_writer()
        started = time.monotonic()
        result = self.run_helper(["pr", "comment", "2", "-R", "owner/repo"], wait=30,
                                 extra={"CAGENTS_GH_NONBLOCKING": "1"})
        self.assert_deferred(result)
        self.assertTrue(result.stderr.startswith(b"cagents-gh: deferred "))
        self.assertNotIn(b"waiting for content admission", result.stderr)
        self.assertLess(time.monotonic() - started, 0.7)
        self.assertEqual(len(self.calls()), 1)
        self.assertIsNone(writer.poll())
        release.touch()
        writer.communicate(timeout=3)

    def test_started_write_has_no_wait_budget_timeout_and_other_repositories_do_not_wait(self):
        writer, release = self.held_writer()  # --wait 0 affects admission only.
        time.sleep(0.15)
        self.assertIsNone(writer.poll())
        started = time.monotonic()
        result = self.run_helper(["issue", "create", "-R", "owner/other", "--title", "fixture"], wait=0)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 1)
        self.assertEqual(len(self.calls()), 2)
        result = self.run_helper(["pr", "comment", "1", "-R", "ghe.example/owner/repo"], wait=0)
        self.assertEqual(result.returncode, 0, result.stderr)
        other_git = self.root / "another-project"
        other_git.mkdir()
        self.git("init", "-q", str(other_git))
        result = self.run_helper(["pr", "review", "1", "-R", "owner/repo", "--approve"],
                                 wait=0, cwd=other_git)
        self.assertEqual(result.returncode, 0, result.stderr)
        release.touch()
        writer.communicate(timeout=3)

    def test_worktrees_share_the_same_target_lock(self):
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "--allow-empty", "-qm", "fixture")
        (self.repo / ".git/info").mkdir(exist_ok=True)
        (self.repo / ".git/info/exclude").write_text(".worktrees/\n")
        worktree = self.repo / ".worktrees/linked"
        self.git("worktree", "add", "--detach", "-q", str(worktree), "HEAD")
        writer, release = self.held_writer()
        result = self.run_helper(["pr", "review", "1", "--repo=OWNER/REPO", "--comment", "-b", "fixture"],
                                 wait=0, cwd=worktree)
        self.assert_deferred(result)
        self.assertEqual(len(self.calls()), 1)
        release.touch()
        writer.communicate(timeout=3)

    def test_simultaneous_writes_are_serial_and_spaced_eight_seconds(self):
        first = self.start(["pr", "comment", "1", "-R", "owner/repo"], extra={"FIXTURE_HOLD": "0.3"})
        second = self.start(["issue", "comment", "2", "-R", "owner/repo"])
        outputs = [first.communicate(timeout=12), second.communicate(timeout=12)]
        notices = b"".join(output[1] for output in outputs)
        self.assertEqual(notices.count(b"waiting for content admission"), 1)
        self.assertIn(b"at most 30s total", notices)
        self.assertEqual(first.returncode, 0)
        self.assertEqual(second.returncode, 0)
        calls = sorted(self.calls(), key=lambda call: call["started"])
        self.assertEqual(len(calls), 2)
        self.assertGreaterEqual(calls[1]["started"] - calls[0]["started"], 7.9)

    def test_diagnostic_failure_does_not_change_admission(self):
        with patch.object(coordinate, "git_common_dir", return_value=self.repo / ".git"), \
                patch.object(coordinate, "WRITE_INTERVAL", 0.05):
            lease = coordinate.admit(("github.com", "owner", "repo"), 0)
            os.close(lease)
            lease = None
            try:
                with patch.object(coordinate.os, "write", side_effect=OSError("stderr unavailable")) as diagnostic:
                    started = time.monotonic()
                    lease = coordinate.admit(("github.com", "owner", "repo"), 1)
                    self.assertGreaterEqual(time.monotonic() - started, 0.04)
                    self.assertEqual(diagnostic.call_count, 1)
                    os.fstat(lease)
            finally:
                if lease is not None:
                    os.close(lease)

    def test_full_stderr_pipe_cannot_block_the_notice_or_change_inherited_flags(self):
        read_fd, write_fd = os.pipe()
        try:
            flags = fcntl.fcntl(write_fd, fcntl.F_GETFL)
            fcntl.fcntl(write_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)
            while True:
                try:
                    os.write(write_fd, b"x" * 4096)
                except BlockingIOError:
                    break
            fcntl.fcntl(write_fd, fcntl.F_SETFL, flags)
            program = (
                "import importlib.util,sys; "
                "s=importlib.util.spec_from_file_location('coordinate',sys.argv[1]); "
                "m=importlib.util.module_from_spec(s); s.loader.exec_module(m); "
                "m.waiting_notice(30); print('returned')"
            )
            result = subprocess.run(["/usr/bin/python3", "-I", "-B", "-c", program, str(SCRIPT)],
                                    stdout=subprocess.PIPE, stderr=write_fd, timeout=2)
            self.assertEqual((result.returncode, result.stdout), (0, b"returned\n"))
            self.assertEqual(fcntl.fcntl(write_fd, fcntl.F_GETFL), flags)
        finally:
            os.close(read_fd)
            os.close(write_fd)

    def test_stdio_exit_status_and_arguments_are_unchanged(self):
        args = ["pr", "review", "https://github.com/Owner/Repo/pull/42", "--repo", "owner/repo",
                "--comment", "--body-file", "-", "--body", "--help"]
        body, stdout, stderr = b"private fixture\x00\xff\n", b"output\x00\xff\n", b"error\x00\xfe\n"
        result = self.run_helper(args, body=body, extra={"FIXTURE_EXIT": "19", "FIXTURE_STDOUT": stdout.hex(),
                                                       "FIXTURE_STDERR": stderr.hex()})
        self.assertEqual((result.returncode, result.stdout, result.stderr), (19, stdout, stderr))
        self.assertEqual(self.calls()[0]["argv"], args)
        self.assertEqual(bytes.fromhex(self.calls()[0]["stdin"]), body)

    def test_malformed_or_mismatched_targets_are_refused_before_exec(self):
        commands = [
            ["pr", "comment", "1"],
            ["issue", "create", "-R", "not-a-repository"],
            ["pr", "create", "-R", "https://github.com/owner/repo"],
            ["pr", "comment", "https://github.com/other/repo/pull/1", "-R", "owner/repo"],
            ["pr", "comment", "https://wrong.example/owner/repo/pull/1", "-R", "owner/repo"],
            ["issue", "comment", "https://github.com/owner/repo/pull/1", "-R", "owner/repo"],
            ["pr", "comment", "https://github.com/owner/repo/pull/0", "-R", "owner/repo"],
            ["pr", "comment", "https://user:secret@github.com/owner/repo/pull/1", "-R", "owner/repo"],
            ["pr", "comment", "1", "-R", "owner/repo", "--repo", "other/repo"],
            ["pr", "comment", "1", "-R", "owner/repo", "--unsupported", "fixture"],
        ]
        for args in commands:
            with self.subTest(args=args):
                self.assert_deferred(self.run_helper(args, cwd=self.root, wait=0))
        self.assertEqual(self.calls(), [])
        self.assertFalse((self.repo / ".git/cagents-gh-content").exists())

    def test_body_urls_paths_and_option_like_content_do_not_change_the_target(self):
        commands = [
            ["pr", "comment", "1", "-Rowner/repo0", "--body", "https://wrong.example/other/repo/pull/2"],
            ["pr", "comment", "1", "--repo=owner/repo1", "--body-file", "/fixture/https://wrong.example/body"],
            ["pr", "comment", "1", "-R", "owner/repo2", "--body", "--repo=wrong.example/other/repo"],
            ["pr", "review", "1", "-R", "owner/repo3", "-r", "--body", "fixture"],
            ["pr", "-R", "owner/repo4", "comment", "1", "--body", "fixture"],
            ["issue", "comment", "https://ghe.example:8443/owner/repo5/issues/1",
             "-R", "ghe.example:8443/owner/repo5", "--body", "https://other.example"],
        ]
        for args in commands:
            with self.subTest(args=args):
                result = self.run_helper(args, wait=0)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.calls()[-1]["argv"], args)
        self.assertEqual(len(list((self.repo / ".git/cagents-gh-content").glob("*.lock"))), len(commands))

    def test_invalid_executable_and_wait_budget_are_preexec_deferrals(self):
        for real, wait in [("relative-gh", 0), (self.root / "missing", 0), (SCRIPT, 0),
                           (self.real, -1), (self.real, 30.1), (self.real, "nan"), (self.real, "inf")]:
            with self.subTest(real=real, wait=wait):
                self.assert_deferred(self.run_helper(["--version"], wait=wait, real=real))
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
