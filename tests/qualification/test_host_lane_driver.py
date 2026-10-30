"""Offline protocol fixtures for the maintained HOST driver.

No fixture opens the real C lock, imports Custody, invokes the checkout helper,
creates the canonical slot, starts a process or executes Cargo/an ELF. Injected
Custody/lock/source observations exercise orchestration and raw-output refusal;
they do not establish productive usage, kernel custody or cache performance.
Run only after fixture admission: /usr/bin/python3 -I -B
    tests/qualification/test_host_lane_driver.py -v
"""
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


PATH = Path(__file__).resolve().parents[2] / 'scripts/qualification/run-host-lane.py'
SPEC = importlib.util.spec_from_file_location('host_lane_driver', PATH)
driver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(driver)
SELECTOR = 'chat::protocol::tests::owning_parent'
OTHER = 'chat::runtime::tests::another_parent'


def positive_return(exit_code=0):
    return {'state': driver.RETURN_STATE, 'lock_acquired': True, 'lease': driver.RETURN_LEASE,
            'all_owned_pidfds_closed': True, 'exit_code': exit_code, 'finished_utc': 'fixture-finished',
            'custody_final': {'subreaper': True, 'initial_children_empty': True,
                'boot_id': 'fixture-boot', 'wrapper': {'pid': 10, 'start_ticks': 1, 'uid': os.getuid()},
                'launch_count': 1, 'nodes': [{'pid': 11, 'start_ticks': 2, 'uid': os.getuid(),
                    'settled': True, 'exit_code': exit_code, 'signals': [],
                    'exit_status_source': 'original Popen.wait/poll reaping'}]}}


def source(head='a' * 40, previous='absent'):
    return {'original_checkout': '/tmp/reviewed-original-source', 'head': head,
            'tree': 'b' * 40, 'parent': 'c' * 40, 'expect_current': previous,
            'sha256': {name: 'd' * 64 for name in driver.REQUIRED_PINS}}


def plan(directory, head='a' * 40, previous='absent', workspace=None):
    directory = Path(directory)
    workspace = directory / 'workspace' if workspace is None else Path(workspace)
    value = {'version': 1, 'workspace_root': str(workspace),
            'control_repository': str(workspace / 'persistent-control'),
            'common_git_dir': str(workspace / 'persistent-control/.git'),
            'identities': {role: [1, 2] for role in driver.IDENTITY_ROLES},
            'tools': {role: {'path': str(directory / 'physical-tools' / role), 'sha256': '1' * 64,
                             'device_inode': [1, 2]} for role in driver.TOOL_ROLES},
            'source': source(head, previous),
            'artifacts': {'checkout_helper': {'path': '/tmp/reviewed-checkout-helper', 'sha256': 'e' * 64},
                          'custody': {'path': '/tmp/reviewed-custody', 'sha256': 'f' * 64}},
            'selectors': [SELECTOR], 'evidence_directory': str(directory), 'total_workload_seconds': 2100}
    value['source']['original_checkout'] = str(workspace / 'candidate-source')
    return value


def prepared(value, paths):
    previous = value['expect_current']
    operation = 'created' if previous == 'absent' else 'reused' if previous == value['head'] else 'updated'
    return {'version': 1, 'status': 'prepared', 'operation': operation,
            'repository': str(paths.repository), 'common_git_dir': str(paths.common),
            'checkout': str(paths.checkout), 'owner': driver.OWNER,
            'head': value['head'], 'tree': value['tree'],
            'previous_head': None if previous == 'absent' else previous,
            'registry': str(paths.common / 'validation-checkouts-v1' /
                            (driver.hashlib.sha256(os.fsencode(paths.checkout)).hexdigest() + '.json')),
            'detached': True, 'clean': True, 'source_only': True,
            'external_requirements': ['exclusive_build_lane_ownership', 'prior_owned_family_settled']}


def cargo_output(paths, checkout=None, features=None):
    checkout = paths.checkout if checkout is None else checkout
    artifact = {'reason': 'compiler-artifact', 'manifest_path': str(checkout / 'Cargo.toml'),
                'target': {'name': 'cagents', 'kind': ['lib'], 'src_path': str(checkout / 'src/lib.rs')},
                'profile': {'test': True}, 'features': features or ['host'],
                'executable': str(paths.target / 'debug/deps/cagents-a12b')}
    return (json.dumps(artifact) + '\n' + json.dumps({'reason': 'build-finished', 'success': True}) + '\n').encode()


def parent_output(selector=SELECTOR, filtered=1):
    return (f'running 1 test\ntest {selector} ... ok\n\n'
            f'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; {filtered} filtered out; finished in 0.01s\n').encode()


def result(raw, code=0, state='finished', cleanup=None):
    return SimpleNamespace(stdout=raw, original_exit=code, state=state, cleanup_exit=cleanup)


