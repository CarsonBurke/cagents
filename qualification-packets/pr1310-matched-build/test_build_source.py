"""AUTHORED UNRUN source oracles, synthetic parser data, no artifact/native proof.

Future pure invocation selects exact actual source AST only. This preparation
never imports the producer or calls a function/case/helper/orchestration method.
"""
import ast
import hashlib
import inspect
import json
from pathlib import Path
import re
from types import SimpleNamespace
import unittest

EXPECTED_SOURCE_SHA256 = '935fb054ac718f73cf5ed27e0c8d467b86df434846419d28dea02d10e1775c76'


def selected_source():
    raw = Path(__file__).with_name('run-arm.py').read_bytes()
    if hashlib.sha256(raw).hexdigest() != EXPECTED_SOURCE_SHA256:
        raise AssertionError('Owning source changed; a fresh source review is required')
    tree = ast.parse(raw)
    constants = {'TARGET', 'LOCK', 'TOOL', 'ZIG_VERSION', 'ORIGINAL_COMMAND_BINDING', 'MAX_LOG', 'MAX_CONFIG', 'GRAPHS'}
    functions = {'require', 'unique_json', 'commands', 'zig_environment', 'cargo_artifact', 'original_output', 'require_original_owner'}
    selected = []
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name) and node.targets[0].id in constants:
            selected.append(node)
        elif isinstance(node, ast.ClassDef) and node.name == 'Hold':
            selected.append(node)
        elif isinstance(node, ast.FunctionDef) and node.name in functions:
            selected.append(node)
    namespace = {'Path': Path, 'json': json, 'hashlib': hashlib, 're': re, 'inspect': inspect}
    exec(compile(ast.Module(body=selected, type_ignores=[]), '<actual-matched-build-pure-source>', 'exec'), namespace)
    return namespace


