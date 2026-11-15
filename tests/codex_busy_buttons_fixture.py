#!/usr/bin/python3
"""Offline authentication and login-terminal controls used by native evidence."""

from pathlib import Path
import os
import runpy
import shutil
import subprocess
import tempfile
import time
import unittest

FIXTURES = Path(__file__).resolve().parent / "fixtures"
PEER = runpy.run_path(str(FIXTURES / "codex-child-errors.py"))


class BusyButtonsFixture(unittest.TestCase):
    def test_authentication_refusal_is_a_parent_rpc_error_without_child_activity(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 17, "method": "turn/start", "params": {
            "threadId": scenario.parent,
            "input": [{"type": "text", "text": "auth-required"}],
        }}, "/fixture/home/.codex")
        self.assertEqual(replies, [{"id": 17, "error": {
            "code": -32000, "message": "Not logged in. Sign in again to continue."}}])
        self.assertFalse(scenario.turns)
        replies = scenario.request({"id": 18, "method": "turn/start", "params": {
            "threadId": scenario.child,
            "input": [{"type": "text", "text": "auth-required"}],
        }}, "/fixture/home/.codex")
        self.assertEqual(replies, [{"id": 18, "error": {
            "code": -32602, "message": "Prompt the fixture parent"}}])

    def test_login_holds_its_terminal_but_status_ignores_the_delay(self):
        with tempfile.TemporaryDirectory(prefix="codex-login-fixture-") as directory:
            root = Path(directory)
            (root / "bin").mkdir()
            home = root / "home/.codex"
            home.mkdir(parents=True)
            executable = root / "bin/codex"
            shutil.copy2(FIXTURES / "fixture-codex.sh", executable)
            switch = home / "fixture-sign-in-seconds"
            switch.write_text("1\n")
            environment = {"HOME": str(root / "home"), "CODEX_HOME": str(home),
                           "PATH": "/usr/bin:/bin"}

            def login(*arguments):
                return subprocess.run([str(executable), "login", *arguments],
                                      env=environment, capture_output=True, text=True, timeout=5)

            started = time.monotonic()
            result = login()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertGreaterEqual(time.monotonic() - started, 1)
            switch.write_text("invalid\n")
            self.assertEqual(login("status").returncode, 0)
            result = login()
            self.assertEqual(result.returncode, 2)
            self.assertIn("fixture-sign-in-seconds", result.stderr)

            with tempfile.TemporaryDirectory(prefix="codex-login-outside-") as outside:
                alias = root / "linked-home"
                alias.symlink_to(outside, target_is_directory=True)
                for refused in [outside, str(alias)]:
                    environment["CODEX_HOME"] = refused
                    result = login()
                    self.assertEqual(result.returncode, 2)
                    self.assertIn("beneath its private root", result.stderr)


if __name__ == "__main__":
    unittest.main()
