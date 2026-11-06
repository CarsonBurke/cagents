"""Synthetic context fixture TEST SOURCE; not executed during authoring.

This oracle reads production function source and, only when explicitly run later,
evaluates that function against an isolated in-memory metadata/filesystem model.
It never reads, patches, copies or strips real os.environ, never invokes the
controller entrypoint, and grants no actual User provenance, permission, cache,
shared resource, runtime capability or scheduling authority.
"""
import ast
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
from types import SimpleNamespace
import unittest

HERE = Path(__file__).resolve().parent
CONTROL_NAME = '/synthetic/workspace/research'
HEAD = 'ab' * 20


class MemoryPath:
    def __init__(self, name, files):
        self.name = str(PurePosixPath(name))
        self.files = files

    def __truediv__(self, suffix):
        return MemoryPath(str(PurePosixPath(self.name) / suffix), self.files)

    def __eq__(self, other):
        return isinstance(other, MemoryPath) and self.name == other.name

    def __str__(self):
        return self.name

    def resolve(self, strict=False):
        return self

    def is_file(self):
        return self.name in self.files

    def is_symlink(self):
        return False

    def read_bytes(self):
        return self.files[self.name]

    def read_text(self):
        return self.read_bytes().decode()


def synthetic_context(source_name, selected_case, attempt):
    """Exercise only hosted_context using fake metadata; never the real caller."""
    selected = {'case': selected_case, 'never_merge': True,
                'product_head': '6dc09b9a1b5d205650e244401da0938b653d16da',
                'product_tree': 'aaab67a21378958066103707baf3abe8850577d6'}
    case_bytes = json.dumps(selected).encode()
    event = {'number': 1921, 'pull_request': {'head': {
        'repo': {'full_name': 'CarsonBurke/cagents'},
        'ref': 'research/pr1853-bd88-qualification-20261028', 'sha': HEAD}}}
    files = {'/synthetic/event.json': json.dumps(event).encode(),
             CONTROL_NAME + '/.git/HEAD': (HEAD + '\n').encode(),
             CONTROL_NAME + '/.github/qualification/pr2015/case.json': case_bytes}
    engine = {'GITHUB_ACTIONS': 'true', 'RUNNER_ENVIRONMENT': 'github-hosted',
              'RUNNER_OS': 'Linux', 'RUNNER_ARCH': 'X64',
              'GITHUB_EVENT_NAME': 'pull_request', 'GITHUB_SERVER_URL': 'https://github.com',
              'GITHUB_REPOSITORY': 'CarsonBurke/cagents',
              'GITHUB_EVENT_PATH': '/synthetic/event.json',
              'GITHUB_HEAD_REF': 'research/pr1853-bd88-qualification-20261028',
              'GITHUB_WORKSPACE': '/synthetic/workspace', 'GITHUB_RUN_ID': '123',
              'GITHUB_RUN_ATTEMPT': attempt,
              'GITHUB_WORKFLOW_REF': 'CarsonBurke/cagents/.github/workflows/research-pr1853-bd88.yml@refs/pull/1921/merge',
              'GITHUB_WORKFLOW_SHA': 'cd' * 20, 'GITHUB_SHA': 'ef' * 20,
              'PR2015_CASE_SHA256': hashlib.sha256(case_bytes).hexdigest()}
    path = HERE / source_name
    parsed = ast.parse(path.read_text(), filename=str(path))
    nodes = [node for node in parsed.body if isinstance(node, ast.FunctionDef) and node.name == 'hosted_context']
    assert len(nodes) == 1
    namespace = {'os': SimpleNamespace(environ=engine),
                 'Path': lambda value: MemoryPath(value, files),
                 'CONTROL': MemoryPath(CONTROL_NAME, files), 'json': json, 're': re,
                 'control_path': lambda relative: CONTROL_NAME + '/' + relative,
                 'digest': lambda value: hashlib.sha256(MemoryPath(str(value), files).read_bytes()).hexdigest()}
    # Future fixture execution only: no module import, global initializer or main.
    exec(compile(ast.Module(body=nodes, type_ignores=[]), str(path), 'exec'), namespace)
    return namespace['hosted_context']()


class OriginalAttemptContextSourceTests(unittest.TestCase):
    def test_installer_attempt1_allowed_by_both_contexts(self):
        for source in ('case-select.py', 'run-packet.py'):
            with self.subTest(source=source):
                result = synthetic_context(source, 'installer2015', '1')
                self.assertEqual(result['run_attempt'], '1')
                self.assertEqual(result['run_binding'], '123:1:' + HEAD)

    def test_installer_reruns_and_nonliteral_one_refused_by_both_contexts(self):
        for source in ('case-select.py', 'run-packet.py'):
            for attempt in ('2', '3', '999', '01'):
                with self.subTest(source=source, attempt=attempt):
                    with self.assertRaisesRegex(AssertionError, 'one original attempt1'):
                        synthetic_context(source, 'installer2015', attempt)

    def test_zero_remains_invalid_for_all_supported_cases(self):
        for source, selected in (('case-select.py', 'installer2015'),
                                 ('case-select.py', 'legacy-final-save-1726'),
                                 ('run-packet.py', 'installer2015')):
            with self.subTest(source=source, selected=selected):
                with self.assertRaises(AssertionError):
                    synthetic_context(source, selected, '0')

    def test_legacy_positive_attempt_semantics_preserved(self):
        for attempt in ('1', '2', '999', '01'):
            with self.subTest(attempt=attempt):
                result = synthetic_context('case-select.py', 'legacy-final-save-1726', attempt)
                self.assertEqual(result['run_attempt'], attempt)


if __name__ == '__main__':
    unittest.main()
