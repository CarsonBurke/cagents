#!/usr/bin/env python3
"""Profile-route preparation contracts; no profiler/application launch."""
import hashlib
import importlib.util
import json
import os
import select
import signal
import subprocess
import sys
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


class ProfileOwnership(unittest.TestCase):
    def test_worker_thread_spawn_is_included_in_child_census(self):
        # A real disposable process demonstrates Linux's per-thread children
        # semantics without launching an application, provider, or profiler.
        code = """import json, subprocess, sys, threading
stop = threading.Event()
def worker():
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
    print(json.dumps({'child': child.pid, 'thread': threading.get_native_id()}), flush=True)
    try:
        stop.wait(20)
    finally:
        child.terminate()
        child.wait(timeout=3)
t = threading.Thread(target=worker)
t.start()
sys.stdin.readline()
stop.set()
t.join()
"""
        parent = subprocess.Popen([sys.executable, '-u', '-c', code], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True, start_new_session=True)
        try:
            ready, _, _ = select.select([parent.stdout], [], [], 5)
            self.assertTrue(ready, 'worker child did not become ready')
            spawned = json.loads(parent.stdout.readline())
            process = Path('/proc') / str(parent.pid)
            main_children = (process / 'task' / str(parent.pid) / 'children').read_text().split()
            self.assertNotIn(str(spawned['child']), main_children)
            owner = {'pid': parent.pid}
            native = types.SimpleNamespace(current=lambda value: parent.poll() is None)
            self.assertIn(spawned['child'], MODULE.all_thread_children(process, owner, native))
            parent.stdin.write('stop\n')
            parent.stdin.flush()
            self.assertEqual(parent.wait(timeout=5), 0)
            self.assertFalse(Path('/proc', str(spawned['child'])).exists())
        finally:
            if parent.poll() is None:
                os.killpg(parent.pid, signal.SIGKILL)
                parent.wait(timeout=5)
            parent.stdin.close()
            parent.stdout.close()

    def test_native_close_requires_worker_success_independent_of_driver(self):
        fake_common = types.ModuleType('common')
        for name in ('atomic_json', 'current', 'identity', 'load_config', 'monotonic_stamp',
                     'private_environment', 'require_build_admission', 'run_owned_command',
                     'verify_manifest', 'verify_source'):
            setattr(fake_common, name, lambda *args: None)
        holder_spec = importlib.util.spec_from_file_location('heap_holder',
            Path(__file__).resolve().parents[1] / 'scripts/hold_chat_memory_profile.py')
        holder = importlib.util.module_from_spec(holder_spec)
        with patch.dict(sys.modules, {'common': fake_common, 'chat_memory_heap': MODULE}):
            holder_spec.loader.exec_module(holder)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = root / 'quit.json'
            response = request.with_suffix('.response')
            cfg = {'_case': root}
            with patch.object(holder, 'action', return_value=0), patch.object(holder, 'atomic_json') as record:
                with self.assertRaisesRegex(RuntimeError, 'no bounded response'):
                    holder.request_quit(cfg, request)
                response.write_text(json.dumps({'exit_status': 1, 'stderr': 'native action failed'}))
                with self.assertRaisesRegex(RuntimeError, 'window-close action failed'):
                    holder.request_quit(cfg, request)
                self.assertFalse(record.call_args.args[1]['accepted'])
                response.write_text(json.dumps({'exit_status': 0}))
                holder.request_quit(cfg, request)
                self.assertTrue(record.call_args.args[1]['accepted'])
            with patch.object(holder, 'action', return_value=1), patch.object(holder, 'atomic_json'):
                with self.assertRaisesRegex(RuntimeError, 'window-close action failed'):
                    holder.request_quit(cfg, request)


if __name__ == '__main__':
    unittest.main()