class MatchedBuildSourceOracles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.actual = selected_source()

    def setUp(self):
        self.hold = self.actual['Hold']
        self.checkout = Path('/tmp/pure-synthetic-source')
        self.graph = self.actual['commands']()[2]

    def artifact(self):
        executable = str(self.actual['TARGET'] / 'dev-runtime' / 'cagents-ui')
        return {'reason': 'compiler-artifact', 'package_id': 'path+file:///tmp/pure-synthetic-source#cagents@0.1.3',
                'manifest_path': '/tmp/pure-synthetic-source/Cargo.toml',
                'target': {'kind': ['bin'], 'crate_types': ['bin'], 'name': 'cagents-ui',
                           'src_path': '/tmp/pure-synthetic-source/src/bin/cagents-ui.rs'},
                'features': ['development', 'ui'], 'profile': {'opt_level': '0', 'debuginfo': 1,
                    'debug_assertions': True, 'overflow_checks': True, 'test': False},
                'executable': executable, 'filenames': [executable], 'fresh': False}

    def stream(self, *rows):
        return b''.join((row if isinstance(row, str) else json.dumps(row)).encode() + b'\n' for row in rows)

    def test_exact_development_build_graphs_remain_separate(self):
        rows = self.actual['commands']()
        self.assertEqual([(row['role'], row['name'], row['features'], row['seconds']) for row in rows],
                         [('cli', 'cagents', ['development'], 600),
                          ('host', 'cagentsd', ['development', 'host'], 600),
                          ('ui', 'cagents-ui', ['development', 'ui'], 900)])
        for row in rows:
            self.assertEqual(row['argv'][1:7], ['build', '--locked', '--profile', 'dev-runtime', '--no-default-features', '--features'])
            self.assertEqual(row['argv'][-1], '--message-format=json')
            self.assertNotIn('integrated-browser', row['argv'])

    def test_combined_progress_cache_and_unique_actual_bin_are_preserved(self):
        raw = self.stream('   Compiling cagents v0.1.3', self.artifact(),
                          {'reason': 'build-finished', 'success': True}, 'warning: synthetic parser fixture')
        value = self.actual['cargo_artifact'](raw, self.checkout, self.graph)
        self.assertEqual(value['stream'], 'ORIGINAL_COMBINED_STDOUT_STDERR')
        self.assertEqual(value['unparsed_human_line_numbers'], [1, 4])
        self.assertFalse(value['fresh'])
        self.assertEqual(value['raw_sha256'], hashlib.sha256(raw).hexdigest())
        self.assertTrue(value['profile_name_not_present_in_Cargo_JSON'])
        with self.assertRaises(self.hold):
            self.actual['cargo_artifact'](self.stream(self.artifact(), self.artifact(),
                {'reason': 'build-finished', 'success': True}), self.checkout, self.graph)

    def test_wrong_source_profile_feature_and_completion_refuse(self):
        for key, wrong in [('manifest_path', '/tmp/other/Cargo.toml'),
                           ('package_id', 'path+file:///tmp/other#cagents@0.1.3'),
                           ('features', ['development', 'host', 'ui']), ('executable', '/tmp/wrong'),
                           ('profile', {'test': True}), ('fresh', 1)]:
            row = self.artifact(); row[key] = wrong
            with self.subTest(key=key), self.assertRaises(self.hold):
                self.actual['cargo_artifact'](self.stream(row, {'reason': 'build-finished', 'success': True}), self.checkout, self.graph)
        for finish in [[], [{'reason': 'build-finished', 'success': False}],
                       [{'reason': 'build-finished', 'success': True}] * 2]:
            with self.subTest(finish=finish), self.assertRaises(self.hold):
                self.actual['cargo_artifact'](self.stream(self.artifact(), *finish), self.checkout, self.graph)
        for contrary in ['{"reason":"build-finished","success":true,"success":false}',
                         '{ malformed complete JSON }', 'error[E0000]: contrary diagnostic',
                         {'reason': 'compiler-message', 'message': {'level': 'error'}}]:
            with self.subTest(contrary=contrary), self.assertRaises((self.hold, ValueError)):
                self.actual['cargo_artifact'](self.stream(self.artifact(), contrary,
                    {'reason': 'build-finished', 'success': True}), self.checkout, self.graph)
        raw = self.stream(self.artifact(), {'reason': 'build-finished', 'success': True})
        with self.assertRaises(self.hold):
            self.actual['cargo_artifact'](raw[:-1], self.checkout, self.graph)

    def test_zig_reports_exact_selected_install_and_pinned_version(self):
        raw = b'{\n "zig_exe": "/tmp/pure/zig",\n "version": "0.15.2"\n}\n'
        value = self.actual['zig_environment'](raw, Path('/tmp/pure/zig'))
        self.assertEqual(value['reported_version'], '0.15.2')
        zon = b'.{\n .zig_exe = "/tmp/pure/zig",\n .version = "0.15.2",\n}\n'
        self.assertEqual(self.actual['zig_environment'](zon, Path('/tmp/pure/zig'))['reported_executable'], '/tmp/pure/zig')
        for bad in [raw.replace(b'0.15.2', b'0.14.1'), raw.replace(b'/tmp/pure/zig', b'/tmp/wrong/zig'),
                    raw + b' "version": "0.15.2"\n', b'{}\n']:
            with self.subTest(raw=bad), self.assertRaises(self.hold):
                self.actual['zig_environment'](bad, Path('/tmp/pure/zig'))

    def test_missing_original_owner_and_contrary_original_status_refuse(self):
        def forbidden(*_args):
            raise AssertionError('Missing-boundary oracle must not call supplied functions')
        with self.assertRaisesRegex(self.hold, 'BINDING_UNSUPPLIED'):
            self.actual['require_original_owner'](forbidden, forbidden, forbidden)
        raw = b'actual bytes in SYNTHETIC parser fixture only\n'
        result = SimpleNamespace(state='finished', original_exit=0, cleanup_exit=None, stdout=raw)
        record = dict(state='finished', original_exit=0, cleanup_exit=None, stdout_complete=True,
                      unforced_own_family=True, stdout_bytes=len(raw), log_sha256=hashlib.sha256(raw).hexdigest())
        self.assertEqual(self.actual['original_output'](result, record), raw)
        for field, bad in [('original_exit', None), ('original_exit', 101), ('cleanup_exit', 0), ('state', 'timeout')]:
            values = vars(result).copy(); values[field] = bad
            with self.subTest(field=field, bad=bad), self.assertRaises(self.hold):
                self.actual['original_output'](SimpleNamespace(**values), record)
        for field, bad in [('unforced_own_family', False), ('stdout_complete', False), ('log_sha256', '0' * 64)]:
            values = record.copy(); values[field] = bad
            with self.subTest(field=field), self.assertRaises(self.hold):
                self.actual['original_output'](result, values)


if __name__ == '__main__':
    unittest.main()
