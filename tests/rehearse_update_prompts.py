#!/usr/bin/python3
"""Exercise update review prompts through the rehearsal's actual PTY driver."""
from pathlib import Path
import subprocess
import sys
import unittest


DRIVER = Path(__file__).resolve().parents[1] / 'scripts/rehearse_update_pty.py'


class UpdatePrompts(unittest.TestCase):
    def drive(self, program, *answers):
        return subprocess.run(
            [sys.executable, str(DRIVER), '--timeout', '5', *answers, '--',
             sys.executable, '-u', '-c', program],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=10,
        )

    def test_review_then_explicit_stop_and_confirmation(self):
        result = self.drive('''
import sys, time
# A prompt can arrive across more than one PTY read.
sys.stdout.write('Enter: review what ')
sys.stdout.flush()
time.sleep(.1)
print('stops   n: cancel', flush=True)
assert input() == ''
print('Type "stop" to stop these and update. Anything else cancels.', flush=True)
assert input() == 'stop'
print('Apply the update? [y/N] ', end='', flush=True)
assert input() == 'yes'
print('reviewed and confirmed', flush=True)
''', '--answer', 'yes', '--stop', 'stop')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('reviewed and confirmed', result.stdout)

    def test_review_does_not_authorize_stopping_without_explicit_answer(self):
        result = self.drive('''
print('Enter: review what stops   n: cancel', flush=True)
assert input() == ''
print('Type "stop" to stop these and update. Anything else cancels.', flush=True)
assert input() == ''
print('stopping was not authorized', flush=True)
''', '--answer', 'yes')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('stopping was not authorized', result.stdout)

    def test_working_agents_choice_still_waits_by_default(self):
        result = self.drive('''
print('Enter: wait   i: interrupt them now   n: cancel', flush=True)
assert input() == ''
print('waiting for agents', flush=True)
''', '--stop', 'stop')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('waiting for agents', result.stdout)


if __name__ == '__main__':
    unittest.main()
