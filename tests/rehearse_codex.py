#!/usr/bin/env python3
"""Focused offline checks for the update rehearsal's Codex compatibility peer."""
import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
import uuid


FIXTURES = Path(__file__).resolve().parent / "fixtures"


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, FIXTURES / filename)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


adapter = module("rehearsal_codex", "rehearsal-codex.py")
peer = module("rehearsal_peer", "codex-tool-rows.py")


class ListenerTests(unittest.TestCase):
    def test_accepts_private_runtime_and_old_build_convention(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            path = runtime / "control.sock"
            self.assertEqual(adapter.listener_path(["--listen", f"unix://{path}"], runtime), path)
            old = Path("/tmp") / f"cagents-codex-{uuid.uuid4()}"
            old.mkdir(mode=0o700)
            try:
                path = old / "control.sock"
                self.assertEqual(adapter.listener_path([f"--listen=unix://{path}"], runtime), path)
            finally:
                old.rmdir()

    def test_rejects_public_symlink_existing_and_unrelated_sockets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / "runtime"
            runtime.mkdir(mode=0o700)
            public = runtime / "public"
            public.mkdir(mode=0o755)
            unrelated = root / "other"
            unrelated.mkdir(mode=0o700)
            link = runtime / "link"
            link.symlink_to(unrelated, target_is_directory=True)
            existing = runtime / "control.sock"
            existing.touch()
            for path in (public / "control.sock", unrelated / "control.sock",
                         link / "control.sock", existing, runtime / "other.sock"):
                with self.subTest(path=path), self.assertRaises(ValueError):
                    adapter.listener_path(["--listen", f"unix://{path}"], runtime)


class PersistenceTests(unittest.TestCase):
    def test_resume_restores_exact_thread_and_history_across_listener_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            first = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            first.request({"id": 1, "method": "thread/start"}, directory)
            first.request({"id": 2, "method": "turn/start", "params": {"threadId": first.thread}}, directory)
            restarted = adapter.scenario_class(peer, storage, Path("/first/control.sock"))()
            self.assertEqual((restarted.thread, restarted.child, restarted.latest),
                             (first.thread, first.child, first.latest))
            successor = adapter.scenario_class(peer, storage, Path("/second/control.sock"))()
            self.assertNotEqual(successor.thread, first.thread)
            resumed = successor.request({"id": 3, "method": "thread/resume",
                                         "params": {"threadId": first.thread}}, directory)[0]["result"]
            self.assertEqual(resumed["thread"]["id"], first.thread)
            self.assertEqual(resumed["initialTurnsPage"]["data"], [first.latest])
            self.assertEqual(successor.child, first.child)

    def test_probe_listener_does_not_replace_conversation_state(self):
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "state"
            conversation = adapter.scenario_class(peer, storage, Path("/chat/control.sock"))()
            conversation.request({"id": 1, "method": "thread/start"}, directory)
            probe = adapter.scenario_class(peer, storage, Path("/probe/control.sock"))()
            probe.request({"id": 2, "method": "account/read"}, directory)
            resumed = adapter.scenario_class(peer, storage, Path("/chat/control.sock"))()
            self.assertEqual(resumed.thread, conversation.thread)
            self.assertNotEqual(resumed.thread, probe.thread)


class StdioProbeTests(unittest.TestCase):
    def test_account_probe_answers_without_stderr_or_persisted_conversation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, runtime = root / "home", root / "rt"
            home.mkdir(mode=0o700)
            runtime.mkdir(mode=0o700)
            frames = [{"id": 1, "method": "initialize", "params": {}},
                      {"method": "initialized"},
                      {"id": 2, "method": "account/read", "params": {}},
                      {"id": 3, "method": "account/rateLimits/read"},
                      {"id": 4, "method": "model/list", "params": {"limit": 100}}]
            result = subprocess.run([sys.executable, "-I", str(FIXTURES / "rehearsal-codex.py"), "app-server"],
                                    env={**os.environ, "CAGENTS_TOOL_ROWS_FIXTURE_ROOT": str(root),
                                         "CODEX_HOME": str(home), "XDG_RUNTIME_DIR": str(runtime)},
                                    input="".join(json.dumps(frame) + "\n" for frame in frames),
                                    text=True, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            responses = {frame["id"]: frame["result"] for frame in map(json.loads, result.stdout.splitlines())}
            self.assertEqual(set(responses), {1, 2, 3, 4})
            self.assertEqual(responses[2]["account"]["email"], "fixture@invalid")
            self.assertTrue(responses[3]["ordinaryUsageAllowed"])
            self.assertFalse((home / "rehearsal-app-server").exists())
            events = list(map(json.loads, (root / "logs/codex-provider.jsonl").read_text().splitlines()))
            self.assertEqual([event["method"] for event in events if event["event"] == "probe-request"],
                             [frame["method"] for frame in frames])


if __name__ == "__main__":
    unittest.main()
