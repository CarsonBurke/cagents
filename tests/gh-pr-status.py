#!/usr/bin/python3
"""Offline bootstrap, concurrency and shared-wire coverage; never builds Rust."""
import importlib.machinery
import importlib.util
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


def wait_for(predicate):
    deadline = time.monotonic() + 5
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
                        FIXTURE_ROOT=str(self.root), PYTHONDONTWRITEBYTECODE="1")
        for name in ["GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]:
            self.env.pop(name, None)
        self.patch = patch.dict(os.environ, self.env, clear=True)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.script = self.repo / "scripts/gh-pr-status"
        self.script.write_bytes((ROOT / "scripts/gh-pr-status").read_bytes())
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
    deadline=time.monotonic()+5
    while not (r/'release-rest').exists():
        if time.monotonic()>deadline: sys.exit(9)
        time.sleep(.01)
if (r/'response').exists():
    sys.stdout.write((r/'response').read_text())
    sys.exit(1)
print('HTTP/2.0 200 OK\\nX-RateLimit-Remaining: 4999\\nX-RateLimit-Resource: core\\n')
print(json.dumps(dict(number=int(sys.argv[-1].split('/')[-1]),title='Fixture PR',
    html_url='https://github.com/o/r/pull/7',state='open',merged=False,draft=False,
    head=dict(ref='branch',sha='a'*40),base=dict(ref='main'),mergeable=True,
    mergeable_state='clean',updated_at='2026-11-10T00:00:00Z')))
''')
        self.install("scripts/cargo-gate", '''#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
r = Path(os.environ['FIXTURE_ROOT'])
with (r/'builds').open('a') as f: f.write(json.dumps(dict(args=sys.argv[1:], source=Path('src/github_status.rs').read_text()))+'\\n')
(r/'build-started').touch()
deadline=time.monotonic()+5
while (r/'hold-build').exists() and not (r/'release-build').exists():
    if time.monotonic()>deadline: sys.exit(9)
    time.sleep(.01)
if (r/'fail-build').exists(): sys.exit(8)
p=Path('target/custom/cagents').absolute()
p.parent.mkdir(parents=True,exist_ok=True)
p.write_text("#!/usr/bin/python3\\nimport os,sys\\nfrom pathlib import Path\\nif '--help' in sys.argv: sys.exit(0)\\nr=Path(os.environ['FIXTURE_ROOT'])\\nwith (r/'cli-calls').open('a') as f: f.write('called\\\\n')\\nprint('cached-new-cli')\\nsys.exit(int(os.environ.get('FIXTURE_CLI_EXIT','0')))\\n")
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

    def invoke(self, number=7, env=None):
        return subprocess.run([str(self.script), str(number), "-R", "o/r", "--json"],
                              env=env or self.env, capture_output=True, text=True, timeout=5)

    def lines(self, name):
        path = self.root / name
        return path.read_text().splitlines() if path.exists() else []

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
            results = [child.communicate(timeout=5) for child in children]
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
                thread.join(timeout=1)
                self.assertFalse(thread.is_alive(), "a stopped budget owner must not hang the caller")
                self.assertIn("Host cooldown could not be saved", result[0]["error"])
                self.assertGreaterEqual(result[0]["next_attempt"], int(time.time()) + 599)
                self.assertEqual(status.rest(key)["next_attempt"], result[0]["next_attempt"])
                self.assertEqual(len(self.lines("requests")), 1)
        finally:
            self.root.joinpath("release-rest").touch()
            thread.join(timeout=5)

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

    def test_invalid_target_is_rejected_before_any_build_or_request(self):
        for repository, hostname, number in [("o/.git", "github.com", 7),
                                             ("o/r", "user:secret@github.com", 7),
                                             ("o/r", "github.com", 0)]:
            with self.assertRaises(ValueError):
                status.target(repository, hostname, number)
        self.assertFalse(self.lines("requests"))
        self.assertFalse(self.lines("builds"))

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
                                  env=self.env, capture_output=True, text=True, timeout=5)

        result = real()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["data"]["source"], "cache")
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
