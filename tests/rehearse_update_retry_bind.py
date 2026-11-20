#!/usr/bin/python3
"""The update retry's startup bind fixture: a small C library, never Rust or the app.

Compiling it needs only a C compiler; the library is built and preloaded in a
private temporary root, as the retry rehearsal builds it in its own.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parent / 'fixtures/rehearsal-startup-bind.c'
PROBE = 'import socket,sys;s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);print("bound")'


@unittest.skipUnless(shutil.which('cc'), 'the startup bind fixture needs a C compiler, cc')
class StartupBind(unittest.TestCase):
    def fixture(self, marked=True):
        directory = tempfile.TemporaryDirectory(prefix='ru-bind-', dir='/var/tmp')
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / 'logs').mkdir()
        (root / '.fixture').mkdir()
        (root / 'rt/cagents').mkdir(parents=True)
        (root / '.rehearse-update').write_text(str(root) + '\n')
        if marked:
            (root / '.cagents-fixture-root').touch()
        candidate, source = root / 'candidate/cagentsd', root / 'source/cagentsd'
        for executable in (candidate, source):
            executable.parent.mkdir()
            shutil.copy2(Path(sys.executable).resolve(), executable)
        library = root / '.fixture/startup-bind.so'
        subprocess.run(['cc', '-shared', '-fPIC', '-Wall', '-Wextra', '-Werror',
                        '-DRETRY_ROOT=' + json.dumps(str(root)), '-DRETRY_CANDIDATE=' + json.dumps(str(candidate)),
                        SOURCE, '-o', library, '-ldl'], check=True)
        return root, candidate, source, library

    def probe(self, executable, path, library, *, home=None):
        environment = {**os.environ, 'LD_PRELOAD': str(library)}
        if home is None:
            environment.pop('HOME', None)
        else:
            environment['HOME'] = str(home)
        return subprocess.run([executable, '-B', '-c', PROBE, path], env=environment,
                              capture_output=True, text=True)

    def snapshot(self, root):
        return {
            str(path.relative_to(root)): (
                'directory' if path.is_dir() else 'file',
                path.stat().st_mode,
                path.stat().st_mtime_ns,
                None if path.is_dir() else hashlib.sha256(path.read_bytes()).hexdigest(),
            )
            for path in (root, *sorted(root.rglob('*')))
        }

    def test_refuses_unmarked_compiled_home_without_writes(self):
        root, candidate, _, library = self.fixture(marked=False)
        address = root / 'rt/cagents/cagentsd.sock'
        (root / 'real-looking-account/.claude').mkdir(parents=True)
        before = self.snapshot(root)
        # The compiled root supplies HOME explicitly; an environment fallback
        # must never authorize a root without the positive marker.
        for home in (root / 'real-looking-account', None):
            with self.subTest(home=home):
                refused = self.probe(candidate, address, library, home=home)
                self.assertNotEqual(refused.returncode, 0)
                self.assertEqual(refused.stderr.splitlines(), [
                    f'fixture-home: refusing HOME={root.resolve()}: not inside a marked fixture home',
                ])
                self.assertEqual(refused.stdout, '')
                self.assertEqual(self.snapshot(root), before)
        (root / '.cagents-fixture-root').touch()
        accepted = self.probe(candidate, address, library)
        self.assertEqual((accepted.returncode, accepted.stdout), (0, 'bound\n'), accepted.stderr)
        self.assertTrue((root / 'logs/startup-bind.tsv').is_file())

    def test_same_elf_and_library_refuse_armed_then_bind_disarmed(self):
        root, candidate, source, library = self.fixture()
        sentinel = root / '.fixture/fail-new-daemon.armed'
        sentinel.write_text(str(candidate) + '\n')
        address = root / 'rt/cagents/cagentsd.sock'
        digests = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in (candidate, library)}
        for _ in range(2):
            refused = self.probe(candidate, address, library)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn('[Errno 98]', refused.stderr)
            self.assertFalse(address.exists())
        other = root / 'unrelated.sock'
        self.assertEqual(self.probe(candidate, other, library).returncode, 0)
        other.unlink()
        self.assertEqual(self.probe(source, address, library).returncode, 0)
        address.unlink()
        sentinel.rename(sentinel.with_suffix('.disarmed'))
        accepted = self.probe(candidate, address, library)
        self.assertEqual((accepted.returncode, accepted.stdout), (0, 'bound\n'))
        self.assertEqual(digests, {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in digests})
        decisions = [line.split('\t') for line in (root / 'logs/startup-bind.tsv').read_text().splitlines()]
        serving = [row for row in decisions if row[2] == str(address)]
        self.assertEqual([row[3] for row in serving], ['fail-armed', 'fail-armed', 'pass-source', 'pass-disarmed'])
        self.assertEqual([row[1] for row in serving], [str(candidate), str(candidate), str(source), str(candidate)])
        self.assertTrue(all(int(row[0]) > 1 for row in decisions))


if __name__ == '__main__':
    unittest.main()
