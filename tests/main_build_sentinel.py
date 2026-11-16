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
        self.environment = patch.dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                                      SENTINEL_CALLS=str(self.calls),
                                      SENTINEL_LANES=json.dumps(sentinel.LANES))
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

    def test_all_three_lanes_and_every_new_commit_without_noop_rebuilds(self):
        self.tick()
        second = self.commit("merge one")
        third = self.commit("merge two")
        self.now += 120
        self.tick()
        self.now += 120
        self.tick()
        records = self.recorded()
        self.assertEqual([(entry[0], entry[1]) for entry in records],
                         [(sha, lane) for sha in (self.first, second, third) for lane in sentinel.LANES])
        for entry in records:
            self.assertEqual(entry[2][:4], ['check', '--locked', '--no-default-features', '--all-targets'])
            self.assertEqual(entry[2][-1], '--message-format=json')
            self.assertEqual(entry[3], 'fw16')

    def test_commit_before_the_application_package_skips_only_its_lanes(self):
        self.tick()
        self.command("git", "-C", str(self.origin), "rm", "-q", "crates/cagents-ui/Cargo.toml")
        earlier = self.commit("before the application package")
        self.now += 120
        self.tick()
        self.assertEqual([entry[1] for entry in self.recorded() if entry[0] == earlier],
                         ["cli", "host", "ui"])

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


if __name__ == "__main__":
    unittest.main()
