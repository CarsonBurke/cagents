#!/usr/bin/python3
"""Check the seeded parent chain and provider-home isolation without a provider."""
import json
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


fixture = runpy.run_path(str(Path(__file__).with_name("claude-resume-history.py")))


class ClaudeResumeHistory(unittest.TestCase):
    def test_complete_history_and_refused_overwrite(self):
        with tempfile.TemporaryDirectory(prefix="claude-history-") as directory:
            root = Path(directory) / "fixture"
            metadata = fixture["seed"](root, turns=50)
            transcript = Path(metadata["history"])
            original = transcript.read_bytes()
            rows = [json.loads(line) for line in original.splitlines()]
            self.assertEqual(len(rows), 100)
            parent = None
            for number, row in enumerate(rows):
                self.assertEqual(row["sessionId"], metadata["thread_id"])
                self.assertEqual(row["cwd"], metadata["working_directory"])
                self.assertEqual(row["parentUuid"], parent)
                self.assertEqual(row["type"], "user" if number % 2 == 0 else "assistant")
                self.assertEqual(row["message"]["role"], row["type"])
                self.assertTrue(row["message"]["content"][0]["text"])
                parent = row["uuid"]
            self.assertEqual(transcript.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                fixture["seed"](root)
            self.assertEqual(transcript.read_bytes(), original)

    def test_real_home_alias_and_invalid_size_are_rejected_before_writes(self):
        with tempfile.TemporaryDirectory(prefix="claude-history-") as directory:
            root = Path(directory) / "fixture"
            root.mkdir()
            account = Path(directory) / "account"
            account.mkdir()
            (root / "home").symlink_to(account, target_is_directory=True)
            with patch.object(fixture["pwd"], "getpwuid", return_value=SimpleNamespace(pw_dir=str(account))):
                with self.assertRaisesRegex(ValueError, "real Claude home"):
                    fixture["seed"](root)
            self.assertFalse((account / ".claude").exists())
            for turns in [0, 201]:
                with self.assertRaises(ValueError):
                    fixture["seed"](root, turns)
            self.assertFalse((account / ".claude").exists())


if __name__ == "__main__":
    unittest.main()
