#!/usr/bin/python3
"""Merge queue contracts: real Git histories, offline gh and Cargo gate peers."""

from contextlib import contextmanager
import fcntl
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts/merge-queue"

# Runs in its own processes as both fake gh and the fake gate found on PATH.
PEER = r'''#!/usr/bin/python3
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(os.environ['MERGE_QUEUE_FIXTURE'])

def git(*args):
    return subprocess.check_output(['git', *args], cwd=root / 'server', stderr=subprocess.STDOUT, text=True).strip()

with (root / 'peer.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    data = json.loads((root / 'peer.json').read_text())
    args = sys.argv[1:]
    if Path(sys.argv[0]).name == 'cargo-gate':
        lane = args[args.index('--features') + 1] if '--features' in args else 'cli'
        head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
        tree = subprocess.check_output(['git', 'rev-parse', 'HEAD^{tree}'], text=True).strip()
        data['lanes'].append({'lane': lane, 'operation': args[0], 'head': head, 'tree': tree,
                              'args': args, 'helper': os.environ.get('CAGENTS_TEST_RETAINED_ACTOR_HELPER'),
                              'loopback': os.environ.get('CAGENTS_BUILD_REMOTE_LOOPBACK'),
                              'build_remote': os.environ.get('CAGENTS_BUILD_REMOTE')})
        if args[0] == 'exec':
            assert args[1:5] == ['/usr/bin/python3', 'scripts/build-ceditor-product', 'cagentsd', '--locked'], args
            product = Path.cwd() / 'target/fixture-cagentsd'
            product.parent.mkdir(exist_ok=True)
            product.write_text('#!/bin/sh\nexit 0\n')
            product.chmod(0o755)
            (root / 'peer.json').write_text(json.dumps(data))
            print(product)
            sys.exit(0)
        if data.get('move_main') and not data.get('moved_main'):
            git('fetch', '-q', 'origin', 'main')
            git('reset', '--hard', 'origin/main')
            (root / 'server/external').write_text('other steward\n')
            git('add', '.')
            git('commit', '-qm', 'other steward')
            git('push', '-q', 'origin', 'HEAD:main')
            data['moved_main'] = True
        block = data.get('block_gate') and not data.get('blocked')
        if block:
            data['blocked'] = True
        if data.get('mutate_checkout') and lane == 'ui':
            Path('shared').write_text('changed during build\n')
        fail = (data.get('fail_base') or Path('broken').exists()
                or (Path('left').exists() and Path('right').exists())
                or (Path('cooperate-a').exists() != Path('cooperate-b').exists()))
        (root / 'peer.json').write_text(json.dumps(data))
        fcntl.flock(lock, fcntl.LOCK_UN)
        if block:
            (root / 'gate-started').touch()
            deadline = time.monotonic() + 20
            while not (root / 'release-gate').exists():
                if time.monotonic() > deadline:
                    sys.exit('fixture gate release timed out')
                time.sleep(.02)
        runtime_fail = (args[0] == 'test' and (
            (lane == 'cli' and (data.get('fail_runtime_base') or Path('runtime-broken').exists()
                               or (Path('runtime-left').exists() and Path('runtime-right').exists())))
            or Path(f'{lane}-runtime-broken').exists()))
        if runtime_fail:
            print(f'error: {lane} test failed: fixture EWOULDBLOCK')
            sys.exit(1)
        if fail and lane == 'host':
            print('error[E0001]: fixture broken change')
            sys.exit(1)
        sys.exit(0)
    assert args[0] == 'api', args
    if 'graphql' in args:
        number = next(int(a.split('=', 1)[1]) for a in args if a.startswith('number='))
        pr = data['prs'][str(number)]
        data['queries'].append(number)
        answer = {'data': {'repository': {'pullRequest': pr}}}
    elif 'PUT' in args:
        endpoint = next(a for a in args if a.startswith('repos/'))
        number = int(endpoint.split('/')[-2])
        pr = data['prs'][str(number)]
        sha = next(a[4:] for a in args if a.startswith('sha='))
        assert pr['state'] == 'OPEN', pr
        assert sha == pr['headRefOid'] or data.get('head_moves_on_write'), (sha, pr)
        data['writes'].append(number)
        if data.get('head_moves_on_write'):
            (root / 'peer.json').write_text(json.dumps(data))
            print('HTTP/2.0 409 Conflict\n\n{"message":"Head branch was modified"}')
            sys.exit(1)
        git('fetch', '-q', 'origin', 'main')
        git('reset', '--hard', 'origin/main')
        git('fetch', '-q', 'origin', sha)
        git('merge', '--no-ff', '--no-edit', '-m', f'Merge pull request #{number}', sha)
        if data.get('mismatch'):
            (root / 'server/unexpected').write_text(f'untested {number}\n')
            git('add', '.')
            git('commit', '-qm', 'unexpected change')
        git('push', '-q', 'origin', 'HEAD:main')
        pr['state'] = 'MERGED'
        answer = {'merged': True, 'sha': git('rev-parse', 'HEAD')}
    else:
        raise AssertionError(args)
    (root / 'peer.json').write_text(json.dumps(data))
    print('HTTP/2.0 200 OK\nContent-Type: application/json\n\n' + json.dumps(answer))
'''


