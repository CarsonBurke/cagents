#!/usr/bin/python3
"""Supervised fw16-only libc fixture checks; compiles C, never Rust or the app."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parent / 'fixtures/rehearsal-startup-bind.c'
PROBE = 'import socket,sys;s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);print("bound")'


class StartupBind(unittest.TestCase):
    def test_same_elf_and_library_refuse_armed_then_bind_disarmed(self):
        with tempfile.TemporaryDirectory(prefix='ru-bind-', dir='/var/tmp') as temporary:
            root = Path(temporary)
            (root / 'logs').mkdir()
            (root / '.fixture').mkdir()
            (root / 'rt/cagents').mkdir(parents=True)
            (root / '.rehearse-update').write_text(str(root) + '\n')
            candidate, source = root / 'candidate/cagentsd', root / 'source/cagentsd'
            for executable in (candidate, source):
                executable.parent.mkdir()
                shutil.copy2(Path(sys.executable).resolve(), executable)
            library = root / '.fixture/startup-bind.so'
            subprocess.run(['cc', '-shared', '-fPIC', '-Wall', '-Wextra', '-Werror',
                            '-DRETRY_ROOT=' + json.dumps(str(root)), '-DRETRY_CANDIDATE=' + json.dumps(str(candidate)),
                            SOURCE, '-o', library, '-ldl'], check=True)
            sentinel = root / '.fixture/fail-new-daemon.armed'
            sentinel.write_text(str(candidate) + '\n')
            address = root / 'rt/cagents/cagentsd.sock'
            digests = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in (candidate, library)}
            environment = {**os.environ, 'LD_PRELOAD': str(library)}
            def probe(executable, path):
                return subprocess.run([executable, '-c', PROBE, path], env=environment, capture_output=True, text=True)
            for _ in range(2):
                refused = probe(candidate, address)
                self.assertNotEqual(refused.returncode, 0)
                self.assertIn('[Errno 98]', refused.stderr)
                self.assertFalse(address.exists())
            other = root / 'unrelated.sock'
            self.assertEqual(probe(candidate, other).returncode, 0)
            other.unlink()
            self.assertEqual(probe(source, address).returncode, 0)
            address.unlink()
            sentinel.rename(sentinel.with_suffix('.disarmed'))
            accepted = probe(candidate, address)
            self.assertEqual((accepted.returncode, accepted.stdout), (0, 'bound\n'))
            self.assertEqual(digests, {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in digests})
            decisions = [line.split('\t') for line in (root / 'logs/startup-bind.tsv').read_text().splitlines()]
            serving = [row for row in decisions if row[2] == str(address)]
            self.assertEqual([row[3] for row in serving], ['fail-armed', 'fail-armed', 'pass-source', 'pass-disarmed'])
            self.assertEqual([row[1] for row in serving], [str(candidate), str(candidate), str(source), str(candidate)])
            self.assertTrue(all(int(row[0]) > 1 for row in decisions))


if __name__ == '__main__':
    if socket.gethostname().split('.')[0] != 'fw16':
        raise SystemExit('Run this C fixture check only through the supervised genuine fw16 shell.')
    if any(name in os.environ for name in ('CAGENTS_AGENT_AUTHORITY', 'CAGENTS_AGENT_TOKEN', 'CAGENTS_SESSION_TOKEN')):
        raise SystemExit('Run from a genuine user shell; inherited agent authority was not modified.')
    unittest.main()
