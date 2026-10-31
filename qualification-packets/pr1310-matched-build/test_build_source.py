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

EXPECTED_SOURCE_SHA256 = 'f19275176ea578ce0efe0b92b7f2bc47180a4b5b388e9c2ae275f9a6ca8e3341'


def selected_source():
    raw = Path(__file__).with_name('run-arm.py').read_bytes()
    if hashlib.sha256(raw).hexdigest() != EXPECTED_SOURCE_SHA256:
        raise AssertionError('Owning source changed; a fresh source review is required')
    tree = ast.parse(raw)
    constants = {'ARMS', 'TARGET', 'LOCK', 'TOOL', 'ZIG_VERSION', 'ORIGINAL_COMMAND_BINDING', 'MAX_LOG', 'MAX_CONFIG', 'MAX_ELF', 'GRAPHS'}
    functions = {'require', 'unique_json', 'commands', 'zig_environment', 'cargo_artifact', 'original_output', 'require_original_owner', 'runtime_match_mapping'}
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

    def test_cli_root_library_sibling_does_not_veto_actual_bin(self):
        cli = self.actual['commands']()[0]
        binary = self.artifact()
        executable = str(self.actual['TARGET'] / 'dev-runtime' / 'cagents')
        binary.update(features=['development'], executable=executable, filenames=[executable],
                      target={'kind': ['bin'], 'crate_types': ['bin'], 'name': 'cagents',
                              'src_path': '/tmp/pure-synthetic-source/src/main.rs'})
        library = {**binary, 'target': {'kind': ['lib'], 'crate_types': ['lib'], 'name': 'cagents',
                                       'src_path': '/tmp/pure-synthetic-source/src/lib.rs'},
                   'executable': None, 'filenames': [str(self.actual['TARGET'] / 'dev-runtime' / 'libcagents.rlib')]}
        finish = {'reason': 'build-finished', 'success': True}
        for label, rows, bin_line in [('lib-before-bin', (library, binary), 3),
                                      ('lib-after-bin', (binary, library), 2)]:
            with self.subTest(order=label):
                raw = self.stream('   Compiling cagents v0.1.3', *rows, finish)
                selected = self.actual['cargo_artifact'](raw, self.checkout, cli)
                self.assertEqual(selected['target']['kind'], ['bin'])
                self.assertEqual(selected['target']['name'], 'cagents')
                self.assertEqual(selected['features'], ['development'])
                self.assertEqual(selected['executable'], executable)
                self.assertEqual(selected['line'], bin_line)
                self.assertEqual(selected['raw_sha256'], hashlib.sha256(raw).hexdigest())
                self.assertEqual(selected['stream'], 'ORIGINAL_COMBINED_STDOUT_STDERR')
        with self.assertRaises(self.hold):
            self.actual['cargo_artifact'](self.stream(library, finish), self.checkout, cli)
        with self.assertRaises(self.hold):
            self.actual['cargo_artifact'](self.stream(library, binary, binary, finish), self.checkout, cli)

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


    def mapping_artifacts(self):
        # Synthetic retained-writer fixtures only; never actual ELF/UID/files.
        root = Path('/tmp/cagents-history-build-11111111-2222-4333-8444-555555555555')
        rows = []
        for index, graph in enumerate(self.actual['commands']()):
            executable = str(self.actual['TARGET'] / 'dev-runtime' / graph['name'])
            cargo = self.artifact()
            cargo.update(target={'kind': ['bin'], 'crate_types': ['bin'], 'name': graph['name'],
                                 'src_path': str(self.checkout / graph['source'])},
                         features=graph['features'], executable=executable, filenames=[executable])
            selected = self.actual['cargo_artifact'](self.stream(cargo, {'reason': 'build-finished', 'success': True}),
                                                       self.checkout, graph)
            source_id = dict(device=9, inode=100 + index * 3, uid=1000, mode=0o755,
                             links=2, bytes=64 + index, mtime_ns=1, ctime_ns=1)
            copy_id = dict(source_id, inode=101 + index * 3, mode=0o700, links=1)
            row = {'role': graph['role'], 'graph': graph, 'original_command_record': {'synthetic': True},
                   'actual_Cargo': selected,
                   'immutable_copy': {'source': executable, 'source_identity': source_id,
                       'path': str(root / graph['name']), 'identity': copy_id,
                       'sha256': hashlib.sha256(('SYNTHETIC-' + graph['role']).encode()).hexdigest(),
                       'bytes': source_id['bytes']}}
            row['copy_receipt'] = {'path': str(root / (graph['name'] + '.json')),
                                  'identity': dict(copy_id, inode=102 + index * 3, mode=0o600), 'sha256': None}
            rows.append(row)
        self.reseal_mapping_receipts(rows)
        return root, rows

    def reseal_mapping_receipts(self, rows):
        # Exact known write_copy_receipt JSON preimage format, not a native file
        # producer. Negative cases can retain a coherent receipt so a different
        # mapping guard, rather than only this checksum, must reject them.
        for row in rows:
            payload = (json.dumps({key: value for key, value in row.items() if key != 'copy_receipt'},
                                  sort_keys=True, allow_nan=False) + '\n').encode()
            row['copy_receipt']['identity']['bytes'] = len(payload)
            row['copy_receipt']['sha256'] = hashlib.sha256(payload).hexdigest()

    def test_runtime_mapping_keeps_original_paths_and_separate_copy_evidence(self):
        root, rows = self.mapping_artifacts()
        before = json.dumps(rows, sort_keys=True)
        mapped = self.actual['runtime_match_mapping']('after', self.checkout, root, rows)
        self.assertEqual(set(mapped['matched']), {'cli', 'host', 'ui'})
        for row in rows:
            role = row['role']
            self.assertEqual(mapped['matched'][role], {'path': row['actual_Cargo']['executable'],
                                                       'sha256': row['immutable_copy']['sha256']})
            self.assertNotEqual(mapped['matched'][role]['path'], row['immutable_copy']['path'])
            self.assertIs(mapped['copy_evidence'][role]['immutable_copy'], row['immutable_copy'])
            self.assertIs(mapped['copy_evidence'][role]['copy_receipt'], row['copy_receipt'])
        self.assertEqual(json.dumps(rows, sort_keys=True), before)
        self.assertIsNone(mapped['current_runtime_inode_or_native_qualification'])

    def test_runtime_mapping_refuses_missing_duplicate_alias_and_receipt_drift(self):
        root, pristine = self.mapping_artifacts()
        mutations = [
            ('missing-role', lambda rows: rows.pop()),
            ('duplicate-role', lambda rows: rows.__setitem__(1, rows[0])),
            ('wrong-original-executable', lambda rows: rows[0]['actual_Cargo'].__setitem__('executable', '/tmp/wrong')),
            ('wrong-source-relation', lambda rows: rows[0]['immutable_copy'].__setitem__('source', rows[1]['actual_Cargo']['executable'])),
            ('copy-is-runtime-path', lambda rows: rows[0]['immutable_copy'].__setitem__('path', rows[0]['actual_Cargo']['executable'])),
            ('aliased-source-inode', lambda rows: rows[1]['immutable_copy']['source_identity'].__setitem__('inode', rows[0]['immutable_copy']['source_identity']['inode'])),
            ('aliased-source-copy-inode', lambda rows: rows[0]['immutable_copy']['identity'].__setitem__('inode', rows[0]['immutable_copy']['source_identity']['inode'])),
            ('wrong-source-bytes', lambda rows: rows[0]['immutable_copy']['source_identity'].__setitem__('bytes', 99)),
            ('wrong-copy-mode', lambda rows: rows[0]['immutable_copy']['identity'].__setitem__('mode', 0o755)),
            ('wrong-receipt-path', lambda rows: rows[0]['copy_receipt'].__setitem__('path', rows[1]['copy_receipt']['path'])),
            ('aliased-path-spelling', lambda rows: rows[0]['immutable_copy'].__setitem__('path', str(root) + '/./cagents')),
        ]
        for label, mutate in mutations:
            rows = json.loads(json.dumps(pristine))
            mutate(rows)
            self.reseal_mapping_receipts(rows)
            with self.subTest(case=label), self.assertRaises(self.hold):
                self.actual['runtime_match_mapping']('after', self.checkout, root, rows)
        rows = json.loads(json.dumps(pristine))
        rows[0]['copy_receipt']['sha256'] = '0' * 64
        with self.assertRaises(self.hold):
            self.actual['runtime_match_mapping']('after', self.checkout, root, rows)
        rows = json.loads(json.dumps(pristine))
        rows[0]['copy_receipt']['identity']['bytes'] += 1
        with self.assertRaises(self.hold):
            self.actual['runtime_match_mapping']('after', self.checkout, root, rows)


if __name__ == '__main__':
    unittest.main()
