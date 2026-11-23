#!/usr/bin/python3
"""Exercise the watcher against real disposable Git history, with no Rust builds or API writes."""
import fcntl
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "scripts/main-build-sentinel"
loader = importlib.machinery.SourceFileLoader("sentinel", str(SOURCE))
spec = importlib.util.spec_from_loader(loader.name, loader)
sentinel = importlib.util.module_from_spec(spec)
loader.exec_module(sentinel)


class SentinelTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sentinel-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.origin = self.root / "origin"
        self.repo = self.root / "repository"
        self.state = self.root / "state"
        self.calls = self.root / "calls"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.command("git", "init", "-q", "-b", "main", str(self.origin))
        self.command("git", "-C", str(self.origin), "config", "user.email", "fixture@example.invalid")
        self.command("git", "-C", str(self.origin), "config", "user.name", "Fixture")
        scripts = self.origin / "scripts"
        scripts.mkdir()
        gate = scripts / "cargo-gate"
        gate.write_text('''#!/usr/bin/python3
import json, os, subprocess, sys
from pathlib import Path
if sys.argv[1] == 'test':
    assert sys.argv[2:5] == ['--locked', '--all-targets', '--no-fail-fast'], sys.argv
    selection = sys.argv[5:]
    lane = next(name for name, value in json.loads(os.environ['SENTINEL_TEST_LANES']).items()
                if value == selection)
    sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    with open(os.environ['SENTINEL_TESTS'], 'a') as out:
        out.write(json.dumps([sha, lane, os.environ.get('CAGENTS_BUILD_REMOTE'),
                              os.environ.get('RUST_TEST_THREADS'),
                              os.environ.get('CAGENTS_TEST_FIXTURE')]) + '\\n')
    if Path('litter-' + lane).exists():
        Path('left-by-a-test').write_text('')
    if Path('refused-' + lane).exists():
        print('build-disk-guard: not enough space')
        sys.exit(2)
    print('     Running unittests src/lib.rs')
    print('running 400 tests')
    for index in range(400):
        print(f'test passing_number_{index} ... ok')
    if Path('failing-' + lane).exists():
        print('test lane_breaks ... FAILED')
        print('')
        print('failures:')
        print('')
        print('---- lane_breaks stdout ----')
        print('')
        print("thread 'lane_breaks' panicked at src/lib.rs:1:1:")
        print('the reason it broke')
        print('')
        print('')
        print('failures:')
        print('    lane_breaks')
        print('')
        print('test result: FAILED. 400 passed; 1 failed')
        print('error: test failed, to rerun pass `--lib`')
        print('     Running tests/later.rs')
        for index in range(400):
            print(f'test later_number_{index} ... ok')
        print('error: 1 target failed:')
        print('    `--lib`')
        sys.exit(101)
    sys.exit(0)
assert sys.argv[-1] == '--message-format=json', sys.argv
selection = sys.argv[sys.argv.index('--all-targets') + 1:-1]
lane = next(name for name, value in json.loads(os.environ['SENTINEL_LANES']).items()
            if value == selection)
sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
with open(os.environ['SENTINEL_CALLS'], 'a') as out:
    out.write(json.dumps([sha, lane, sys.argv[1:], os.environ.get('CAGENTS_BUILD_REMOTE')]) + '\\n')
def diagnostic(package_id, level, text):
    print(json.dumps({'reason': 'compiler-message', 'package_id': package_id,
                      'message': {'level': level, 'message': text,
                                  'rendered': f'{level}: {text}\\n'}}))
print('Checking cagents', file=sys.stderr)
# A line on standard output that is not one of Cargo's messages is logged as is.
print(json.dumps({'note': 'not from Cargo'}))
print(json.dumps({'reason': 'compiler-artifact', 'target': {'name': 'cagents'}}))
# Vendored path dependencies print warnings in every lane; they never count.
diagnostic('path+file:///src/vendor/libcosmic#1.0.0', 'warning', 'vendored unused import')
diagnostic('path+file:///src/vendor/iced-core#iced_core@0.14.0', 'warning', 'vendored dead code')
if Path('warning-' + lane).exists():
    diagnostic('path+file:///src/crates/cagents-ui#0.1.3', 'warning', 'function `lane_only` is never used')
    # Once for the library and once for its tests, as Cargo reports it.
    diagnostic('path+file:///src#cagents@0.1.3', 'warning', 'unused import: `Lane`')
    diagnostic('path+file:///src#cagents@0.1.3', 'warning', 'unused import: `Lane`')
if Path('broken-' + lane).exists():
    diagnostic('path+file:///src#cagents@0.1.3', 'error', 'deliberately incompatible merge')
    sys.exit(101)
''')
        gate.chmod(0o755)
        (self.origin / "Cargo.toml").write_text(
            '[workspace]\nmembers = [".", "crates/*"]\n\n[package]\nname = "cagents"\n')
        application = self.origin / "crates/cagents-ui/Cargo.toml"
        application.parent.mkdir(parents=True)
        application.write_text('[package]\nname = "cagents-ui"\n')
        kit = self.origin / "crates/cagents-ui-kit/Cargo.toml"
        kit.parent.mkdir(parents=True)
        kit.write_text('[package]\nname = "cagents-ui-kit"\n')
        dictation = self.origin / "crates/cagents-dictate/Cargo.toml"
        dictation.parent.mkdir(parents=True)
        dictation.write_text('[package]\nname = "cagents-dictate"\n')
        editor = self.origin / "crates/ceditor/Cargo.toml"
        editor.parent.mkdir(parents=True)
        editor.write_text('[package]\nname = "ceditor"\n')
        self.first = self.commit("baseline")
        self.command("git", "clone", "-q", str(self.origin), str(self.repo))
        gh = self.bin / "gh"
        gh.write_text('''#!/usr/bin/python3
import json, os, sys
with open(os.environ['SENTINEL_CALLS'], 'a') as out:
    out.write(json.dumps(sys.argv[1:]) + '\\n')
if os.environ.get('SENTINEL_RATE_LIMIT'):
    if sys.argv[1:3] == ['api', 'rate_limit']:
        print(json.dumps({'resources': {'core': {'remaining': 0, 'reset': 9999999999}}}))
        sys.exit(0)
    print('API rate limit exceeded', file=sys.stderr)
    sys.exit(1)
if sys.argv[1:3] == ['issue', 'list']:
    print('[]')
if sys.argv[1:3] == ['issue', 'create']:
    print('https://github.com/fixture/repository/issues/42')
''')
        gh.chmod(0o755)
        # Records each start of the prebuild service with how many lanes were
        # tested by then; the host's own manager is never asked.
        systemctl = self.bin / "systemctl"
        systemctl.write_text('''#!/usr/bin/python3
import json, os, sys
from pathlib import Path
tests = Path(os.environ['SENTINEL_TESTS'])
with open(os.environ['SENTINEL_SYSTEMCTL'], 'a') as out:
    out.write(json.dumps([sys.argv[1:], len(tests.read_text().splitlines()) if tests.exists() else 0]) + '\\n')
''')
        systemctl.chmod(0o755)
        self.environment = patch.dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                                      SENTINEL_SYSTEMCTL=str(self.root / "systemctl"),
                                      SENTINEL_CALLS=str(self.calls),
                                      SENTINEL_LANES=json.dumps(sentinel.LANES),
                                      SENTINEL_TESTS=str(self.root / "tests"),
                                      SENTINEL_TEST_LANES=json.dumps(sentinel.TEST_LANES))
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.now = 2000

    def command(self, *args):
        return subprocess.check_output(args, text=True, stderr=subprocess.PIPE).strip()

    def commit(self, message, broken=None, warning=None):
        if broken:
            (self.origin / f"broken-{broken}").touch()
        if warning:
            (self.origin / f"warning-{warning}").touch()
        self.command("git", "-C", str(self.origin), "add", ".")
        self.command("git", "-C", str(self.origin), "commit", "-q", "--allow-empty", "-m", message)
        return self.command("git", "-C", str(self.origin), "rev-parse", "HEAD")

    def tick(self):
        with patch.object(sentinel.time, "time", return_value=self.now):
            sentinel.tick(self.repo, self.state, "fixture/repository")

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_every_lane_of_the_newest_commit_without_noop_rebuilds(self):
        self.tick()
        self.commit("merge one")
        third = self.commit("merge two")
        self.now += 120
        self.tick()
        self.now += 120
        self.tick()
        records = self.recorded()
        # A passing tip vouches for the commits before it.
        self.assertEqual([(entry[0], entry[1]) for entry in records],
                         [(sha, lane) for sha in (self.first, third) for lane in sentinel.LANES])
        for entry in records:
            self.assertEqual(entry[2][:4], ['clippy', '--locked', '--no-default-features', '--all-targets'])
            self.assertEqual(entry[2][-1], '--message-format=json')
            self.assertEqual(entry[3], 'fw16')

    def test_commit_before_the_application_package_skips_only_its_lanes(self):
        self.tick()
        self.command("git", "-C", str(self.origin), "rm", "-q", "crates/cagents-ui/Cargo.toml")
        earlier = self.commit("before the application package")
        self.now += 120
        self.tick()
        self.assertEqual([entry[1] for entry in self.recorded() if entry[0] == earlier],
                         ["cli", "host", "ui", "dictate", "editor"])

    def test_commit_before_the_dictation_helper_skips_only_its_lane(self):
        self.tick()
        self.command("git", "-C", str(self.origin), "rm", "-q", "crates/cagents-dictate/Cargo.toml")
        earlier = self.commit("before the dictation helper")
        self.now += 120
        self.tick()
        self.assertEqual([entry[1] for entry in self.recorded() if entry[0] == earlier],
                         ["cli", "host", "ui", "app", "browser", "editor"])

    def test_failure_is_reported_and_closed_issue_reopened_then_recovery_reported(self):
        self.tick()
        broken = self.commit("incompatible merge", broken="host")
        self.now += 120
        self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["checked"], broken)
        self.assertEqual(state["last_good"], self.first)
        self.assertEqual(state["issue"], 42)
        body = (self.state / "issue.md").read_text()
        self.assertIn(self.first, body)
        self.assertIn(broken, body)
        self.assertIn("host: exit 101", body)
        self.assertIn("deliberately incompatible merge", body)
        self.commit("another broken merge")
        self.now += 120
        self.tick()
        self.assertEqual(sum(entry[:2] == ["issue", "create"] for entry in self.recorded()), 1)
        self.assertTrue(any(entry[:3] == ["issue", "reopen", "42"] for entry in self.recorded()))
        (self.origin / "broken-host").unlink()
        fixed = self.commit("fix")
        self.now += 120
        self.tick()
        self.assertTrue(any(entry[:3] == ["issue", "comment", "42"] and fixed in str(entry)
                            for entry in self.recorded()))

    def test_a_failing_tip_is_bisected_to_the_first_failing_commit_in_its_failing_lanes(self):
        self.tick()
        commits = [self.commit("merge one"), self.commit("merge two"),
                   self.commit("incompatible merge", warning="ui"), self.commit("merge four"),
                   self.commit("merge five"), self.commit("merge six")]
        self.now += 120
        self.tick()
        records = self.recorded()
        checks = [(entry[0], entry[1]) for entry in records if len(entry) == 4]
        # The tip in every lane, then only the lane it fails, halving the range.
        self.assertEqual(checks[len(sentinel.LANES):2 * len(sentinel.LANES)],
                         [(commits[-1], lane) for lane in sentinel.LANES])
        bisected = checks[2 * len(sentinel.LANES):]
        self.assertEqual({lane for _, lane in bisected}, {"ui"})
        self.assertLessEqual(len(bisected), 3)
        comments = [entry for entry in records if entry[:3] == ["issue", "comment", "42"]]
        self.assertEqual(len(comments), 1)
        self.assertIn(f"after `{self.first}`", comments[0][4])
        self.assertIn(f"- `{commits[2]}` (incompatible merge): ui", comments[0][4])
        state = json.loads((self.state / "state.json").read_text())
        self.assertNotIn("bisect", state)
        self.assertEqual(state["checked"], commits[-1])
        self.assertEqual(state["last_good"], self.first)

    def test_lanes_that_broke_at_different_commits_are_each_bisected_to_their_own(self):
        self.tick()
        commits = [self.commit("merge one"), self.commit("host break", broken="host"),
                   self.commit("merge three"), self.commit("merge four"),
                   self.commit("cli warning", warning="cli"), self.commit("merge six")]
        self.now += 120
        self.tick()
        checks = [(entry[0], entry[1]) for entry in self.recorded() if len(entry) == 4]
        self.assertEqual({lane for _, lane in checks[2 * len(sentinel.LANES):]}, {"cli", "host"})
        [comment] = [entry for entry in self.recorded() if entry[:3] == ["issue", "comment", "42"]]
        self.assertIn(f"- `{commits[1]}` (host break): host", comment[4])
        self.assertIn(f"- `{commits[4]}` (cli warning): cli", comment[4])

    def test_a_tip_failing_only_on_warnings_is_built_before_the_bisection_moves_the_checkout(self):
        self.with_evidence_tool()
        self.tick()
        self.commit("merge one")
        warned = self.commit("lane-only helper", warning="host")
        self.commit("merge three")
        tip = self.commit("merge four")
        self.now += 120
        self.tick()
        self.assertTrue(any(entry[:3] == ["issue", "comment", "42"] for entry in self.recorded()))
        self.assertTrue((self.state / "builds" / tip).is_dir())
        self.assertFalse((self.state / "builds" / warned).exists())

    def test_a_tip_failing_since_an_earlier_failure_is_not_bisected(self):
        self.tick()
        self.commit("incompatible merge", broken="host")
        self.now += 120
        self.tick()
        before = len(self.recorded())
        self.commit("merge one")
        tip = self.commit("merge two")
        self.now += 120
        self.tick()
        checks = [entry[:2] for entry in self.recorded()[before:] if len(entry) == 4]
        self.assertEqual(checks, [[tip, lane] for lane in sentinel.LANES])
        self.assertFalse(any(entry[:2] == ["issue", "comment"] for entry in self.recorded()))

    def test_an_interrupted_bisection_resumes_without_checking_again_what_it_checked(self):
        self.tick()
        commits = [self.commit("merge one", broken="cli"), *(self.commit(f"merge {n}") for n in range(2, 9))]
        checked = []
        real = sentinel.check_commit

        def interrupted(repository, directory, sha, lanes):
            # The tip, one bisection step, then an interruption in the second.
            if len(checked) == 2:
                checked.append("interrupted")
                raise KeyboardInterrupt
            checked.append(sha)
            return real(repository, directory, sha, lanes)
        self.now += 120
        with patch.object(sentinel, "check_commit", interrupted), self.assertRaises(KeyboardInterrupt):
            self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["bisect"]["commits"], commits)
        fetched = state["last_fetch"]
        self.now += 120
        with patch.object(sentinel, "check_commit", lambda *call: checked.append(call[2]) or real(*call)):
            self.tick()
        self.assertEqual(checked[:3], [commits[-1], commits[3], "interrupted"])
        # The first step's commit is not checked again, and nothing is fetched.
        self.assertNotIn(commits[3], checked[3:])
        self.assertEqual(json.loads((self.state / "state.json").read_text())["last_fetch"], fetched)
        comments = [entry for entry in self.recorded() if entry[:3] == ["issue", "comment", "42"]]
        self.assertIn(f"- `{commits[0]}` (merge one): cli", comments[0][4])

    def test_a_queue_of_every_commit_an_earlier_sentinel_saved_checks_only_its_newest(self):
        self.tick()
        commits = [self.commit("merge one"), self.commit("merge two"), self.commit("merge three")]
        self.command("git", "-C", str(self.repo), "fetch", "-q", "origin", "main")
        path = self.state / "state.json"
        state = json.loads(path.read_text())
        # A fetch rebuilds the queue; one behind a result still to report is kept.
        passed = {lane: 0 for lane in sentinel.LANES}
        path.write_text(json.dumps(state | {"queue": commits, "pending": {
            "sha": commits[0], "lanes": passed, "warnings": {}}}))
        before = len(self.recorded())
        self.now += 1
        self.tick()
        checks = [entry[:2] for entry in self.recorded()[before:] if len(entry) == 4]
        self.assertEqual(checks, [[commits[-1], lane] for lane in sentinel.LANES])
        self.assertEqual(json.loads(path.read_text())["checked"], commits[-1])

    def test_workspace_warnings_fail_the_check_but_vendored_ones_do_not(self):
        self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["last_good"], self.first)
        self.assertFalse(any(entry[:2] == ["issue", "create"] for entry in self.recorded()))
        log = (self.state / "logs" / f"{self.first}-ui.log").read_text()
        self.assertIn("warning: vendored unused import", log)
        self.assertIn("Checking cagents", log)
        self.assertIn('{"note": "not from Cargo"}\n', log)
        self.assertNotIn("compiler-artifact", log)
        warned = self.commit("lane-only helper", warning="app")
        self.now += 120
        self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["checked"], warned)
        self.assertEqual(state["last_good"], self.first)
        self.assertEqual(state["compiled"], warned)
        self.assertEqual(state["issue"], 42)
        body = (self.state / "issue.md").read_text()
        self.assertIn("- app: exit 0, 2 warnings\n", body)
        self.assertIn("- cli: exit 0\n", body)
        self.assertIn("function `lane_only` is never used", body)
        self.assertNotIn("vendored", body)
        self.assertNotIn("cli diagnostics", body)
        (self.origin / "warning-app").unlink()
        fixed = self.commit("gate the helper")
        self.now += 120
        self.tick()
        self.assertTrue(any(entry[:3] == ["issue", "comment", "42"] and fixed in str(entry)
                            for entry in self.recorded()))
        self.assertEqual(json.loads((self.state / "state.json").read_text())["last_good"], fixed)

    def test_an_unreadable_manifest_leaves_the_lanes_to_cargo(self):
        self.tick()
        (self.origin / "Cargo.toml").write_text("[workspace\n")
        broken = self.commit("malformed manifest", broken="cli", warning="host")
        self.now += 120
        self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["checked"], broken)
        body = (self.state / "issue.md").read_text()
        self.assertIn("- cli: exit 101\n", body)
        self.assertIn("- host: exit 0\n", body)
        self.assertIn("deliberately incompatible merge", body)

    def test_rate_limit_waits_for_reset_and_retries_report_without_rebuilding(self):
        self.commit("broken", broken="ui")
        with patch.dict(os.environ, SENTINEL_RATE_LIMIT="1"):
            with self.assertRaisesRegex(RuntimeError, "rate limit"):
                self.tick()
        before = self.recorded()
        self.now += 120
        self.tick()
        self.assertEqual(self.recorded(), before)
        self.now = 10000000000
        self.tick()
        builds = [entry for entry in self.recorded() if len(entry) == 4 and entry[1] in sentinel.LANES]
        self.assertEqual(len(builds), len(sentinel.LANES))
        self.assertNotIn("pending", json.loads((self.state / "state.json").read_text()))

    def test_notification_error_waits_before_retrying(self):
        self.commit("broken", broken="cli")
        with patch.object(sentinel, "report", side_effect=RuntimeError("network unavailable")):
            with self.assertRaisesRegex(RuntimeError, "network"):
                self.tick()
        before = self.recorded()
        self.tick()
        self.assertEqual(self.recorded(), before)
        self.now += 120
        self.tick()
        self.assertEqual(sum(entry[1] in sentinel.LANES for entry in self.recorded()), len(sentinel.LANES))

    def test_distinct_state_directories_cannot_rewrite_shared_checkout(self):
        worktrees = self.repo / ".worktrees"
        worktrees.mkdir()
        with (worktrees / ".main-build-sentinel.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.tick()
            self.assertFalse(self.state.exists())
            self.state = self.root / "another-state"
            self.tick()
            self.assertFalse(self.state.exists())
        self.tick()
        self.assertEqual(len(self.recorded()), len(sentinel.LANES))

    def test_rapid_invocations_do_not_fetch_or_build(self):
        self.tick()
        self.commit("next")
        with patch.object(sentinel, "git", side_effect=AssertionError("unexpected Git request")):
            self.tick()
        self.assertEqual(len(self.recorded()), len(sentinel.LANES))


    def tested(self):
        path = self.root / "tests"
        return [json.loads(line)[:2] for line in path.read_text().splitlines()] if path.exists() else []

    def test_the_newest_compiling_commit_is_tested_once_in_every_lane_and_its_verdict_kept(self):
        self.tick()
        self.commit("merge one")
        tip = self.commit("merge two")
        self.now += 120
        self.tick()
        self.now += 120
        self.tick()
        self.assertEqual(self.tested(), [[sha, lane] for sha in (self.first, tip) for lane in sentinel.TEST_LANES])
        verdict = json.loads((self.state / "verdicts" / f"{tip}.json").read_text())
        self.assertTrue(verdict["passed"])
        self.assertEqual(list(verdict["lanes"]), list(sentinel.TEST_LANES))
        self.assertEqual(json.loads((self.state / "state.json").read_text())["tested"], tip)
        self.assertFalse(any(entry[:2] == ["issue", "create"] for entry in self.recorded()))

    def test_the_prebuild_starts_before_the_tests_without_waiting_and_on_every_tick(self):
        self.tick()
        self.now += 1
        self.tick()
        calls = [json.loads(line) for line in (self.root / "systemctl").read_text().splitlines()]
        start = ["--user", "start", "--no-block", sentinel.PREBUILD_SERVICE]
        # Before the first lane's tests, after them, and on a tick with nothing to do.
        self.assertEqual(calls, [[start, 0], [start, len(self.tested())], [start, len(self.tested())]])

    def test_a_host_without_the_prebuild_service_still_tests(self):
        (self.bin / "systemctl").write_text("#!/bin/sh\nexit 5\n")
        self.tick()
        self.assertTrue(json.loads((self.state / "verdicts" / f"{self.first}.json").read_text())["passed"])

    def test_a_commit_that_does_not_compile_is_not_tested(self):
        self.tick()
        broken = self.commit("incompatible merge", broken="host")
        self.now += 120
        self.tick()
        self.assertNotIn(broken, [sha for sha, _ in self.tested()])
        self.assertFalse((self.state / "verdicts" / f"{broken}.json").exists())

    def test_failing_tests_are_reported_and_recovery_comments(self):
        self.tick()
        (self.origin / "failing-host").touch()
        failing = self.commit("break a test")
        self.now += 120
        self.tick()
        verdict = json.loads((self.state / "verdicts" / f"{failing}.json").read_text())
        self.assertFalse(verdict["passed"])
        self.assertEqual(verdict["lanes"]["host"]["status"], 101)
        self.assertEqual(verdict["lanes"]["cli"]["status"], 0)
        [create] = [entry for entry in self.recorded() if entry[:2] == ["issue", "create"]]
        self.assertEqual(create[3], sentinel.TEST_TITLE)
        body = (self.state / "test-issue.md").read_text()
        self.assertIn(failing, body)
        self.assertIn("- host: exit 101", body)
        self.assertIn("test lane_breaks ... FAILED", body)
        self.assertEqual(json.loads((self.state / "state.json").read_text())["tests_last_good"], self.first)
        (self.origin / "failing-host").unlink()
        fixed = self.commit("fix the test")
        self.now += 120
        self.tick()
        self.assertTrue(any(entry[:3] == ["issue", "comment", "42"] and fixed in str(entry)
                            for entry in self.recorded()))

    def test_a_failed_test_report_is_retried_without_testing_again(self):
        self.tick()
        (self.origin / "failing-cli").touch()
        self.commit("break a test")
        self.now += 120
        with patch.object(sentinel, "report_tests", side_effect=RuntimeError("network unavailable")):
            with self.assertRaisesRegex(RuntimeError, "network"):
                self.tick()
        before = self.tested()
        self.now += 120
        self.tick()
        self.assertEqual(self.tested(), before)
        self.assertTrue(any(entry[:2] == ["issue", "create"] for entry in self.recorded()))
        self.assertNotIn("tests_pending", json.loads((self.state / "state.json").read_text()))

    def test_a_required_commit_is_tested_once_and_its_verdict_returned(self):
        self.tick()
        untested = self.commit("version bump")
        self.command("git", "-C", str(self.repo), "fetch", "-q", "origin", "main")
        verdict = sentinel.require_tests(self.repo, self.state, untested)
        self.assertTrue(verdict["passed"])
        before = self.tested()
        self.assertEqual(sentinel.require_tests(self.repo, self.state, untested), verdict)
        self.assertEqual(self.tested(), before)
        (self.origin / "failing-ui").touch()
        failing = self.commit("break a test")
        self.command("git", "-C", str(self.repo), "fetch", "-q", "origin", "main")
        self.assertFalse(sentinel.require_tests(self.repo, self.state, failing)["passed"])
        # The watcher then reports the tip without testing it again.
        self.now += 120
        self.tick()
        self.assertEqual(sum(sha == failing for sha, _ in self.tested()), len(sentinel.TEST_LANES))

    def test_the_issue_shows_each_failure_and_the_lane_command_without_passing_tests(self):
        self.tick()
        (self.origin / "failing-ui").touch()
        self.commit("break a UI test")
        self.now += 120
        self.tick()
        body = (self.state / "test-issue.md").read_text()
        self.assertIn("test lane_breaks ... FAILED", body)
        self.assertIn("the reason it broke", body)
        self.assertIn("error: 1 target failed:", body)
        self.assertNotIn("passing_number_", body)
        self.assertNotIn("later_number_", body)
        self.assertIn("`RUST_TEST_THREADS=2 scripts/cargo-gate test --locked --all-targets --no-fail-fast "
                      + " ".join(sentinel.TEST_LANES["ui"]) + "`", body)

    def test_lanes_get_their_thread_count_and_none_of_the_callers_test_variables(self):
        with patch.dict(os.environ, RUST_TEST_THREADS="64", CAGENTS_TEST_FIXTURE="leaked"):
            self.tick()
        threads = {lane: (remote, count, fixture) for _, lane, remote, count, fixture
                   in (json.loads(line) for line in (self.root / "tests").read_text().splitlines())}
        for lane, (remote, count, fixture) in threads.items():
            self.assertEqual((remote, count, fixture), ("fw16", sentinel.TEST_THREADS.get(lane), None), lane)

    def test_a_lane_stopped_outside_the_code_keeps_no_verdict_and_is_tested_again_later(self):
        self.tick()
        (self.origin / "refused-host").touch()
        refused = self.commit("merge while the build host is full")
        self.now += 120
        self.tick()
        self.assertFalse((self.state / "verdicts" / f"{refused}.json").exists())
        self.assertFalse(any(entry[:2] == ["issue", "create"] for entry in self.recorded()))
        state = json.loads((self.state / "state.json").read_text())
        self.assertNotEqual(state.get("tested"), refused)
        before = self.tested()
        self.now += 120
        self.tick()
        self.assertEqual(self.tested(), before)
        self.now += 1800
        self.tick()
        self.assertGreater(len(self.tested()), len(before))
        # Required directly, it reports the stop as neither pass nor failure.
        verdict = sentinel.require_tests(self.repo, self.state, refused)
        self.assertEqual(verdict["outside_code"], ["host"])
        self.assertFalse((self.state / "verdicts" / f"{refused}.json").exists())

    def test_a_removed_verdict_is_tested_again_by_the_watcher(self):
        self.tick()
        tip = json.loads((self.state / "state.json").read_text())["tested"]
        before = len(self.tested())
        (self.state / "verdicts" / f"{tip}.json").unlink()
        self.now += 120
        self.tick()
        self.assertEqual(len(self.tested()), 2 * before)
        self.assertTrue((self.state / "verdicts" / f"{tip}.json").exists())

    def test_files_a_test_leaves_in_the_checkout_do_not_stop_later_checks(self):
        (self.origin / "litter-cli").touch()
        self.commit("a test that litters")
        self.tick()
        checkout = self.repo / ".worktrees" / "main-build-sentinel"
        self.assertFalse((checkout / "left-by-a-test").exists())
        merged = self.commit("merge after it")
        self.now += 120
        self.tick()
        self.assertEqual(json.loads((self.state / "state.json").read_text())["checked"], merged)

    def test_a_required_commit_not_yet_fetched_is_fetched_and_the_checkout_returned(self):
        self.tick()
        checked = json.loads((self.state / "state.json").read_text())["checked"]
        unfetched = self.commit("version bump just pushed")
        verdict = sentinel.require_tests(self.repo, self.state, unfetched)
        self.assertTrue(verdict["passed"])
        self.assertEqual(verdict["sha"], unfetched)
        checkout = self.repo / ".worktrees" / "main-build-sentinel"
        self.assertEqual(self.command("git", "-C", str(checkout), "rev-parse", "HEAD"), checked)

    def test_a_kept_verdict_is_returned_while_a_tick_holds_the_locks(self):
        self.tick()
        tip = json.loads((self.state / "state.json").read_text())["tested"]
        with ((self.repo / ".worktrees" / ".main-build-sentinel.lock").open("a") as checkout_lock,
              (self.state / "lock").open("a") as lock):
            fcntl.flock(checkout_lock, fcntl.LOCK_EX)
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertTrue(sentinel.require_tests(self.repo, self.state, tip)["passed"])

    def with_isolation_check(self):
        """Commits a lane-isolation check that records its runs and fails
        where the checkout has `leaky`, as when a lane resolves another's graph."""
        script = self.origin / "scripts/check-lane-isolation"
        script.write_text('''#!/usr/bin/python3
import json, os, subprocess, sys
from pathlib import Path
sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
with open(os.environ['SENTINEL_CALLS'], 'a') as out:
    out.write(json.dumps(['check-lane-isolation', sha]) + '\\n')
if Path('leaky').exists():
    print('check-lane-isolation: the ui lane resolves Ghostty (libghostty-vt)', file=sys.stderr)
    sys.exit(1)
print('check-lane-isolation: 6 lanes resolve only their own graphs')
''')
        script.chmod(0o755)
        return self.commit("check lane isolation")

    def isolation_runs(self):
        return [entry for entry in self.recorded() if entry[0] == "check-lane-isolation"]

    def test_a_commit_with_the_isolation_check_runs_it_after_its_lanes(self):
        self.tick()
        # One from before the check has nothing to run.
        self.assertEqual(self.isolation_runs(), [])
        tip = self.with_isolation_check()
        self.now += 120
        self.tick()
        runs = [entry[:2] for entry in self.recorded() if tip in entry[:2]]
        self.assertEqual(runs, [[tip, lane] for lane in sentinel.LANES] + [["check-lane-isolation", tip]])
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual(state["last_good"], tip)
        self.assertIn("6 lanes resolve only their own graphs",
                      (self.state / "logs" / f"{tip}-isolation.log").read_text())

    def test_a_commit_resolving_another_lanes_graph_fails_but_is_still_built_and_tested(self):
        self.tick()
        self.with_isolation_check()
        (self.origin / "leaky").touch()
        leaky = self.commit("ghostty in the ui")
        self.now += 120
        self.tick()
        state = json.loads((self.state / "state.json").read_text())
        self.assertEqual((state["checked"], state["compiled"], state["last_good"]), (leaky, leaky, self.first))
        body = (self.state / "issue.md").read_text()
        self.assertIn("- isolation: exit 1", body)
        self.assertIn("the ui lane resolves Ghostty", body)
        # It compiled, so its tests still run.
        self.assertIn([leaky, "cli"], self.tested())
        (self.origin / "leaky").unlink()
        fixed = self.commit("ghostty back in the host")
        self.now += 120
        self.tick()
        self.assertTrue(any(entry[:3] == ["issue", "comment", "42"] and f"Recovered at `{fixed}`" in entry[4]
                            for entry in self.recorded()))

    def test_a_leak_is_bisected_like_a_lane_to_the_commit_that_made_it(self):
        self.tick()
        # A commit from before the check, between the passing one and the tip.
        before = self.commit("merge one")
        self.with_isolation_check()
        self.commit("merge three")
        (self.origin / "leaky").touch()
        leaky = self.commit("ghostty in the ui")
        self.commit("merge five")
        self.commit("merge six")
        self.now += 120
        self.tick()
        [comment] = [entry for entry in self.recorded() if entry[:3] == ["issue", "comment", "42"]]
        self.assertIn(f"- `{leaky}` (ghostty in the ui): isolation", comment[4])
        # Bisecting runs only the isolation check, compiling nothing again, and
        # treats a commit without it as passing.
        self.assertEqual(len([entry for entry in self.recorded() if len(entry) == 4]), 2 * len(sentinel.LANES))
        self.assertNotIn(before, [entry[1] for entry in self.isolation_runs()])

    def with_evidence_tool(self):
        """Gives main a native-evidence that builds stand-in executables."""
        tool = self.origin / "scripts" / "native-evidence"
        tool.write_text('''#!/usr/bin/python3
import json, os, subprocess, sys
from pathlib import Path
BINS = ("cagents", "cagentsd", "cagents-ui")
if sys.argv[1:] == ["__build-arguments"]:
    print("\\n".join(f"--profile dev-runtime --bin {name}" for name in BINS))
    sys.exit(0)
assert sys.argv[1] == "--task" and sys.argv[3:5] == ["build", "before"], sys.argv
sha = subprocess.check_output(["git", "-C", sys.argv[5], "rev-parse", "HEAD"], text=True).strip()
with open(os.environ["SENTINEL_CALLS"], "a") as out:
    out.write(json.dumps([sha, "evidence", sys.argv[1:]]) + "\\n")
if Path(sys.argv[5], "broken-evidence").exists():
    sys.exit(101)
target = Path(sys.argv[2], "target-before", "dev-runtime")
target.mkdir(parents=True, exist_ok=True)
for name in BINS:
    (target / name).write_text(sha)
''')
        tool.chmod(0o755)
        self.evidence = patch.object(sentinel, "EVIDENCE_TASK", self.root / "evidence")
        self.evidence.start()
        self.addCleanup(self.evidence.stop)
        return self.commit("evidence tool")

    def evidence_builds(self):
        return [entry[0] for entry in self.recorded() if entry[1] == "evidence"]

    def test_the_newest_passing_commit_is_kept_built_for_evidence(self):
        sha = self.with_evidence_tool()
        self.tick()
        build = self.state / "builds" / sha
        self.assertEqual((build / "cagents-ui").read_text(), sha)
        self.assertEqual((build / "arguments").read_text().splitlines()[-1], "--profile dev-runtime --bin cagents-ui")
        self.assertTrue((self.state / "builds" / ".lock").exists())
        self.now += 120
        self.tick()
        self.assertEqual(self.evidence_builds(), [sha])

    def test_a_failing_commit_is_not_built_and_a_failed_build_is_not_retried(self):
        good = self.with_evidence_tool()
        self.tick()
        self.commit("incompatible merge", broken="host")
        self.now += 120
        self.tick()
        self.assertEqual(self.evidence_builds(), [good])
        (self.origin / "broken-host").unlink()
        (self.origin / "broken-evidence").touch()
        unbuildable = self.commit("passes its checks, not its evidence build")
        for _ in range(2):
            self.now += 120
            self.tick()
        self.assertEqual(self.evidence_builds(), [good, unbuildable])
        self.assertFalse((self.state / "builds" / unbuildable).exists())
        self.assertFalse(any(entry.name.startswith(".") and entry.is_dir()
                             for entry in (self.state / "builds").iterdir()))
        self.assertTrue((self.state / "builds" / good).is_dir())

    def test_a_commit_that_compiles_with_warnings_is_still_built(self):
        self.with_evidence_tool()
        self.tick()
        warned = self.commit("lane-only helper", warning="host")
        self.now += 120
        self.tick()
        self.assertEqual(self.evidence_builds()[-1], warned)
        self.assertTrue((self.state / "builds" / warned).is_dir())

    def test_only_the_newest_builds_are_kept(self):
        built = [self.with_evidence_tool()]
        self.tick()
        for number in range(sentinel.BUILDS_KEPT):
            built.append(self.commit(f"merge {number}"))
            self.now += 120
            self.tick()
        kept = {entry.name for entry in (self.state / "builds").iterdir() if not entry.name.startswith(".")}
        self.assertEqual(kept, set(built[1:]))

    def test_a_build_left_unfinished_is_removed(self):
        self.with_evidence_tool()
        (self.state / "builds" / ".abandoned.new").mkdir(parents=True)
        self.tick()
        self.assertFalse((self.state / "builds" / ".abandoned.new").exists())


class LaneSelectionTests(unittest.TestCase):
    def test_a_commit_from_before_a_package_was_split_out_checks_without_it(self):
        lane = sentinel.LANES["browser"]
        self.assertEqual(sentinel.present(lane, {"cagents", "cagents-ui"}),
                         ["-p", "cagents-ui", "--features", "cagents-ui/browser-protocol-fixture"])
        self.assertEqual(sentinel.present(lane, {"cagents", "cagents-ui", "cagents-ui-kit"}), lane)
        # Members the manifests could not name: Cargo reports what is missing.
        self.assertEqual(sentinel.present(lane, None), lane)

    def test_a_commit_from_before_the_kit_was_split_out_tests_without_it(self):
        command = sentinel.test_command("ui", {"cagents", "cagents-ui"})
        self.assertNotIn("cagents-ui-kit", command)
        self.assertEqual(command[command.index("--no-fail-fast") + 1:],
                         ["--no-default-features", "-p", "cagents", "-p", "cagents-ui", "--features", "cagents/ui"])
        self.assertIn("cagents-ui-kit", sentinel.test_command("ui", {"cagents", "cagents-ui", "cagents-ui-kit"}))


if __name__ == "__main__":
    unittest.main()
