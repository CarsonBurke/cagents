#!/usr/bin/env python3
"""Pure controller proof tests; no desktop, input, OCR or application starts."""
import hashlib
import json
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


MODULE = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'scripts/chat_memory_kwin.py'))


class ControllerProof(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.case = Path(self.directory.name)
        self.value = {'xid': '0x123', 'ui': {'pid': 42, 'start': '99'}}
        self.namespace = MODULE['window_identity'].__globals__
        self.current = patch.dict(self.namespace, current=lambda value: True)
        self.current.start()
        self.addCleanup(self.current.stop)

    def adapter(self):
        adapter = MODULE['Adapter'].__new__(MODULE['Adapter'])
        adapter.case = self.case
        adapter.control = self.case / 'control-chat.py'
        adapter.control.write_text('# request/reply only\n')
        adapter.control_hash = hashlib.sha256(adapter.control.read_bytes()).hexdigest()
        adapter.pid, adapter.ticks = 42, '99'
        adapter.output = self.case / 'receipts.jsonl'
        adapter.owner, adapter.xid = {'pid': 7, 'start': '8'}, None
        return adapter

    def test_window_requires_exact_ui_identity_and_real_xid(self):
        self.assertEqual(MODULE['window_identity'](self.value, 42, '99'), '0x123')
        for value in ({**self.value, 'ui': {'pid': 43, 'start': '99'}},
                      {**self.value, 'ui': {'pid': 42, 'start': '100'}},
                      {**self.value, 'xid': '0x0'}, {}):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                MODULE['window_identity'](value, 42, '99')

    def test_chord_goes_only_through_explicit_request_client(self):
        adapter = self.adapter()
        response = SimpleNamespace(returncode=0, stdout=json.dumps(self.value), stderr='', check_returncode=lambda: None)
        with patch.object(self.namespace['subprocess'], 'run', return_value=response) as run:
            adapter.action('key', 'ctrl+w')
        command = run.call_args.args[0]
        self.assertEqual(command[1:], [str(adapter.control), 'chord', 'Control_L+w'])
        self.assertEqual(json.loads(adapter.output.read_text())['controller_action'], 'chord')

    def test_changed_controller_refuses_before_request(self):
        adapter = self.adapter()
        adapter.control.write_text('# changed\n')
        with patch.object(self.namespace['subprocess'], 'run') as run, self.assertRaisesRegex(SystemExit, 'changed'):
            adapter.action('window')
        run.assert_not_called()

    def test_footer_ocr_never_allows_a_different_actor(self):
        adapter = self.adapter()
        actions = []
        def action(command, *arguments):
            actions.append(command)
            if command == 'screenshot':
                Path(arguments[0]).write_bytes(b'private screenshot fixture')
            return self.value
        adapter.action = action
        response = SimpleNamespace(stdout='Exact actor memory-other;', check_returncode=lambda: None)
        with patch.object(self.namespace['subprocess'], 'run', return_value=response), self.assertRaisesRegex(SystemExit, 'footer'):
            adapter.focused_ui('memory-minted')
        self.assertEqual(actions, ['window', 'screenshot'])
        self.assertFalse(json.loads(adapter.output.read_text())['matched'])

    def test_matching_footer_revalidates_native_window_after_ocr(self):
        adapter = self.adapter()
        actions = []
        def action(command, *arguments):
            actions.append(command)
            if command == 'screenshot':
                Path(arguments[0]).write_bytes(b'private screenshot fixture')
        adapter.action = action
        response = SimpleNamespace(stdout='Exact actor\nmemory-minted;', check_returncode=lambda: None)
        with patch.object(self.namespace['subprocess'], 'run', return_value=response):
            adapter.focused_ui('memory-minted')
        self.assertEqual(actions, ['window', 'screenshot', 'window'])


if __name__ == '__main__':
    unittest.main()
