#!/usr/bin/env python3
"""Profile-route preparation contracts; no profiler/application launch."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('heap_route', Path(__file__).resolve().parents[1] / 'scripts/chat_memory_heap.py')
MODULE = importlib.util.module_from_spec(spec)
spec.loader.exec_module(MODULE)


class ProfileRoute(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.root.chmod(0o700)
        bundle = self.root / 'bundle'
        (bundle / 'bin').mkdir(parents=True)
        executable = bundle / 'bin/cagents-ui'
        executable.write_text('not executed')
        self.cfg = {'_case': self.root, '_bundle': bundle, 'source_sha': 'a' * 40,
                    'native_owner': {'pid': 1, 'start': 'fake'},
                    'heap_profile': {'purpose': 'heap-attribution', 'maximum_seconds': 90}}
        self.manifest = {'source_sha': 'a' * 40, 'profile': 'dev-runtime',
            'products': {'cagents-ui': {'path': str(executable), 'sha256': 'not-a-product-claim'}}}
        self.native = types.SimpleNamespace(require_build_admission=lambda cfg: None,
            require_profile_lifetime_admission=lambda cfg: None,
            private_environment=lambda cfg: None, current=lambda identity: True,
            sha256=lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest(),
            atomic_json=lambda path, value: Path(path).write_text(json.dumps(value)))
        self.addCleanup(patch.stopall)
        patch.object(MODULE, 'EVIDENCE', self.root).start()
        patch.object(MODULE, 'common', return_value=self.native).start()

    def test_preparation_pins_tools_without_launching_anything(self):
        with patch.object(MODULE.subprocess, 'Popen', side_effect=AssertionError('unexpected launch')):
            env = MODULE.prepare_environment(self.cfg, self.manifest, {'XDG_DATA_HOME': str(self.root / 'data')})
        prepared = json.loads((self.root / 'heap-profile/prepared.json').read_text())
        self.assertEqual(prepared['status'], 'prepared_not_executed')
        self.assertFalse(prepared['native_validation'])
        self.assertEqual(env['DUMP_HEAPTRACK_OUTPUT'], str(self.root / 'heap-profile/heaptrack.raw'))
        self.assertEqual(env['LD_PRELOAD'], prepared['tools']['preload']['path'])
        self.assertEqual((self.root / 'heap-profile/heaptrack.raw').stat().st_mode & 0o777, 0o600)
        self.assertTrue(all(len(tool['sha256']) == 64 for tool in prepared['tools'].values()))
        self.assertEqual(MODULE.monitor(self.cfg, {'pid': 123, 'start': 'fake'})['raw_bytes'], 0)

    def test_admission_failure_precedes_all_artifacts(self):
        self.native.require_build_admission = lambda cfg: (_ for _ in ()).throw(RuntimeError('not admitted'))
        with self.assertRaisesRegex(RuntimeError, 'not admitted'):
            MODULE.prepare_environment(self.cfg, self.manifest, {})
        self.assertFalse((self.root / 'heap-profile').exists())

    def test_stock_short_handoff_domain_cannot_start_a_profile(self):
        del self.native.require_profile_lifetime_admission
        with self.assertRaisesRegex(RuntimeError, 'stock r5 is insufficient'):
            MODULE.prepare_environment(self.cfg, self.manifest, {})
        self.assertFalse((self.root / 'heap-profile').exists())

    def test_no_inherited_injector_or_unbounded_profile(self):
        for environment in ({'LD_PRELOAD': 'old'}, {'DUMP_HEAPTRACK_OUTPUT': '/elsewhere'}):
            with self.assertRaisesRegex(RuntimeError, 'inherited'):
                MODULE.prepare_environment(self.cfg, self.manifest, environment)
        self.cfg['heap_profile']['maximum_seconds'] = 121
        with self.assertRaisesRegex(RuntimeError, 'bounded'):
            MODULE.prepare_environment(self.cfg, self.manifest, {})
        self.assertFalse((self.root / 'heap-profile').exists())

    def test_source_mismatch_or_expired_owner_never_qualifies(self):
        self.manifest['source_sha'] = 'b' * 40
        with self.assertRaisesRegex(RuntimeError, 'source-bound'):
            MODULE.prepare_environment(self.cfg, self.manifest, {})
        self.manifest['source_sha'] = self.cfg['source_sha']
        MODULE.prepare_environment(self.cfg, self.manifest, {})
        self.native.current = lambda identity: False
        with self.assertRaisesRegex(RuntimeError, 'exact native owner'):
            MODULE.monitor(self.cfg, {'pid': 123, 'start': 'fake'})


if __name__ == '__main__':
    unittest.main()