class FakeCustody:
    def __init__(self, runner):
        self.runner, self.pid, self.nodes = runner, 999, {}

    def scan(self):
        self.runner.events.append('owned-scan')

    def reap(self, node):
        return node.get('settled') is True

    def snapshot(self):
        return positive_return()['custody_final'] | {'nodes': list(self.nodes.values())}

    def capture(self, argv, cwd, role, timeout, on_incomplete):
        target = self.runner.control_launches if role.startswith('control-git-') else self.runner.launches
        target.append((role, list(argv), cwd, timeout))
        self.runner.events.append('launch:' + role)
        outcome = self.runner.outputs.get(role)
        if outcome is None:
            if role == 'control-git-0':
                outcome = result((str(self.runner.paths.repository) + '\n').encode())
            elif role == 'control-git-1':
                value = self.runner.control_common_override or str(self.runner.paths.common)
                outcome = result((value + '\n').encode())
            elif role == 'prepare-canonical-checkout':
                outcome = result(json.dumps(prepared(self.runner.plan['source'], self.runner.paths)).encode())
            elif role == 'host-binary-check':
                outcome = result(b'Finished `dev` profile [unoptimized + debuginfo] target(s)\n')
            elif role == 'host-lib-test-compilation':
                outcome = result(cargo_output(self.runner.paths))
            elif role == 'same-ELF-complete-inventory':
                outcome = result(f'{SELECTOR}: test\n{OTHER}: test\n\n2 tests, 0 benchmarks\n'.encode())
            elif role == 'parent-01':
                outcome = result(parent_output())
            else:
                raise AssertionError('Unexpected fixture launch: ' + role)
        if outcome.state != 'finished':
            on_incomplete(outcome)
        if role == self.runner.unsettled_after:
            self.runner.outstanding = {1000}
        return outcome

    def settle_all(self, _observer, _report):
        self.runner.events.append('positive-own-family-settlement')
        return self.snapshot()

    def close_pidfds(self):
        self.runner.events.append('own-pidfds-closed')


class ProtocolRunner(driver.Runner):
    """Use real run/prepare/captured/workload; replace external authority/probes."""
    def __init__(self, directory, head='a' * 40, previous='absent'):
        self.plan = plan(directory, head, previous)
        self.paths = driver.HostPaths(self.plan)
        self.work = Path(directory)
        self.receipt = self.work / 'receipt.json'
        self.prior_path = self.work / 'prior-return.json'
        self.prior_path.write_text(json.dumps(positive_return()))
        self.prior_return = None
        self.events, self.launches, self.outputs, self.outstanding = [], [], {}, set()
        self.control_launches, self.control_common_override = [], None
        self.unsettled_after, self.busy = None, False
        self.helper = {'Custody': lambda _stopped: self.custody(), 'child_ids': lambda _pid: self.outstanding}
        self.owned, self.lease, self.lock_acquired = None, None, False
        self.total_deadline, self.initial_context, self.serial, self.outer_exit = None, None, 0, 1
        self.entries = driver.phases(self.plan['selectors'], self.paths)
        self.value = {'commands': [], 'metadata_commands': [], 'errors': [], 'qualification': False,
                      'phase_states': [{'label': entry['label'], 'state': 'UNRUN'} for entry in self.entries]}

    def custody(self):
        self.events.append('new-owned-family')
        return FakeCustody(self)

    def check_bindings(self):
        self.events.append('pinned-bindings')

    def acquire_lane(self):
        self.events.append('actual-C-flock')
        if self.busy:
            raise BlockingIOError('fixture C busy')
        self.lock_acquired = True
        self.value['lock_acquired'] = True

    def source_observation(self, stage, original=False):
        self.events.append('source:' + stage)

    def context_observation(self, stage):
        self.events.append('context:' + stage)

    def target_observation(self, stage):
        self.events.append('existing-target:' + stage)

    def layout_observation(self, stage):
        self.events.append('existing-layout:' + stage)


class DriverProtocolTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='host-driver-protocol-')
        self.addCleanup(self.directory.cleanup)
        self.runner = ProtocolRunner(self.directory.name)
        self.context_plan = copy.deepcopy(self.runner.plan)
        self.context = {'physical_tools': {name: {'sha256': '1' * 64, 'device_inode': [1, 2], 'physical': value['path']}
                         for name, value in self.runner.plan['tools'].items()}}
        self.elf = {'path': str(self.runner.paths.target / 'debug/deps/cagents-a12b'), 'device_inode': [1, 2], 'sha256': '3' * 64}
        for patch in (mock.patch.object(driver, 'STOP_REQUESTED', False),
                      mock.patch.object(driver, 'STOP_SIGNALS', []),
                      mock.patch.object(driver, 'context_guard'),
                      mock.patch.object(driver, 'context_snapshot', return_value=self.context),
                      mock.patch.object(driver, 'observe_elf', return_value=self.elf),
                      mock.patch('sys.stdout', new_callable=io.StringIO)):
            patch.start()
            self.addCleanup(patch.stop)

    def run_driver(self):
        self.runner.run()
        return [role for role, _argv, _cwd, _timeout in self.runner.launches]

    def helper_record(self):
        return next(record for record in self.runner.value['metadata_commands'] if record['label'] == 'prepare-canonical-checkout')

    def test_flock_and_prior_statement_gates_precede_staging_then_only_canonical_HOST_commands(self):
        with mock.patch.object(driver, 'prior_return_observation', wraps=driver.prior_return_observation) as prior:
            roles = self.run_driver()
        prior.assert_called_once()
        self.assertEqual(roles, ['prepare-canonical-checkout', 'host-binary-check',
                                'host-lib-test-compilation', 'same-ELF-complete-inventory', 'parent-01'])
        events = self.runner.events
        self.assertLess(events.index('new-owned-family'), events.index('actual-C-flock'))
        self.assertLess(events.index('actual-C-flock'), events.index('launch:prepare-canonical-checkout'))
        self.assertLess(events.index('source:original-before-staging'), events.index('launch:prepare-canonical-checkout'))
        stage = self.runner.launches[0]
        self.assertEqual(stage[1], [self.runner.paths.tools['python'], '-I', '-B', '/tmp/reviewed-checkout-helper',
            '--repository', str(self.runner.paths.repository), '--checkout', str(self.runner.paths.checkout), '--owner', driver.OWNER,
            '--head', 'a' * 40, '--tree', 'b' * 40, '--expect-current', 'absent'])
        self.assertEqual(stage[2:], (str(self.runner.paths.repository), 60))
        for role, argv, cwd, _timeout in self.runner.launches[1:]:
            self.assertEqual(cwd, str(self.runner.paths.checkout))
            if role in ('host-binary-check', 'host-lib-test-compilation'):
                self.assertIn('CARGO_TARGET_DIR=' + str(self.runner.paths.target), argv)
                self.assertIn('CARGO_BUILD_JOBS=2', argv)
                self.assertEqual(argv[argv.index('--features') + 1], 'host')
                self.assertIn('--no-default-features', argv)
            else:
                self.assertEqual(argv[0], self.elf['path'])
        self.assertEqual(self.runner.outer_exit, 0)
        self.assertEqual(self.runner.value['same_ELF_complete_inventory']['parent_passes'], 0)
        self.assertTrue(self.runner.value['commands'][-1]['test_observation']['qualification'])

    def test_invalid_prior_statement_stops_before_new_custody_lock_or_helper(self):
        value = positive_return()
        value['custody_final']['nodes'][0]['settled'] = False
        self.runner.prior_path.write_text(json.dumps(value))
        with self.assertRaisesRegex(RuntimeError, 'unknown settlement'):
            self.run_driver()
        self.assertEqual(self.runner.events, ['pinned-bindings'])
        self.assertEqual(self.runner.launches, [])

    def test_busy_C_has_no_helper_or_Cargo_launch(self):
        self.runner.busy = True
        with self.assertRaises(BlockingIOError):
            self.run_driver()
        self.assertEqual(self.runner.launches, [])
        self.assertNotIn('source:original-before-staging', self.runner.events)

    def test_control_common_mismatch_stops_before_helper_or_Cargo(self):
        self.runner.control_common_override = str(self.runner.work / 'independent-common')
        with self.assertRaisesRegex(RuntimeError, 'different common Git binding'):
            self.run_driver()
        self.assertEqual(len(self.runner.control_launches), 2)
        self.assertEqual(self.runner.launches, [])

    def test_prepare_cannot_bypass_actual_C_or_positive_return_gates(self):
        for lock_acquired, prior in [(False, positive_return()), (True, None)]:
            with self.subTest(lock_acquired=lock_acquired):
                self.runner.lock_acquired, self.runner.prior_return = lock_acquired, prior
                with self.assertRaisesRegex(RuntimeError, 'lacks actual C/prior positive return'):
                    self.runner.prepare_checkout()
                self.assertEqual(self.runner.launches, [])

    def test_prepare_also_requires_verified_control_common_provenance(self):
        self.runner.lock_acquired, self.runner.prior_return = True, positive_return()
        with self.assertRaisesRegex(RuntimeError, 'lacks exact control/common Git provenance'):
            self.runner.prepare_checkout()
        self.assertEqual(self.runner.launches, [])

    def test_helper_refusal_retains_original_bytes_and_mutation_flag_without_Cargo(self):
        raw = b'{"version":1,"status":"refused","mutation_may_have_occurred":true}'
        self.runner.outputs['prepare-canonical-checkout'] = result(raw, 2)
        with self.assertRaisesRegex(RuntimeError, 'helper refused'):
            self.run_driver()
        self.assertEqual(len(self.runner.launches), 1)
        record = self.helper_record()
        self.assertEqual(record['original_exit'], 2)
        self.assertEqual(Path(record['log']).read_bytes(), raw)
        self.assertNotIn('prepared_checkout', self.runner.value)

    def test_cleanup_zero_never_promotes_unknown_helper_original_result(self):
        raw = json.dumps(prepared(self.runner.plan['source'], self.runner.paths)).encode()
        self.runner.outputs['prepare-canonical-checkout'] = result(raw, None, 'timeout', 0)
        with self.assertRaisesRegex(RuntimeError, 'unknown original result'):
            self.run_driver()
        record = self.helper_record()
        self.assertIsNone(record['original_exit'])
        self.assertEqual(record['cleanup_exit'], 0)
        self.assertEqual(Path(record['incomplete_before_cleanup']['path']).read_bytes(), raw)
        self.assertEqual(len(self.runner.launches), 1)

    def test_successful_helper_with_unsettled_descendant_cannot_launch_Cargo(self):
        self.runner.unsettled_after = 'prepare-canonical-checkout'
        with self.assertRaisesRegex(RuntimeError, 'Owned descendants remain'):
            self.run_driver()
        self.assertEqual(len(self.runner.launches), 1)
        record = self.helper_record()
        self.assertEqual(record['original_exit'], 0)
        self.assertTrue(Path(record['log']).is_file())
        self.assertNotIn('prepared_checkout', self.runner.value)

    def test_prepared_wrong_previous_head_blocks_Cargo(self):
        value = prepared(self.runner.plan['source'], self.runner.paths)
        value['previous_head'] = '9' * 40
        self.runner.outputs['prepare-canonical-checkout'] = result(json.dumps(value).encode())
        with self.assertRaisesRegex(RuntimeError, 'Prepared checkout receipt differs'):
            self.run_driver()
        self.assertEqual(len(self.runner.launches), 1)

    def test_failed_original_check_stops_compilation_and_preserves_exit(self):
        self.runner.outputs['host-binary-check'] = result(b'original failure\n', 17)
        roles = self.run_driver()
        self.assertEqual(roles, ['prepare-canonical-checkout', 'host-binary-check'])
        self.assertEqual(self.runner.outer_exit, 17)
        self.assertFalse(self.runner.value['commands'][0]['qualification'])

    def test_complete_primary_budget_required_before_helper_launch(self):
        self.runner.plan['total_workload_seconds'] = 100
        with self.assertRaisesRegex(RuntimeError, 'Full phase budget'):
            self.run_driver()
        self.assertEqual(self.runner.launches, [])

    def test_second_candidate_keeps_same_checkout_common_anchor_target_and_exact_previous_pin(self):
        self.run_driver()
        second_directory = Path(self.directory.name) / 'second-evidence'
        second_directory.mkdir()
        previous_paths = self.runner.paths
        self.runner = ProtocolRunner(second_directory, head='4' * 40, previous='a' * 40)
        # Same admitted workspace/tool roles, new external evidence and source.
        self.runner.paths = previous_paths
        for key in ('workspace_root', 'control_repository', 'common_git_dir', 'identities', 'tools'):
            self.runner.plan[key] = self.context_plan[key]
        self.runner.entries = driver.phases(self.runner.plan['selectors'], self.runner.paths)
        self.run_driver()
        argv = self.runner.launches[0][1]
        self.assertEqual(argv[argv.index('--expect-current') + 1], 'a' * 40)
        staged = self.runner.value['prepared_checkout']
        self.assertEqual((staged['operation'], staged['previous_head'], staged['head']), ('updated', 'a' * 40, '4' * 40))
        self.assertEqual((staged['checkout'], staged['common_git_dir']), (str(self.runner.paths.checkout), str(self.runner.paths.common)))
        self.assertIn('CARGO_TARGET_DIR=' + str(self.runner.paths.target), self.runner.launches[1][1])

    def test_finish_retains_C_through_settlement_retry_and_pidfd_closure(self):
        self.runner.owned = FakeCustody(self.runner)
        self.runner.lease = 12345  # Mocked FD; never opened or closed by the fixture.
        self.runner.value['lock_acquired'] = True
        calls = 0
        original = self.runner.owned.settle_all
        def settle(observer, report):
            nonlocal calls
            calls += 1
            if calls == 1:
                self.runner.events.append('settlement-blocked-C-retained')
                raise RuntimeError('injected unknown settlement')
            return original(observer, report)
        self.runner.owned.settle_all = settle
        with mock.patch.object(driver.time, 'sleep'), mock.patch.object(driver.os, 'close',
                side_effect=lambda fd: self.runner.events.append('close-C:' + str(fd))) as close:
            self.runner.finish()
        close.assert_called_once_with(12345)
        events = self.runner.events
        self.assertLess(events.index('settlement-blocked-C-retained'), events.index('positive-own-family-settlement'))
        self.assertLess(events.index('positive-own-family-settlement'), events.index('own-pidfds-closed'))
        self.assertLess(events.index('own-pidfds-closed'), events.index('close-C:12345'))
        driver.prior_return_observation(self.runner.value)


class DriverRefusalTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='host-driver-binding-')
        self.addCleanup(self.directory.cleanup)
        self.paths = driver.HostPaths(plan(Path(self.directory.name) / 'evidence'))

    def test_positive_prior_return_accepts_failed_original_without_upgrading_it(self):
        statement = positive_return(23)
        statement['custody_final']['nodes'][0]['signals'] = [{'signal': 15}]
        observation = driver.prior_return_observation(statement)
        self.assertEqual(observation['original_exit_code'], 23)
        self.assertEqual(observation['custody_final']['nodes'][0]['signals'], [{'signal': 15}])
        self.assertTrue(observation['coordinator_statement_only'])

    def test_prior_positive_claim_needs_each_required_gate(self):
        for field, replacement in [('lock_acquired', False), ('all_owned_pidfds_closed', False),
                                   ('lease', 'flock appeared free'), ('state', 'exited')]:
            with self.subTest(field=field):
                statement = positive_return()
                statement[field] = replacement
                with self.assertRaises(RuntimeError):
                    driver.prior_return_observation(statement)

    def test_actual_lock_open_has_no_create_and_busy_fd_is_not_recorded_as_acquired(self):
        runner = object.__new__(driver.Runner)
        runner.paths = self.paths
        runner.paths.identities['lane_lock'] = [2, 3]
        runner.directory_observation = lambda _role, _path: None
        runner.value, runner.lock_acquired, runner.lease = {}, False, None
        info = SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=os.getuid(), st_nlink=1, st_dev=2, st_ino=3)
        with mock.patch.object(driver.os, 'open', return_value=12345) as opened, \
             mock.patch.object(driver.os, 'fstat', return_value=info), \
             mock.patch.object(driver.Path, 'resolve', return_value=runner.paths.lock), \
             mock.patch.object(driver.Path, 'lstat', return_value=info), \
             mock.patch.object(driver.fcntl, 'flock', side_effect=BlockingIOError) as flock:
            with self.assertRaises(BlockingIOError):
                runner.acquire_lane()
        self.assertEqual(opened.call_args.args, (runner.paths.lock, os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW))
        flock.assert_called_once_with(12345, driver.fcntl.LOCK_EX | driver.fcntl.LOCK_NB)
        self.assertFalse(runner.lock_acquired)
        self.assertNotIn('lock_acquired', runner.value)

    def test_named_lock_replacement_during_successful_flock_never_admits_staging(self):
        runner = object.__new__(driver.Runner)
        runner.paths = self.paths
        runner.paths.identities['lane_lock'] = [2, 3]
        runner.directory_observation = lambda _role, _path: None
        runner.value, runner.lock_acquired, runner.lease = {}, False, None
        original = SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=os.getuid(), st_nlink=1, st_dev=2, st_ino=3)
        replacement = SimpleNamespace(st_dev=2, st_ino=4)
        with mock.patch.object(driver.os, 'open', return_value=12345), \
             mock.patch.object(driver.os, 'fstat', return_value=original), \
             mock.patch.object(driver.Path, 'resolve', return_value=runner.paths.lock), \
             mock.patch.object(driver.Path, 'lstat', side_effect=[original, replacement]), \
             mock.patch.object(driver.fcntl, 'flock') as flock:
            with self.assertRaisesRegex(RuntimeError, 'replaced while acquiring'):
                runner.acquire_lane()
        flock.assert_called_once()
        self.assertFalse(runner.lock_acquired)
        self.assertNotIn('lock_acquired', runner.value)

    def test_prepared_receipt_binds_all_source_slot_owner_and_common_fields(self):
        original = source()
        valid = prepared(original, self.paths)
        self.assertEqual(driver.prepared_observation(json.dumps(valid).encode(), 0, 'finished', original, self.paths), valid)
        for field, replacement in [('checkout', '/tmp/foreign-slot'), ('repository', '/tmp/foreign-repo'),
                                   ('owner', 'new-owner'), ('common_git_dir', '/tmp/new-git'),
                                   ('head', '9' * 40), ('tree', '8' * 40), ('previous_head', '7' * 40),
                                   ('registry', '/tmp/new-registry'), ('operation', 'updated'),
                                   ('detached', False), ('clean', False), ('source_only', False), ('version', True)]:
            with self.subTest(field=field):
                changed = valid | {field: replacement}
                with self.assertRaises(RuntimeError):
                    driver.prepared_observation(json.dumps(changed).encode(), 0, 'finished', original, self.paths)

    def test_plan_cannot_supply_commands_union_features_new_tools_or_a_new_target(self):
        with tempfile.TemporaryDirectory(prefix='host-plan-source-') as directory:
            original = plan(Path(directory) / 'new-evidence')
            driver.validate_plan(original)
            for key, value in [('commands', [['sh', '-c', 'anything']]), ('features', ['host', 'ui']),
                               ('target', '/tmp/new-target'), ('tools', {'cargo': '/tmp/cargo'})]:
                with self.subTest(key=key), self.assertRaises(RuntimeError):
                    driver.validate_plan(original | {key: value})
            for key, value in [('expect_current', 'HEAD'), ('head', 'main')]:
                changed = copy.deepcopy(original)
                changed['source'][key] = value
                with self.subTest(key=key), self.assertRaises(RuntimeError):
                    driver.validate_plan(changed)

    def test_malformed_or_duplicate_JSON_is_not_an_admission_receipt(self):
        with self.assertRaises(RuntimeError):
            driver.json_bytes(b'{"version":1,"version":1}')
        with self.assertRaises(ValueError):
            driver.prepared_observation(b'{not JSON}', 0, 'finished', source(), self.paths)

    def test_Cargo_artifact_must_be_unique_canonical_isolated_HOST_and_successful(self):
        driver.select_host_artifact(cargo_output(self.paths), 0, self.paths)
        wrong_original = Path('/tmp/reviewed-original-source')
        for raw in [cargo_output(self.paths, checkout=wrong_original), cargo_output(self.paths, features=['host', 'ui']),
                    cargo_output(self.paths) + cargo_output(self.paths), cargo_output(self.paths) + b'{broken\n']:
            with self.subTest(raw=raw[:80]), self.assertRaises((RuntimeError, ValueError)):
                driver.select_host_artifact(raw, 0, self.paths)
        with self.assertRaises(RuntimeError):
            driver.select_host_artifact(cargo_output(self.paths), 1, self.paths)
        forbidden = {'reason': 'compiler-artifact', 'target': {'name': 'servo'}}
        with self.assertRaises(RuntimeError):
            driver.select_host_artifact(cargo_output(self.paths) + json.dumps(forbidden).encode(), 0, self.paths)

    def test_inventory_is_complete_same_ELF_presence_and_zero_parent_passes(self):
        raw = f'{SELECTOR}: test\n{OTHER}: test\n\n2 tests, 0 benchmarks\n'.encode()
        observation = driver.inventory_observation(raw, 0, [SELECTOR])
        self.assertEqual(observation['parent_passes'], 0)
        for changed in [raw.replace(b'2 tests', b'3 tests'), raw + b'unparsed trailing output\n',
                        raw.replace(OTHER.encode(), SELECTOR.encode()), raw.replace(SELECTOR.encode(), b'missing')]:
            with self.subTest(raw=changed[:70]), self.assertRaises(RuntimeError):
                driver.inventory_observation(changed, 0, [SELECTOR])

    def test_parent_raw_oracle_never_sums_nested_output_or_upgrades_nonzero_unknown(self):
        raw = parent_output()
        self.assertTrue(driver.test_observation(raw, 0, SELECTOR, 2)['qualification'])
        for changed, code in [(raw, 1), (raw, None), (raw + raw, 0), (raw, 0),
                              (raw + b'test nested ... ok\n', 0), (raw + b'\x1b[0m', 0)]:
            count = 3 if changed is raw and code == 0 else 2
            with self.subTest(code=code, size=len(changed), count=count):
                self.assertFalse(driver.test_observation(changed, code, SELECTOR, count)['qualification'])

    def test_inherited_authority_is_preserved_and_Git_routing_is_refused_by_name(self):
        environment = {'HOME': '/original/home', 'OPAQUE_AUTHORITY': 'fixture-opaque-value'}
        with mock.patch.dict(os.environ, environment, clear=True), \
             mock.patch.object(driver.signal, 'getsignal', return_value=driver.signal.SIG_DFL):
            driver.context_guard()
            self.assertEqual(dict(os.environ), environment)
            os.environ['GIT_DIR'] = '/fixture-only-routing'
            with self.assertRaisesRegex(RuntimeError, 'routing conflicts') as caught:
                driver.context_guard()
            self.assertNotIn('/fixture-only-routing', str(caught.exception))
            self.assertEqual(os.environ['OPAQUE_AUTHORITY'], environment['OPAQUE_AUTHORITY'])