def wait_for(predicate, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(.03)
    return False


class MergeQueue(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / ".worktrees"
        scratch.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="merge-queue-test-", dir=scratch)
        self.directory = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.repo = self.directory / "checkout"
        self.origin = self.directory / "origin.git"
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.env = os.environ | {"MERGE_QUEUE_FIXTURE": str(self.directory),
                                 "PATH": str(self.bin) + ":/usr/bin:/bin",
                                 "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                                 "GIT_AUTHOR_NAME": "Queue fixture", "GIT_AUTHOR_EMAIL": "queue@example.invalid",
                                 "GIT_COMMITTER_NAME": "Queue fixture", "GIT_COMMITTER_EMAIL": "queue@example.invalid"}
        for key in list(self.env):
            if key.startswith('GIT_') and key not in {'GIT_CONFIG_NOSYSTEM', 'GIT_CONFIG_GLOBAL',
                                                     'GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL',
                                                     'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL'}:
                del self.env[key]
        for name in ("gh", "cargo-gate"):
            path = self.bin / name
            path.write_text(PEER)
            path.chmod(0o755)
        self.call(["git", "init", "--bare", "--initial-branch=main", str(self.origin)], cwd=self.directory)
        self.call(["git", "clone", str(self.origin), str(self.repo)], cwd=self.directory)
        (self.repo / "scripts").mkdir()
        shutil.copy2(TOOL, self.repo / "scripts/merge-queue")
        # Production invokes the gate in the tested checkout. This offline
        # fixture's tracked entrypoint delegates to its fake gate on PATH.
        gate = self.repo / "scripts/cargo-gate"
        gate.write_text('#!/bin/sh\nexec cargo-gate "$@"\n')
        gate.chmod(0o755)
        (self.repo / ".gitignore").write_text('.worktrees/\ntarget/\n')
        (self.repo / "shared").write_text('original\n')
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("push", "-q", "origin", "main")
        # Origin is a real local bare remote; only URL identity discovery is
        # shimmed, leaving fetches and fake merge writes entirely offline.
        real_git = shutil.which("git", path="/usr/bin:/bin")
        (self.bin / "git").write_text(f'''#!/bin/sh
if [ "$*" = "remote get-url origin" ]; then
    echo https://github.com/fixture/queue.git
else
    exec {real_git} "$@"
fi
''')
        (self.bin / "git").chmod(0o755)
        self.call(["git", "clone", str(self.origin), str(self.directory / "server")], cwd=self.directory)
        self.peer_path = self.directory / "peer.json"
        self.peer_path.write_text(json.dumps({"prs": {}, "queries": [], "writes": [], "lanes": []}))
        self.state_path = self.repo / ".git/merge-queue/state.json"
        self.addCleanup(self.stop_runner)

    def call(self, args, *, cwd=None, check=True, timeout=120):
        result = subprocess.run(args, cwd=cwd or self.repo, env=self.env,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        if check:
            self.assertEqual(result.returncode, 0, result.stdout)
        return result

    def git(self, *args):
        return self.call(["git", *args]).stdout.strip()

    def queue(self, *args, check=True, cwd=None):
        return self.call([str(self.repo / "scripts/merge-queue"), *map(str, args)], check=check, cwd=cwd)

    @contextmanager
    def peer(self):
        with (self.directory / "peer.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            data = json.loads(self.peer_path.read_text())
            yield data
            self.peer_path.write_text(json.dumps(data))

    def data(self):
        with self.peer() as data:
            return data

    def state(self):
        return json.loads(self.state_path.read_text())

    def stop_runner(self):
        (self.directory / "release-gate").touch()
        if not self.state_path.exists():
            return
        # Lease ownership, rather than an arbitrary sleep, proves no children
        # still use the fixture before TemporaryDirectory removes it.
        path = self.state_path.parent / "runner.lock"
        with path.open('a') as lock:
            def free():
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return True
                except BlockingIOError:
                    return False
            self.assertTrue(wait_for(free), 'fixture runner did not exit')

    def pr(self, number, filename=None, text=None, *, base=None):
        self.git("checkout", "-q", "-B", f"pr-{number}", base or self.base)
        (self.repo / (filename or f"file-{number}")).write_text(text or f"change {number}\n")
        self.git("add", ".")
        self.git("commit", "-qm", f"PR {number}")
        sha = self.git("rev-parse", "HEAD")
        self.git("push", "-q", "origin", f"HEAD:refs/heads/pr-{number}")
        self.git("checkout", "-q", "main")
        record = {"state": "OPEN", "headRefOid": sha, "baseRefName": "main", "isDraft": False,
                  "commits": {"nodes": [{"commit": {"oid": sha, "statusCheckRollup": {
                      "state": "SUCCESS", "contexts": {"pageInfo": {"hasNextPage": False},
                      "nodes": [{"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"}]}}}}]}}
        with self.peer() as data:
            data['prs'][str(number)] = record
        return sha

    @contextmanager
    def held_runner(self):
        self.queue("status")
        with (self.state_path.parent / "runner.lock").open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def batch(self, numbers):
        with self.held_runner():
            for number in numbers:
                self.queue("enqueue", number)
        return [self.queue("wait", n, check=False) for n in numbers]

    def test_batch_checks_three_lanes_then_cli_tests_once_and_pins_every_write(self):
        for n in range(1, 5):
            self.pr(n)
        outcomes = self.batch(range(1, 5))
        self.assertEqual([r.returncode for r in outcomes], [0] * 4)
        data = self.data()
        self.assertEqual(data['writes'], [1, 2, 3, 4])
        self.assertEqual(data['queries'], [1, 2, 3, 4])
        self.assertEqual([(r['operation'], r['lane']) for r in data['lanes']],
                         [('check', 'cli'), ('check', 'host'), ('check', 'ui'), ('test', 'cli')])
        self.assertEqual(len({r['tree'] for r in data['lanes']}), 1)
        for lane in data['lanes']:
            self.assertEqual(lane['args'][1:4], ['--locked', '--no-default-features', '--all-targets'])
        self.git('fetch', '-q', 'origin', 'main')
        self.assertEqual(data['lanes'][0]['tree'], self.git('rev-parse', 'origin/main^{tree}'))
        self.assertFalse((self.repo / '.worktrees/merge-queue-batch').exists())

    def test_conflict_rejects_only_conflicting_pr(self):
        self.pr(1, 'shared', 'first\n')
        self.pr(2, 'shared', 'second\n')
        self.pr(3)
        outcomes = self.batch([1, 2, 3])
        self.assertEqual([r.returncode for r in outcomes], [0, 2, 0])
        self.assertEqual(self.data()['writes'], [1, 3])

    def test_bisection_lands_innocent_prs_in_same_run(self):
        for n in range(1, 7):
            self.pr(n, 'broken' if n == 3 else None)
        outcomes = self.batch(range(1, 7))
        self.assertEqual([r.returncode for r in outcomes], [0, 0, 3, 0, 0, 0])
        self.assertIn('error[E0001]', outcomes[2].stdout)
        self.assertEqual(self.data()['writes'], [1, 2, 4, 5, 6])
        self.assertGreater(len(self.data()['lanes']), 6)

    def test_interacting_changes_do_not_land_as_an_untested_union(self):
        self.pr(1, 'left')
        self.pr(2, 'right')
        outcomes = self.batch([1, 2])
        self.assertEqual([r.returncode for r in outcomes], [0, 3])
        self.assertEqual(self.data()['writes'], [1])

    def test_build_mutation_cannot_be_called_a_tested_tree(self):
        self.pr(1)
        with self.peer() as data:
            data['mutate_checkout'] = True
        result = self.batch([1])[0]
        self.assertEqual(result.returncode, 8, result.stdout)
        self.assertIn('build changed the tested checkout', result.stdout)
        self.assertEqual(self.data()['writes'], [])

    def test_cooperating_prs_across_failed_halves_survive_bisection(self):
        self.pr(1, 'cooperate-a')
        self.pr(2, 'broken')
        self.pr(3, 'cooperate-b')
        self.pr(4)
        outcomes = self.batch([1, 2, 3, 4])
        self.assertEqual([r.returncode for r in outcomes], [0, 3, 0, 0],
                         '\n'.join(r.stdout for r in outcomes))
        self.assertEqual(self.data()['writes'], [1, 3, 4])

    def test_cli_runtime_failure_bisects_after_all_checks_pass(self):
        for number in range(1, 5):
            self.pr(number, 'runtime-broken' if number == 2 else None)
        outcomes = self.batch([1, 2, 3, 4])
        self.assertEqual([r.returncode for r in outcomes], [0, 3, 0, 0],
                         '\n'.join(r.stdout for r in outcomes))
        self.assertIn('test-cli failed', outcomes[1].stdout)
        self.assertIn('EWOULDBLOCK', outcomes[1].stdout)
        data = self.data()
        self.assertEqual(data['writes'], [1, 3, 4])
        self.assertGreater(len(data['lanes']), 4)
        for offset in range(0, len(data['lanes']), 4):
            self.assertEqual([(r['operation'], r['lane']) for r in data['lanes'][offset:offset + 4]],
                             [('check', 'cli'), ('check', 'host'), ('check', 'ui'), ('test', 'cli')])

    def test_interacting_cli_runtime_regression_cannot_merge(self):
        self.pr(1, 'runtime-left')
        self.pr(2, 'runtime-right')
        outcomes = self.batch([1, 2])
        self.assertEqual([r.returncode for r in outcomes], [0, 3])
        self.assertIn('test-cli failed', outcomes[1].stdout)
        self.assertEqual(self.data()['writes'], [1])

    def test_optional_tests_union_persists_through_runtime_bisection(self):
        self.pr(1)
        self.pr(2, 'ui-runtime-broken')
        self.pr(3)
        with self.held_runner():
            self.queue('enqueue', 1, '--test-lanes', 'host')
            self.queue('enqueue', 2, '--test-lanes', 'ui')
            self.queue('enqueue', 3)
        outcomes = [self.queue('wait', n, check=False) for n in (1, 2, 3)]
        self.assertEqual([r.returncode for r in outcomes], [0, 3, 0],
                         '\n'.join(r.stdout for r in outcomes))
        self.assertIn('test-ui failed', outcomes[1].stdout)
        data = self.data()
        self.assertEqual(data['writes'], [1, 3])
        for offset in range(0, len(data['lanes']), 7):
            attempt = data['lanes'][offset:offset + 7]
            self.assertEqual([(r['operation'], r['lane']) for r in attempt], [
                ('check', 'cli'), ('check', 'host'), ('check', 'ui'), ('test', 'cli'),
                ('exec', 'host'), ('test', 'host'), ('test', 'ui')])
            self.assertEqual(attempt[4]['head'], attempt[5]['head'])
            self.assertTrue(attempt[5]['helper'].endswith('/target/fixture-cagentsd'))
        self.assertIn('tests=cli,host', self.queue('status').stdout)

    def test_host_test_failure_uses_matching_helper_and_is_attributed(self):
        self.pr(1, 'host-runtime-broken')
        with self.held_runner():
            self.queue('enqueue', 1, '--test-lanes', 'host')
        outcome = self.queue('wait', 1, check=False)
        self.assertEqual(outcome.returncode, 3, outcome.stdout)
        self.assertIn('test-host failed', outcome.stdout)
        self.assertEqual(self.data()['writes'], [])

    def test_git_config_default_and_explicit_cli_only_override_are_saved(self):
        self.pr(1)
        self.pr(2)
        self.git('config', '--local', 'mergequeue.testLanes', 'host,ui')
        with self.held_runner():
            self.queue('enqueue', 1)
            self.queue('enqueue', 2, '--test-lanes', 'none')
            records = self.state()['requests']
            self.assertEqual([r['test_lanes'] for r in records], [['host', 'ui'], []])
            conflict = self.queue('enqueue', 1, '--test-lanes', 'none', check=False)
            self.assertEqual(conflict.returncode, 8, conflict.stdout)
        self.queue('wait', 1)
        self.queue('wait', 2)
        self.assertEqual(len(self.data()['lanes']), 7)

    def test_local_runtime_steps_use_loopback_slot_scratch(self):
        self.pr(1)
        self.env['CAGENTS_BUILD_REMOTE'] = 'local'
        self.env['CAGENTS_BUILD_REMOTE_LOOPBACK'] = '0'
        self.assertEqual(self.batch([1])[0].returncode, 0)
        steps = self.data()['lanes']
        self.assertEqual(len(steps), 4)
        self.assertTrue(all(step['loopback'] == '1' for step in steps))
        self.assertTrue(all(step['build_remote'] == os.uname().nodename for step in steps))

    def test_cli_runtime_failure_on_main_is_not_blamed_on_a_pr(self):
        self.pr(1)
        with self.peer() as data:
            data['fail_runtime_base'] = True
        outcome = self.batch([1])[0]
        self.assertEqual(outcome.returncode, 8, outcome.stdout)
        self.assertIn('baseline fails', outcome.stdout)
        self.assertIn('test-cli failed', outcome.stdout)
        self.assertEqual(self.data()['writes'], [])

    def test_main_move_rebuilds_before_writes(self):
        self.pr(1)
        with self.peer() as data:
            data['move_main'] = True
        self.assertEqual(self.batch([1])[0].returncode, 0)
        data = self.data()
        self.assertEqual(len(data['lanes']), 8)
        self.assertNotEqual(data['lanes'][0]['tree'], data['lanes'][4]['tree'])
        self.assertEqual(data['writes'], [1])

    def test_head_moved_is_distinct_and_not_built(self):
        self.pr(1)
        with self.held_runner():
            self.queue('enqueue', 1, '--sha', self.base)
        result = self.queue('wait', 1, check=False)
        self.assertEqual(result.returncode, 5, result.stdout)
        self.assertEqual(self.data()['lanes'], [])
        self.assertEqual(self.data()['writes'], [])

    def test_pinned_write_rejects_a_head_moving_after_check(self):
        self.pr(1)
        with self.peer() as data:
            data['head_moves_on_write'] = True
        result = self.batch([1])[0]
        self.assertEqual(result.returncode, 5, result.stdout)

    def test_concurrent_enqueue_joins_next_round_without_lost_wakeup(self):
        self.pr(1)
        self.pr(2)
        with self.peer() as data:
            data['block_gate'] = True
        self.queue('enqueue', 1)
        self.assertTrue(wait_for(lambda: (self.directory / 'gate-started').exists()))
        self.queue('enqueue', 2, '--test-lanes', 'ui')
        listing = self.queue('status').stdout
        self.assertIn('checking', listing)
        self.assertIn('#2', listing)
        (self.directory / 'release-gate').touch()
        self.assertEqual(self.queue('wait', 1).returncode, 0)
        self.assertEqual(self.queue('wait', 2).returncode, 0)
        self.assertEqual(len(self.data()['lanes']), 9)
        self.assertEqual([(r['operation'], r['lane']) for r in self.data()['lanes'][-2:]],
                         [('test', 'cli'), ('test', 'ui')])
        self.assertEqual(self.data()['writes'], [1, 2])

    def test_killed_runner_keeps_lease_until_its_gate_exits(self):
        self.pr(1)
        self.pr(2)
        with self.peer() as data:
            data['block_gate'] = True
        self.queue('enqueue', 1)
        self.assertTrue(wait_for(lambda: (self.directory / 'gate-started').exists()))
        pid = self.state()['batch']['pid']
        os.kill(pid, signal.SIGKILL)
        self.queue('enqueue', 2, '--test-lanes', 'ui')
        with (self.state_path.parent / 'runner.lock').open('a') as lock:
            with self.assertRaises(BlockingIOError):
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.assertEqual(len(self.data()['lanes']), 1)
        self.git('config', '--local', 'mergequeue.testLanes', 'none')
        (self.directory / 'release-gate').touch()
        interrupted = self.queue('wait', 1, check=False)
        self.assertEqual(interrupted.returncode, 8, interrupted.stdout)
        self.assertIn('interrupted', interrupted.stdout)
        self.queue('wait', 2)
        self.assertEqual(self.data()['writes'], [2])
        self.assertEqual(self.data()['lanes'][-1]['lane'], 'ui')
        self.assertEqual(self.data()['lanes'][-1]['operation'], 'test')

    def test_broken_baseline_is_not_attributed_to_prs(self):
        self.pr(1)
        self.pr(2)
        with self.peer() as data:
            data['fail_base'] = True
        outcomes = self.batch([1, 2])
        self.assertEqual([r.returncode for r in outcomes], [8, 8])
        self.assertIn('baseline fails', outcomes[0].stdout)
        self.assertEqual(self.data()['writes'], [])

    def test_shared_common_dir_across_steward_worktrees_and_duplicate_enqueue(self):
        self.pr(1)
        other = self.repo / '.worktrees/steward'
        self.git('worktree', 'add', '--detach', str(other), self.base)
        with self.held_runner():
            self.queue('enqueue', 1)
            self.queue('enqueue', 1, cwd=other)
        self.assertEqual(len(self.state()['requests']), 1)
        self.queue('wait', 1, cwd=other)
        self.assertEqual(self.data()['writes'], [1])

    def test_reenqueue_after_merge_uses_closed_metadata_without_polling(self):
        self.pr(1)
        self.assertEqual(self.batch([1])[0].returncode, 0)
        self.queue('enqueue', 1)
        closed = self.queue('wait', 1, check=False)
        self.assertEqual(closed.returncode, 6, closed.stdout)
        self.assertEqual(self.data()['queries'], [1])
        self.assertEqual(len(self.data()['lanes']), 4)
        self.assertEqual(self.data()['writes'], [1])

    def test_ci_and_closed_have_distinct_outcomes(self):
        self.pr(1)
        self.pr(2)
        self.pr(3)
        with self.peer() as data:
            data['prs']['1']['commits']['nodes'][0]['commit']['statusCheckRollup']['state'] = 'PENDING'
            data['prs']['2']['state'] = 'CLOSED'
            # Aggregate SUCCESS does not authorize an unfinished nonrequired check.
            data['prs']['3']['commits']['nodes'][0]['commit']['statusCheckRollup']['contexts']['nodes'][0]['status'] = 'IN_PROGRESS'
        self.assertEqual([r.returncode for r in self.batch([1, 2, 3])], [4, 6, 4])
        self.assertEqual(self.data()['writes'], [])

    def test_tree_mismatch_alerts_all_batch_waiters_and_halts_queue(self):
        self.pr(1)
        self.pr(2)
        with self.peer() as data:
            data['mismatch'] = True
        outcomes = self.batch([1, 2])
        self.assertEqual([r.returncode for r in outcomes], [7, 7], '\n'.join(r.stdout for r in outcomes))
        self.assertTrue(all('TREE MISMATCH' in r.stdout for r in outcomes))
        self.assertIn('TREE MISMATCH', self.queue('status').stdout)
        self.assertEqual(self.queue('enqueue', 1, check=False).returncode, 8)


class ApiCooldown(unittest.TestCase):
    def module(self):
        loader = importlib.machinery.SourceFileLoader('merge_queue_contract', str(TOOL))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        module = importlib.util.module_from_spec(spec)
        loader.exec_module(module)
        return module

    def test_retry_after_and_reset_are_honored_without_fast_retry(self):
        module = self.module()
        for headers, expected in [('Retry-After: 300', 300), ('X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 1800', 801)]:
            with self.subTest(headers=headers):
                queue = object.__new__(module.Queue)
                queue.root = ROOT
                state = {'retry_at': 0}

                @contextmanager
                def transaction():
                    yield state

                queue.state = transaction
                queue.log = lambda _: None
                now = [1000.0]
                sleeps = []

                def sleep(delay):
                    sleeps.append(delay)
                    now[0] += delay

                failed = subprocess.CompletedProcess([], 1, f'HTTP/2.0 403 Forbidden\n{headers}\n\n{{"message":"rate limit"}}')
                success = subprocess.CompletedProcess([], 0, 'HTTP/2.0 200 OK\n\n{"ok":true}')
                with patch.object(module, 'command', side_effect=[failed, success]), \
                        patch.object(module.time, 'time', side_effect=lambda: now[0]), \
                        patch.object(module.time, 'sleep', side_effect=sleep):
                    self.assertEqual(queue.api(['graphql']), {'ok': True})
                self.assertEqual(sleeps, [expected])
    def test_concurrent_rate_limit_cannot_shorten_shared_deadline(self):
        module = self.module()
        queue = object.__new__(module.Queue)
        queue.root = ROOT
        queue.log = lambda _: None
        state = {'retry_at': 0}

        @contextmanager
        def transaction():
            yield state

        queue.state = transaction
        now = [1000.0]
        calls = []

        def command(*_, **kwargs):
            calls.append(now[0])
            if len(calls) == 1:
                # Another in-flight PR read records a longer cooldown before
                # this read's shorter Retry-After response is handled.
                state['retry_at'] = 2000
                return subprocess.CompletedProcess([], 1,
                    'HTTP/2.0 403 Forbidden\nRetry-After: 300\n\n{"message":"rate limit"}')
            return subprocess.CompletedProcess([], 0, 'HTTP/2.0 200 OK\n\n{"ok":true}')

        def sleep(delay):
            now[0] += delay

        with patch.object(module, 'command', side_effect=command), \
                patch.object(module.time, 'time', side_effect=lambda: now[0]), \
                patch.object(module.time, 'sleep', side_effect=sleep):
            self.assertEqual(queue.api(['graphql']), {'ok': True})
        self.assertEqual(calls, [1000, 2000])

    def test_merge_retry_rechecks_base_after_cooldown(self):
        module = self.module()
        queue = object.__new__(module.Queue)
        queue.root = ROOT
        queue.log = lambda _: None
        state = {'retry_at': 0}

        @contextmanager
        def transaction():
            yield state

        queue.state = transaction
        now = [1000.0]
        calls = []

        def before_attempt():
            calls.append(now[0])
            if len(calls) == 2:
                raise module.MainMoved()

        def sleep(delay):
            now[0] += delay

        failed = subprocess.CompletedProcess([], 1,
            'HTTP/2.0 403 Forbidden\nRetry-After: 300\n\n{"message":"rate limit"}')
        with patch.object(module, 'command', return_value=failed) as command, \
                patch.object(module.time, 'time', side_effect=lambda: now[0]), \
                patch.object(module.time, 'sleep', side_effect=sleep):
            with self.assertRaises(module.MainMoved):
                queue.api(['-X', 'PUT', 'repos/fixture/queue/pulls/1/merge'], before_attempt)
        self.assertEqual(command.call_count, 1, 'must rebuild before retrying the write')
        self.assertEqual(calls, [1000, 1300])


if __name__ == '__main__':
    unittest.main()
