#!/usr/bin/python3
"""Offline bootstrap, concurrency and shared-wire coverage; never builds Rust."""
import errno
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
loader = importlib.machinery.SourceFileLoader("gh_pr_status", str(ROOT / "scripts/gh-pr-status"))
spec = importlib.util.spec_from_loader(loader.name, loader)
status = importlib.util.module_from_spec(spec)
loader.exec_module(status)
# Bounds only a hang; a passing wait returns as soon as its event arrives.
HANG_GUARD = 60


def wait_for(predicate):
    deadline = time.monotonic() + HANG_GUARD
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError("fixture did not complete")
        time.sleep(0.01)


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        for name in ["scripts", "src", "bin", "home", "config"]:
            (self.repo / name).mkdir()
        self.env = dict(os.environ, HOME=str(self.repo / "home"),
                        XDG_STATE_HOME=str(self.root / "state"),
                        GH_CONFIG_DIR=str(self.repo / "config"), GH_TOKEN="fixture-token",
                        PATH=str(self.repo / "bin") + ":/usr/bin:/bin",
                        # Fakes outlast the callers' guard, so a stuck hold fails the caller first.
                        FIXTURE_ROOT=str(self.root), FIXTURE_HOLD_LIMIT=str(2 * HANG_GUARD),
                        PYTHONDONTWRITEBYTECODE="1")
        for name in ["GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]:
            self.env.pop(name, None)
        self.patch = patch.dict(os.environ, self.env, clear=True)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.script = self.repo / "scripts/gh-pr-status"
        self.script.write_bytes((ROOT / "scripts/gh-pr-status").read_bytes())
        (self.script.parent / "cargo_artifacts.py").write_bytes((ROOT / "scripts/cargo_artifacts.py").read_bytes())
        self.script.chmod(0o755)
        self.install("bin/cagents", "#!/bin/sh\necho 'old installed cagents was invoked' >&2\nexit 77\n")
        self.install("bin/gh", '''#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
r = Path(os.environ['FIXTURE_ROOT'])
with (r/'requests').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')
if 'graphql' in sys.argv and (r/'graphql-response').exists():
    print('HTTP/2.0 200 OK\\nX-RateLimit-Remaining: 4999\\nX-RateLimit-Resource: graphql\\n')
    print((r/'graphql-response').read_text())
    sys.exit(0)
if (r/'hold-rest').exists():
    (r/'rest-started').touch()
    deadline=time.monotonic()+float(os.environ['FIXTURE_HOLD_LIMIT'])
    while not (r/'release-rest').exists():
        if time.monotonic()>deadline: sys.exit(9)
        time.sleep(.01)
if (r/'response').exists():
    sys.stdout.write((r/'response').read_text())
    sys.exit(1)
print('HTTP/2.0 200 OK\\nX-RateLimit-Remaining: 4999\\nX-RateLimit-Resource: core\\n')
print(json.dumps(dict(number=int(sys.argv[-1].split('/')[-1]),title='Fixture PR',
    html_url='https://github.com/o/r/pull/7',state='open',merged=False,draft=False,
    head=dict(ref='branch',sha=(r/'head').read_text() if (r/'head').exists() else 'a'*40),
    base=dict(ref='main'),mergeable=True,
    mergeable_state='clean',updated_at='2026-11-10T00:00:00Z')))
''')
        self.install("scripts/cargo-gate", '''#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
r = Path(os.environ['FIXTURE_ROOT'])
with (r/'builds').open('a') as f: f.write(json.dumps(dict(args=sys.argv[1:], source=Path('src/github_status.rs').read_text()))+'\\n')
(r/'build-started').touch()
deadline=time.monotonic()+float(os.environ['FIXTURE_HOLD_LIMIT'])
while (r/'hold-build').exists() and not (r/'release-build').exists():
    if time.monotonic()>deadline: sys.exit(9)
    time.sleep(.01)
if (r/'fail-build').exists(): sys.exit(8)
p=Path('target/custom/cagents').absolute()
p.parent.mkdir(parents=True,exist_ok=True)
p.write_text("#!/usr/bin/python3\\nimport os,sys\\nfrom pathlib import Path\\nif '--help' in sys.argv: sys.exit(0)\\nr=Path(os.environ['FIXTURE_ROOT'])\\nwith (r/'cli-calls').open('a') as f: f.write(Path(sys.argv[0]).parent.name+'\\\\n')\\no=r/'cli-output'\\nprint(o.read_text() if o.exists() else 'cached-new-cli')\\nsys.exit(int(os.environ.get('FIXTURE_CLI_EXIT','0')))\\n")
p.chmod(0o700)
print(json.dumps(dict(reason='compiler-artifact',target=dict(name='cagents',kind=['bin']),executable=str(p))))
''')
        (self.repo / "src/github_status.rs").write_text("reviewed main source")
        (self.repo / ".gitignore").write_text("target/\n.worktrees/\n")
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture baseline")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")

    def install(self, name, text):
        path = self.repo / name
        path.write_text(text)
        path.chmod(0o755)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], stderr=subprocess.PIPE)

    def invoke(self, number=7, env=None, args=()):
        return subprocess.run([str(self.script), str(number), "-R", "o/r", "--json", *args],
                              env=env or self.env, capture_output=True, text=True, timeout=HANG_GUARD)

    def lines(self, name):
        path = self.root / name
        return path.read_text().splitlines() if path.exists() else []

    @staticmethod
    def builder_idle(common):
        try:
            with status.locked(common / "gh-pr-status/build.lock"):
                return True
        except BlockingIOError:
            return False

    def finish_builder(self):
        self.root.joinpath("release-build").touch()
        common, revision, key = status.build_context(self.repo, "refs/remotes/origin/main")
        wait_for(lambda: (common / "gh-pr-status" / key / "source.json").exists())
        # Completion metadata is written inside the lock; wait for cleanup too.
        def done():
            try:
                with status.locked(common / "gh-pr-status/build.lock"):
                    return True
            except BlockingIOError:
                return False
        wait_for(done)

    def test_cold_callers_share_one_build_and_one_rest_then_use_new_cli(self):
        self.root.joinpath("hold-build").touch()
        self.root.joinpath("hold-rest").touch()
        children = [subprocess.Popen([str(self.script), "7", "-R", "o/r", "--json"],
                                    env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True) for _ in range(8)]
        try:
            wait_for(lambda: self.root.joinpath("rest-started").exists())
            wait_for(lambda: self.root.joinpath("build-started").exists())
            self.root.joinpath("release-rest").touch()
            results = [child.communicate(timeout=HANG_GUARD) for child in children]
            self.assertTrue(all(child.returncode in (0, 4) for child in children), results)
            self.assertEqual(len(self.lines("requests")), 1)
            self.assertEqual(len(self.lines("builds")), 1)
            good = [json.loads(out) for (out, err), child in zip(results, children)
                    if child.returncode == 0]
            self.assertTrue(good)
            snapshot = good[0]["data"]["snapshot"]
            self.assertEqual(snapshot["pull_request"]["headRefOid"], "a" * 40)
            self.assertIsNone(snapshot["checks_state"])
            self.assertIsNone(snapshot["pull_request"]["reviewDecision"])
            self.assertFalse(snapshot["checks_complete"])
            self.assertTrue(good[0]["limitations"])
            self.finish_builder()
            result = self.invoke(env=dict(self.env, FIXTURE_CLI_EXIT="4"))
            self.assertEqual(result.returncode, 4, result.stderr)
            self.assertEqual(result.stdout.strip(), "cached-new-cli")
            self.assertEqual(len(self.lines("requests")), 1, "CLI errors must not trigger fallback")
        finally:
            self.root.joinpath("release-rest").touch()
            self.root.joinpath("release-build").touch()
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.communicate()
            self.finish_builder()

    def test_fallback_expires_old_idle_cache_records_without_breaking_cooldowns(self):
        root = status.private(self.root / "state/cagents/github-status")
        now = int(time.time())
        old = now - status.CACHE_RETENTION - 1
        future = now + 60
        expired = {"1": ("json", {"next_attempt": 0}),
                   "2": ("json", {"due": 0}),
                   "3": ("rest-retry", {"retry_at": 0}),
                   "4": ("cooldown", {"primary": {"core": 1}, "secondary": 0}),
                   "5": ("cooldown", 1)}
        retained = {"6": ("json", {"next_attempt": future}),
                    "7": ("json", {"due": future}),
                    "8": ("rest-retry", {"retry_at": future}),
                    "9": ("cooldown", {"primary": {"graphql": future}, "secondary": 1}),
                    "a": ("cooldown", {"primary": {}, "secondary": future}),
                    "b": ("cooldown", future),
                    "c": ("json", {"next_attempt": 0}),
                    "d": ("json", {"unknown": 0})}

        def record(digit, suffix):
            return root / f"{digit * 64}.{suffix}"

        for digit, (suffix, data) in (expired | retained).items():
            path = record(digit, suffix)
            status.store(path, data)
            os.utime(path, (old, old))
        recent = record("0", "json")
        status.store(recent, {"next_attempt": 0})
        symlink = record("f", "json")
        symlink.symlink_to(recent)
        os.utime(symlink, (old, old), follow_symlinks=False)
        lock = record("1", "lock")
        with status.locked(lock):
            inode = lock.stat().st_ino
        with status.locked(record("c", "lock")):
            key = status.target("o/r", "github.com", 7)
            self.assertIsNone(status.rest(key)["error"])
        self.assertEqual(len(self.lines("requests")), 1)
        for digit, (suffix, _) in expired.items():
            self.assertFalse(record(digit, suffix).exists(), digit)
        for digit, (suffix, _) in retained.items():
            self.assertTrue(record(digit, suffix).exists(), digit)
        self.assertTrue(symlink.is_symlink())
        self.assertTrue(recent.exists())
        self.assertEqual(lock.stat().st_ino, inode)

    def test_main_snapshot_ignores_dirty_branch_and_unrelated_commits(self):
        self.git("checkout", "-qb", "agent")
        (self.repo / "src/github_status.rs").write_text("dirty agent source")
        self.assertEqual(self.invoke().returncode, 0)
        self.finish_builder()
        self.assertEqual(json.loads(self.lines("builds")[0])["source"], "reviewed main source")
        original = status.build_context(self.repo, "origin/main")[2]
        self.git("checkout", "--", "src/github_status.rs")
        (self.repo / "src/app.rs").write_text("unrelated app change")
        self.git("add", ".")
        self.git("commit", "-qm", "unrelated")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.assertEqual(status.build_context(self.repo, "origin/main")[2], original)
        self.assertEqual(self.invoke().stdout.strip(), "cached-new-cli")
        self.assertEqual(len(self.lines("builds")), 1)
        (self.repo / "src/github_status.rs").write_text("changed GitHub source")
        self.git("add", ".")
        self.git("commit", "-qm", "github change")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.assertNotEqual(status.build_context(self.repo, "origin/main")[2], original)
        self.invoke()
        self.finish_builder()
        self.assertEqual(len(self.lines("builds")), 2)
        self.assertEqual(len(self.lines("requests")), 1, "binary invalidation must not discard the fallback cache")
        before_dispatch = status.build_context(self.repo, "origin/main")[2]
        (self.repo / "src/control.rs").write_text("changed CLI dispatch")
        self.git("add", ".")
        self.git("commit", "-qm", "dispatch change")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.assertNotEqual(status.build_context(self.repo, "origin/main")[2], before_dispatch)

    def test_a_new_main_reads_ci_through_the_earlier_cli_until_its_own_is_built(self):
        self.invoke()
        self.finish_builder()
        common, _, first = status.build_context(self.repo, "origin/main")
        cache = common / "gh-pr-status"
        # A branch is qualified with --source-ref HEAD part way through, then
        # merged: its commits become ancestors of main, but its build is
        # still no stand-in for main.
        self.git("checkout", "-qb", "candidate")
        (self.repo / "src/github_status.rs").write_text("candidate source")
        self.git("commit", "-qam", "candidate")
        self.assertEqual(subprocess.run([str(self.script), "7", "-R", "o/r", "--json",
                                         "--source-ref", "HEAD"], env=self.env, capture_output=True,
                                        text=True, timeout=HANG_GUARD).returncode, 0)
        candidate = status.build_context(self.repo, "HEAD")[2]
        wait_for(lambda: (cache / candidate / "source.json").exists())
        (self.repo / "src/github_status.rs").write_text("reviewed GitHub source")
        self.git("commit", "-qam", "review fix")
        self.git("checkout", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "merge candidate", "candidate")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        second = status.build_context(self.repo, "origin/main")[2]
        self.assertNotIn(second, {first, candidate})
        wait_for(lambda: self.builder_idle(common))
        os.utime(cache / candidate / "cagents")  # the newest build is the candidate's
        # Main's new CLI takes a while to build. Reads meanwhile still go
        # through a CLI, so they keep CI and review state.
        self.root.joinpath("release-build").unlink(missing_ok=True)
        self.root.joinpath("hold-build").touch()
        for _ in range(3):
            result = self.invoke()
            self.assertEqual(result.stdout.strip(), "cached-new-cli", result.stderr)
        self.assertEqual(self.lines("cli-calls")[-3:], [first] * 3)
        wait_for(lambda: len(self.lines("builds")) == 3)
        self.assertEqual(len(self.lines("requests")), 1, "only the cold start used REST")
        self.finish_builder()
        self.assertEqual(self.invoke().stdout.strip(), "cached-new-cli")
        self.assertEqual(self.lines("cli-calls")[-1], second)
        self.assertEqual(len(self.lines("builds")), 3)
        # Main's own build removes the stand-in, but not the candidate's.
        self.assertFalse((cache / first).exists())
        self.assertTrue((cache / candidate / "cagents").exists())
        # A failing build leaves the last main CLI answering.
        self.root.joinpath("hold-build").unlink()
        self.root.joinpath("fail-build").touch()
        (self.repo / "src/github_status.rs").write_text("broken GitHub source")
        self.git("commit", "-qam", "broken")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.assertEqual(self.invoke().stdout.strip(), "cached-new-cli")
        wait_for(lambda: len(self.lines("builds")) == 4)
        wait_for(lambda: self.builder_idle(common))
        self.assertEqual(self.invoke().stdout.strip(), "cached-new-cli")
        self.assertEqual(self.lines("cli-calls")[-2:], [second] * 2)
        self.assertEqual(len(self.lines("requests")), 1)

    def test_failed_build_is_not_requeued_by_each_status_read(self):
        self.root.joinpath("fail-build").touch()
        self.assertEqual(self.invoke().returncode, 0)
        wait_for(lambda: bool(self.lines("builds")))
        common, _, key = status.build_context(self.repo, "origin/main")
        wait_for(lambda: (common / "gh-pr-status" / key / "build.log").exists()
                 and b"cargo-gate build failed" in (common / "gh-pr-status" / key / "build.log").read_bytes())
        for _ in range(3):
            self.assertEqual(self.invoke().returncode, 0)
        self.assertEqual(len(self.lines("builds")), 1)
        self.assertEqual(len(self.lines("requests")), 1)

    def test_retry_after_on_non_json_error_is_shared_across_prs(self):
        (self.root / "response").write_text("HTTP/2.0 429 Too Many Requests\nRetry-After: 600\nX-RateLimit-Resource: core\n\n<html>unavailable</html>")
        key = status.target("o/r", "github.com", 7)
        first = status.rest(key)
        self.assertGreaterEqual(first["next_attempt"], int(time.time()) + 599)
        second = status.rest(status.target("o/r", "github.com", 8))
        self.assertIn("cooldown", second["error"])
        self.assertEqual(len(self.lines("requests")), 1)

    def test_core_and_graphql_primary_budgets_are_separate_and_tokens_are_not_stored(self):
        root = status.private(Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status")
        budget = root / (status.digest("host-v2", status.credentials(), "github.com") + ".cooldown")
        status.store(budget, {"primary": {"graphql": int(time.time()) + 3600}, "secondary": 0})
        status.rest(status.target("o/r", "github.com", 7))
        self.assertEqual(len(self.lines("requests")), 1)
        with patch.dict(os.environ, GH_TOKEN="different-private-token"):
            status.rest(status.target("o/r", "github.com", 7))
        self.assertEqual(len(self.lines("requests")), 2)
        for path in root.iterdir():
            self.assertNotIn(b"private-token", path.read_bytes())
            self.assertNotIn(b"fixture-token", path.read_bytes())

    def test_primary_and_secondary_deadlines_remain_distinct(self):
        self.assertEqual(status.cooldown({"x-ratelimit-remaining": "0",
                         "x-ratelimit-resource": "core", "x-ratelimit-reset": "3600",
                         "retry-after": "20"}, {}, 100),
                         {"primary": {"core": 3600}, "secondary": 120})
        self.assertEqual(status.cooldown({}, {"message": "API rate limit already exceeded"}, 100),
                         {"primary": {"core": 160}, "secondary": 0})
        self.assertEqual(status.cooldown({"x-ratelimit-reset": "3600"},
                         {"message": "API rate limit already exceeded"}, 100),
                         {"primary": {"core": 3600}, "secondary": 0})
        self.assertEqual(status.cooldown({}, {"title": "secondary rate limit"}, 100),
                         {"primary": {}, "secondary": 0})

    def test_busy_host_budget_never_waits_or_sends_a_request(self):
        root = status.private(Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status")
        lock = root / (status.digest("host-v2", status.credentials(), "github.com") + ".lock")
        with status.locked(lock):
            started = time.monotonic()
            result = status.rest(status.target("o/r", "github.com", 7))
            self.assertLess(time.monotonic() - started, 1)
            self.assertIsNotNone(result["error"])
            self.assertFalse(self.lines("requests"))

    def test_post_request_budget_contention_retains_this_prs_retry_deadline(self):
        self.root.joinpath("hold-rest").touch()
        (self.root / "response").write_text('HTTP/2.0 429 Limited\nRetry-After: 600\n\n{"message":"secondary rate limit"}')
        key = status.target("o/r", "github.com", 7)
        result = []
        thread = threading.Thread(target=lambda: result.append(status.rest(key)))
        thread.start()
        try:
            wait_for(lambda: self.root.joinpath("rest-started").exists())
            root = Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status"
            lock = root / (status.digest("host-v2", status.credentials(), "github.com") + ".lock")
            with status.locked(lock):
                self.root.joinpath("release-rest").touch()
                thread.join(timeout=HANG_GUARD)
                self.assertFalse(thread.is_alive(), "a stopped budget owner must not hang the caller")
                self.assertIn("Host cooldown could not be saved", result[0]["error"])
                self.assertGreaterEqual(result[0]["next_attempt"], int(time.time()) + 599)
                self.assertEqual(status.rest(key)["next_attempt"], result[0]["next_attempt"])
                self.assertEqual(len(self.lines("requests")), 1)
        finally:
            self.root.joinpath("release-rest").touch()
            thread.join(timeout=HANG_GUARD)

    def test_rest_cannot_seed_the_cache_with_a_different_repository(self):
        result = status.rest(status.target("different/r", "github.com", 7))
        self.assertIn("different PR target URL", result["error"])
        self.assertIsNone(result.get("snapshot"))
        self.assertEqual(len(self.lines("requests")), 1)

    def test_an_unrunnable_cached_binary_falls_back_then_rebuilds(self):
        self.invoke()
        self.finish_builder()
        common, _, key = status.build_context(self.repo, "origin/main")
        binary = common / "gh-pr-status" / key / "cagents"
        binary.write_text("not an executable format")
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["data"]["source"], "rest-bootstrap")
        self.assertFalse(binary.exists())
        self.invoke()
        wait_for(lambda: len(self.lines("builds")) == 2)
        self.finish_builder()
        self.assertEqual(self.invoke().stdout.strip(), "cached-new-cli")
        self.assertEqual(len(self.lines("builds")), 2)
        self.assertEqual(len(self.lines("requests")), 1)

    def test_a_read_racing_a_push_is_stale_for_the_pushed_head_without_another_request(self):
        pushed = "b" * 40
        self.root.joinpath("hold-build").touch()  # Keep this on the REST fallback.
        self.assertEqual(self.invoke().returncode, 0)  # GitHub has not seen the push yet.
        self.root.joinpath("head").write_text(pushed)
        result = self.invoke(args=("--expect-head", pushed.upper()))
        self.assertEqual(result.returncode, status.HEAD_NOT_SHOWN, result.stderr)
        data = json.loads(result.stdout)["data"]
        self.assertEqual(data["snapshot"]["pull_request"]["headRefOid"], "a" * 40)
        self.assertTrue(data["stale"])
        self.assertEqual(data["expected_head"], pushed)
        self.assertIsNotNone(data["retry_at"])
        self.assertIn(f"shows head {'a' * 40}, not {pushed}", data["limitations"][-1])
        self.assertEqual(len(self.lines("requests")), 1, "naming a head must not refresh the cache")
        # The same snapshot still answers a caller that expects the head it shows.
        result = self.invoke(args=("--expect-head", "a" * 40))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["data"]["expected_head"], "a" * 40)
        self.finish_builder()

    def test_the_cached_cli_report_is_checked_against_the_pushed_head(self):
        self.invoke()
        self.finish_builder()
        report = {"ok": True, "command": "gh.pr_status", "data": {
            "stale": False, "error": None, "retry_at": int(time.time()) + 60, "limitations": [],
            "snapshot": {"pull_request": {"headRefOid": "a" * 40}, "checks_state": "SUCCESS"}}}
        self.root.joinpath("cli-output").write_text(json.dumps(report))
        result = self.invoke(args=("--expect-head", "b" * 40))
        self.assertEqual(result.returncode, status.HEAD_NOT_SHOWN, result.stderr)
        data = json.loads(result.stdout)["data"]
        self.assertTrue(data["stale"])
        self.assertIn("Read again after retry_at", data["limitations"][0])
        result = self.invoke(args=("--expect-head", "a" * 40))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(json.loads(result.stdout)["data"]["stale"])
        # A failed refresh keeps the CLI's own exit status.
        result = self.invoke(env=dict(self.env, FIXTURE_CLI_EXIT="4"), args=("--expect-head", "b" * 40))
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertTrue(json.loads(result.stdout)["data"]["stale"])
        # A CLI that prints no report confirms nothing, and keeps its binary.
        self.root.joinpath("cli-output").write_text("not a report")
        result = self.invoke(args=("--expect-head", "a" * 40))
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertIn("head is unchecked", result.stderr)
        common, _, key = status.build_context(self.repo, "origin/main")
        self.assertTrue((common / "gh-pr-status" / key / "cagents").exists())
        self.assertEqual(len(self.lines("requests")), 1, "only the cold start used REST")

    def test_a_failed_refresh_keeps_its_status_when_the_head_also_differs(self):
        key = status.target("o/r", "github.com", 7)
        saved = {"fetched_at": int(time.time()) - 300, "error": "GitHub REST request failed",
                 "next_attempt": int(time.time()) + 60,
                 "snapshot": {"pull_request": {"headRefOid": "a" * 40}, "checks_state": None,
                              "checks": [], "checks_complete": False, "primary_cost": None}}
        with patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(status.output(saved, key, True, "b" * 40), 4)
        data = json.loads(out.getvalue())["data"]
        self.assertTrue(data["stale"])
        self.assertEqual(data["expected_head"], "b" * 40)

    def test_a_report_with_no_head_cannot_confirm_the_pushed_head(self):
        self.invoke()
        self.finish_builder()
        for snapshot in (None, {"pull_request": {}}, {"pull_request": {"headRefOid": None}}):
            with self.subTest(snapshot=snapshot):
                self.root.joinpath("cli-output").write_text(json.dumps({
                    "ok": True, "command": "gh.pr_status",
                    "data": {"stale": False, "error": None, "retry_at": None, "snapshot": snapshot}}))
                result = self.invoke(args=("--expect-head", "a" * 40))
                self.assertEqual(result.returncode, 4, result.stderr)
                data = json.loads(result.stdout)["data"]
                self.assertTrue(data["stale"])
                self.assertIn("cannot confirm", data["limitations"][-1])
        self.root.joinpath("cli-output").write_text(json.dumps({"ok": True, "command": "gh.pr_status"}))
        self.assertEqual(self.invoke(args=("--expect-head", "a" * 40)).returncode, 4)

    def test_only_an_unusable_binary_is_discarded_when_the_cli_cannot_start(self):
        self.invoke()
        self.finish_builder()
        common, _, key = status.build_context(self.repo, "origin/main")
        binary = common / "gh-pr-status" / key / "cagents"
        launch = subprocess.run

        def run(command, *args, **kwargs):
            if command[0] == str(binary):
                raise OSError(errno.EAGAIN, "Resource temporarily unavailable")
            return launch(command, *args, **kwargs)

        argv = ["gh-pr-status", "7", "-R", "o/r", "--json", "--expect-head", "a" * 40]
        with patch.object(status, "cached_cli", return_value=binary), \
                patch.object(status.subprocess, "run", side_effect=run), \
                patch("sys.argv", argv), patch("sys.stdout", new_callable=io.StringIO) as out, \
                patch("sys.stderr", new_callable=io.StringIO):
            self.assertEqual(status.main(), 0)
        self.assertEqual(json.loads(out.getvalue())["data"]["source"], "rest-bootstrap")
        self.assertTrue(binary.exists(), "a host that cannot fork just now keeps a good CLI")
        # A binary that cannot be executed at all is discarded, as without the flag.
        binary.write_text("not an executable format")
        result = self.invoke(args=("--expect-head", "a" * 40))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cannot start", result.stderr)
        self.assertFalse(binary.exists())
        self.assertEqual(len(self.lines("requests")), 1, "both fallbacks reuse the shared REST read")

    def test_an_expected_head_needs_json_and_a_full_sha(self):
        for args in (["--expect-head", "a" * 40], ["--json", "--expect-head", "abc123"]):
            with self.subTest(args=args):
                result = subprocess.run([str(self.script), "7", "-R", "o/r", *args], env=self.env,
                                        capture_output=True, text=True, timeout=HANG_GUARD)
                self.assertEqual(result.returncode, 2, result.stderr)
        self.assertFalse(self.lines("requests"))
        self.assertFalse(self.lines("builds"))

    def test_invalid_target_is_rejected_before_any_build_or_request(self):
        for repository, hostname, number in [("o/.git", "github.com", 7),
                                             ("o/r", "user:secret@github.com", 7),
                                             ("o/r", "github.com", 0)]:
            with self.assertRaises(ValueError):
                status.target(repository, hostname, number)
        self.assertFalse(self.lines("requests"))
        self.assertFalse(self.lines("builds"))

    def test_a_cached_graphql_snapshot_is_reported_like_the_cli_reports_it(self):
        key = status.target("o/r", "github.com", 7)
        link = "https://github.com/o/r/actions/runs/1/job/2"
        times = {"startedAt": "2026-11-09T00:00:00Z", "completedAt": "2026-11-09T00:01:00Z"}
        checks = [{"__typename": "CheckRun", "name": "passed", "status": "COMPLETED", "conclusion": "SUCCESS",
                   "detailsUrl": link, **times},
                  {"__typename": "CheckRun", "name": "failed", "status": "COMPLETED", "conclusion": "FAILURE",
                   "detailsUrl": link, **times},
                  {"__typename": "StatusContext", "context": "deploy", "state": "SUCCESS", "targetUrl": link,
                   "description": "Deployed"}]
        saved = {"fetched_at": int(time.time()), "error": None, "next_attempt": 0,
                 "snapshot": {"pull_request": {"headRefOid": "a" * 40}, "checks_state": "FAILURE",
                              "checks": checks, "checks_complete": True, "primary_cost": 1}}
        with patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(status.output(saved, key, True), 0)
        report = json.loads(out.getvalue())
        self.assertEqual(report["data"]["snapshot"]["checks"], [
            {"__typename": "CheckRun", "name": "failed", "status": "COMPLETED", "conclusion": "FAILURE",
             "detailsUrl": link}])
        self.assertEqual(report["data"]["snapshot"]["checks_passed"], 2)
        self.assertEqual(report["limitations"], [])
        self.assertIn("startedAt", saved["snapshot"]["checks"][0], "the cache itself is left as it was")
        self.assertEqual(report["data"]["limitations"], [])

    def test_a_cached_cli_rest_read_reports_ci_but_not_the_review_decision(self):
        key = status.target("o/r", "github.com", 7)
        checks = [{"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"}]
        saved = {"fetched_at": int(time.time()), "error": None, "next_attempt": 0,
                 "snapshot": {"pull_request": {"headRefOid": "a" * 40, "reviewDecision": None},
                              "checks_state": "SUCCESS", "checks": checks, "checks_complete": True,
                              "primary_cost": None, "transport": "rest"}}
        with patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(status.output(saved, key, True), 0)
        report = json.loads(out.getvalue())
        # Top-level limitations tell land-pr CI is unknown; this CI is known.
        self.assertEqual(report["limitations"], [])
        self.assertEqual(report["data"]["source"], "cache")
        self.assertIn("review decision is unknown", report["data"]["limitations"][0])

    def test_the_fallback_and_land_pr_agree_on_which_checks_passed(self):
        land_loader = importlib.machinery.SourceFileLoader("land_pr", str(ROOT / "scripts/land-pr"))
        land_spec = importlib.util.spec_from_loader(land_loader.name, land_loader)
        land = importlib.util.module_from_spec(land_spec)
        land_loader.exec_module(land)
        runs = [{"__typename": "CheckRun", "status": status, "conclusion": conclusion}
                for status in ("COMPLETED", "IN_PROGRESS", "QUEUED")
                for conclusion in ("SUCCESS", "NEUTRAL", "SKIPPED", "FAILURE", "CANCELLED", "TIMED_OUT", None)]
        contexts = [{"__typename": "StatusContext", "state": state}
                    for state in ("SUCCESS", "FAILURE", "ERROR", "PENDING", "EXPECTED")]
        for check in runs + contexts:
            with self.subTest(check=check):
                self.assertEqual(status.check_passed(check), land.passed(check))
        self.assertEqual(sum(map(status.check_passed, runs + contexts)), 4)

    def test_an_account_switch_during_a_cache_hit_discards_the_private_snapshot(self):
        key = status.target("o/r", "github.com", 7)
        first = status.credentials()
        status.rest(key)
        with patch.object(status, "credentials", side_effect=[first, "changed-context"]):
            with self.assertRaisesRegex(ValueError, "credentials changed"):
                status.rest(key)
        self.assertEqual(len(self.lines("requests")), 1)

    def test_graphql_primary_pause_preserves_shared_interval_then_allows_rest(self):
        key = status.target("o/r", "github.com", 7)
        root = status.private(Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status")
        credential = status.credentials()
        name = status.digest("pr-status-v1", credential, "github.com", "o", "r", "7")
        path = root / (name + ".json")
        at = int(time.time())
        status.store(path, {"error": "GraphQL primary limit", "next_attempt": at + 3600})
        status.store(root / (status.digest("host-v2", credential, "github.com") + ".cooldown"),
                     {"primary": {"graphql": at + 3600}, "secondary": 0})
        self.assertIsNotNone(status.rest(key)["error"])
        self.assertFalse(self.lines("requests"), "a new GraphQL attempt still owns the shared 120s interval")
        os.utime(path, (at - 121, at - 121))
        self.assertIsNone(status.rest(key)["error"])
        self.assertEqual(len(self.lines("requests")), 1)

    def test_rest_primary_deadline_does_not_poison_the_graphql_attempt_cache(self):
        at = int(time.time())
        (self.root / "response").write_text('HTTP/2.0 403 Limited\nX-RateLimit-Remaining: 0\n'
            f'X-RateLimit-Reset: {at + 3600}\nX-RateLimit-Resource: core\n\n'
            '{"message":"API rate limit exceeded"}')
        key = status.target("o/r", "github.com", 7)
        report = status.rest(key)
        self.assertGreaterEqual(report["next_attempt"], at + 3600)
        name = status.digest("pr-status-v1", status.credentials(), "github.com", "o", "r", "7")
        root = Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status"
        path = root / (name + ".json")
        cached = status.load(path)
        self.assertLessEqual(cached["next_attempt"], at + 121,
                             "Rust GraphQL must inherit only the shared 120s interval")
        self.assertGreaterEqual(status.load(root / (name + ".rest-retry"))["retry_at"], at + 3600)
        # A later GraphQL cache publication does not erase REST's own deadline.
        status.store(path, {"error": None, "next_attempt": 0})
        self.assertIn("REST cooldown", status.rest(key)["error"])
        self.assertEqual(len(self.lines("requests")), 1)

    @unittest.skipUnless(os.environ.get("GH_PR_STATUS_TEST_CLI"), "optional real gated CLI interoperability")
    def test_real_cli_and_rest_bridge_share_snapshots_and_separate_resource_deadlines(self):
        cli = os.environ["GH_PR_STATUS_TEST_CLI"]
        key = status.target("o/r", "github.com", 7)
        saved = status.rest(key)
        name = status.digest("pr-status-v1", status.credentials(), "github.com", "o", "r", "7")
        path = Path(self.env["XDG_STATE_HOME"]) / "cagents/github-status" / (name + ".json")

        def real():
            return subprocess.run([cli, "gh", "pr-status", "7", "-R", "o/r", "--json"],
                                  env=self.env, capture_output=True, text=True, timeout=HANG_GUARD)

        result = real()
        self.assertEqual(result.returncode, 0, result.stderr)
        # Served from the cache, but labeled: the bridge's REST read has no CI.
        self.assertEqual(json.loads(result.stdout)["data"]["source"], "rest-bootstrap")
        self.assertEqual(len(self.lines("requests")), 1, "Rust must read the bridge's actual wire cache")
        pr = dict(saved["snapshot"]["pull_request"])
        pr["commits"] = {"nodes": [{"commit": {"oid": pr["headRefOid"], "statusCheckRollup": {
            "state": "SUCCESS", "contexts": {"nodes": [], "pageInfo": {"hasNextPage": False}}}}}]}
        (self.root / "graphql-response").write_text(json.dumps({"data": {
            "rateLimit": {"cost": 1}, "repository": {"pullRequest": pr}}}))
        status.store(path, {**status.load(path), "next_attempt": 0})
        result = real()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(json.loads(result.stdout)["data"]["source"], "network")
        self.assertEqual(status.rest(key)["snapshot"]["checks_state"], "SUCCESS")
        self.assertEqual(len(self.lines("requests")), 2, "bridge must reuse Rust's fresh GraphQL cache")

        # Core exhaustion defers REST, without poisoning Rust's GraphQL budget.
        at = int(time.time())
        (self.root / "response").write_text('HTTP/2.0 403 Limited\nX-RateLimit-Remaining: 0\n'
            f'X-RateLimit-Reset: {at + 3600}\nX-RateLimit-Resource: core\n\n'
            '{"message":"API rate limit exceeded"}')
        status.store(path, {**status.load(path), "next_attempt": 0})
        self.assertGreaterEqual(status.rest(key)["next_attempt"], at + 3600)
        self.assertEqual(len(self.lines("requests")), 3)
        # Expire only the ordinary shared interval in this private fixture.
        status.store(path, {**status.load(path), "next_attempt": 0})
        result = real()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(json.loads(result.stdout)["data"]["source"], "network")
        self.assertEqual(len(self.lines("requests")), 4)
        self.assertIsNone(status.rest(key)["error"], "a fresh full snapshot is usable during REST pacing")
        status.store(path, {**status.load(path), "next_attempt": 0})
        self.assertIn("REST cooldown", status.rest(key)["error"])
        self.assertEqual(len(self.lines("requests")), 4, "Rust publication must not erase REST pacing")


if __name__ == "__main__":
    unittest.main()