class SourceWitnessTests(unittest.TestCase):
    """Real pinned-file reads and source method; only Git responses are injected."""
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='host-driver-source-witness-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / '.cargo').mkdir()
        (self.root / '.cargo/config.toml').write_text(
            '[build]\njobs = 2\n[env]\n'
            'CAGENTS_CHECKOUT_ROOT = { value = ".", relative = true, force = true }\n')
        (self.root / 'build.rs').write_text(
            'const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");\n'
            'fn main() { println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT"); }\n')
        self.source = source()
        self.source['original_checkout'] = str(self.root)
        self.source['sha256'] = {name: driver.sha(self.root / name) for name in ('.cargo/config.toml', 'build.rs')}
        self.runner = object.__new__(driver.Runner)
        self.paths = driver.HostPaths(plan(self.root / 'evidence'))
        self.runner.paths = self.paths
        self.runner.plan = {'source': self.source}
        self.runner.value = {}
        self.runner.save = lambda: None
        self.calls = []
        self.values = ['a' * 40, 'b' * 40, 'c' * 40, '', str(self.root), str(self.paths.common)]
        def captured(argv, label, seconds, cwd, metadata):
            self.calls.append((argv, label, seconds, cwd, metadata))
            return result((self.values[len(self.calls) - 1] + '\n').encode()), {}
        self.runner.captured = captured

    def test_original_source_retains_exact_common_head_tree_parent_clean_and_file_witness(self):
        self.runner.source_observation('original-fixture', original=True)
        observation = self.runner.value['source_observations'][0]
        self.assertTrue(observation['original_source_witness'])
        self.assertTrue(observation['exact_source'])
        self.assertEqual(observation['source_hashes'], self.source['sha256'])
        self.assertEqual(len(self.calls), 6)
        status = self.calls[3]
        self.assertEqual(status[0][-4:], ['status', '--porcelain=v1', '--untracked-files=all', '--ignored=matching'])
        self.assertTrue(all(call[3:] == (self.root, True) for call in self.calls))
        self.assertTrue(all('--no-replace-objects' in call[0] for call in self.calls))

    def test_changed_pinned_bytes_fail_before_any_Git_metadata_child(self):
        (self.root / 'build.rs').write_text('different body\n')
        with self.assertRaisesRegex(RuntimeError, 'source bytes changed'):
            self.runner.source_observation('changed-fixture', original=True)
        self.assertEqual(self.calls, [])

    def test_matching_hash_without_compile_time_anchor_still_fails(self):
        (self.root / 'build.rs').write_text('fn main() {}\n')
        self.source['sha256']['build.rs'] = driver.sha(self.root / 'build.rs')
        with self.assertRaisesRegex(RuntimeError, 'checkout-fidelity anchor missing'):
            self.runner.source_observation('anchor-fixture', original=True)
        self.assertEqual(self.calls, [])

    def test_legacy_Cargo_config_cannot_override_reviewed_anchor(self):
        (self.root / '.cargo/config').write_text('[build]\njobs = 99\n')
        with self.assertRaisesRegex(RuntimeError, 'fidelity anchor or existing jobs2'):
            self.runner.source_observation('legacy-config-fixture', original=True)
        self.assertEqual(self.calls, [])

    def test_dirty_or_foreign_common_metadata_cannot_admit_canonical_source(self):
        for index, changed in [(3, '!! ignored-output'), (5, '/tmp/independent-clone-git')]:
            with self.subTest(index=index):
                self.calls.clear()
                previous = self.values[index]
                self.values[index] = changed
                with self.assertRaisesRegex(RuntimeError, 'Exact clean source'):
                    self.runner.source_observation('metadata-fixture', original=True)
                self.values[index] = previous

    def test_canonical_checkout_must_be_detached_before_a_Cargo_phase(self):
        self.values.append('main')
        with mock.patch.object(self.paths, 'checkout', self.root):
            with self.assertRaisesRegex(RuntimeError, 'detachment mismatch'):
                self.runner.source_observation('attached-canonical-fixture')
        self.assertEqual(len(self.calls), 7)

    def test_file_binding_refuses_changed_or_hardlinked_synthetic_ELF_bytes(self):
        target = self.root / 'fixture-ELF-target'
        directory = target / 'debug/deps'
        directory.mkdir(parents=True)
        executable = directory / 'cagents-a123'
        executable.write_bytes(b'\x7fELFfixture synthetic bytes; never executed')
        executable.chmod(0o700)
        with mock.patch.object(self.paths, 'target', target):
            bound = driver.observe_elf(str(executable), self.paths)
            self.assertTrue(bound['ELF_magic'])
            executable.write_bytes(b'\x7fELFchanged fixture bytes; never executed')
            with self.assertRaisesRegex(RuntimeError, 'changed across phases'):
                driver.observe_elf(str(executable), self.paths, bound)
            os.link(executable, directory / 'extra-hardlink')
            with self.assertRaisesRegex(RuntimeError, 'owned ordinary executable'):
                driver.observe_elf(str(executable), self.paths)

    def test_existing_target_must_keep_its_directory_identity_without_creation(self):
        target = self.root / 'fixture-existing-target'
        self.runner.target_identity = None
        with mock.patch.object(self.paths, 'target', target):
            with self.assertRaises(FileNotFoundError):
                self.runner.target_observation('absent-target')
            self.assertFalse(target.exists())
            target.mkdir()
            self.paths.identities['lane_target'] = [target.stat().st_dev, target.stat().st_ino]
            self.runner.target_observation('existing-target')
            old_identity = self.runner.target_identity
            target.rename(self.root / 'retained-old-target')
            target.mkdir()
            self.assertNotEqual([target.stat().st_dev, target.stat().st_ino], old_identity)
            with self.assertRaisesRegex(RuntimeError, 'directory identity changed'):
                self.runner.target_observation('replaced-target')


class PortableBindingTests(unittest.TestCase):
    """Owned alternate directories; no actual flock, Git or tool execution."""
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='host-alternate-workspace-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.plan = plan(self.root / 'new-evidence', workspace=self.root / 'alternate-owned-workspace')
        paths = driver.HostPaths(self.plan)
        paths.workspace.mkdir(mode=0o700)
        (paths.workspace / '.worktrees').mkdir(mode=0o700)
        for role, path in paths.role_paths().items():
            if role == 'lane_lock':
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                path.write_text('owned offline fixture; never flocked')
                path.chmod(0o600)
            else:
                path.mkdir(mode=0o700, parents=True, exist_ok=True)
            info = path.stat()
            self.plan['identities'][role] = [info.st_dev, info.st_ino]
        for role in driver.TOOL_ROLES:
            path = self.root / 'reviewed-physical-tools' / role
            path.parent.mkdir(mode=0o700, exist_ok=True)
            path.write_bytes(('offline synthetic tool bytes for role ' + role).encode())
            path.chmod(0o700)
            info = path.stat()
            self.plan['tools'][role] = {'path': str(path), 'sha256': driver.sha(path), 'device_inode': [info.st_dev, info.st_ino]}
        self.paths = driver.validate_plan(self.plan)
        self.runner = object.__new__(driver.Runner)
        self.runner.paths, self.runner.plan = self.paths, self.plan
        self.runner.value, self.runner.target_identity, self.runner.lease = {}, None, 12345
        self.runner.save = lambda: None

    def test_alternate_owned_workspace_uses_exact_reviewed_C_layout_common_and_tool_roles(self):
        lock_info = self.paths.lock.stat()
        with mock.patch.object(driver.os, 'fstat', return_value=lock_info):
            self.runner.layout_observation('alternate-owned-fixture')
        self.runner.target_observation('alternate-owned-target')
        observed = {'physical_tools': {role: {'physical': binding['path'], 'sha256': driver.sha(binding['path']),
                    'device_inode': [Path(binding['path']).stat().st_dev, Path(binding['path']).stat().st_ino]}
                    for role, binding in self.plan['tools'].items()}}
        self.runner.verify_tools(observed)
        checkout = self.root / 'alternate-owned-workspace/.worktrees/validation-three-lanes/lane-c-checkout'
        self.assertEqual(self.paths.checkout, checkout)
        self.assertEqual(self.paths.target, checkout.parent / 'lane-c-target')
        self.assertEqual(self.paths.lock, checkout.parent / 'lane-c.lock')
        prepared_value = prepared(self.plan['source'], self.paths)
        self.assertEqual(driver.prepared_observation(json.dumps(prepared_value).encode(), 0, 'finished',
                         self.plan['source'], self.paths), prepared_value)
        commands = driver.phases([SELECTOR], self.paths)
        self.assertEqual(commands[0]['argv'][0], self.plan['tools']['env']['path'])
        self.assertIn(self.plan['tools']['cargo']['path'], commands[0]['argv'])
        self.assertIn('CARGO_TARGET_DIR=' + str(checkout.parent / 'lane-c-target'), commands[0]['argv'])
        self.assertIn('CARGO_BUILD_JOBS=2', commands[0]['argv'])

    def test_wrong_existing_C_or_common_identity_is_refused_in_owned_alternate_workspace(self):
        for role in ('lane_target', 'common_git_dir'):
            with self.subTest(role=role):
                expected = self.paths.identities[role]
                self.paths.identities[role] = [expected[0], expected[1] + 1]
                with self.assertRaisesRegex(RuntimeError, 'directory identity changed'):
                    self.runner.directory_observation(role, self.paths.role_paths()[role])
                self.paths.identities[role] = expected

    def test_plan_cannot_retarget_C_layout_or_reuse_another_workspace_identity(self):
        for key in ('lane_directory', 'lane_lock', 'lane_target', 'canonical_checkout', 'owner'):
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                driver.validate_plan(self.plan | {key: str(self.root / 'alternate-target')})
        other = self.root / 'another-owned-workspace'
        other.mkdir(mode=0o700)
        changed = self.plan | {'workspace_root': str(other), 'control_repository': str(other / 'control')}
        other_paths = driver.HostPaths(changed)
        self.runner.paths = other_paths
        with self.assertRaisesRegex(RuntimeError, 'workspace_root directory identity changed'):
            self.runner.directory_observation('workspace_root', other)

    def test_wrong_physical_tool_identity_cannot_be_relabelled_as_the_reviewed_role(self):
        observed = {'physical_tools': {role: {'physical': binding['path'], 'sha256': binding['sha256'],
                    'device_inode': binding['device_inode']} for role, binding in self.plan['tools'].items()}}
        observed['physical_tools']['cargo']['device_inode'] = [1, 2]
        with self.assertRaisesRegex(RuntimeError, 'reviewed path/bytes/identity'):
            self.runner.verify_tools(observed)


if __name__ == '__main__':
    unittest.main()
