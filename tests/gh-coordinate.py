#!/usr/bin/python3
"""Offline tests for shared cooldowns, FIFO content admission and cancellation."""
import importlib.util
import json
import multiprocessing
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("gh_coordinate", ROOT / "scripts/gh-coordinate.py")
coordinate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coordinate)


def writer(root, queue, hold, interval):
    coordinate.WRITE_INTERVAL = interval
    fd = coordinate.before(Path(root), ["pr", "comment", "1"])
    queue.put(time.monotonic())
    time.sleep(hold)
    os.close(fd)


class CoordinationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        (self.root / "coordination.enabled").touch()
        self.environment = patch.dict(os.environ, {"CAGENTS_GH_COORDINATE": "1", "GH_HOST": "github.com"})
        self.environment.start()

    def tearDown(self):
        self.environment.stop()
        self.directory.cleanup()

    def test_nondefault_port_is_shared_by_response_and_nonblocking_admission(self):
        with patch.dict(os.environ, {"CAGENTS_GH_NONBLOCKING": "1"}):
            coordinate.observed(self.root, {"host": "ghe.example:8443", "status": 429, "headers": {"retry-after": "120"}})
            with self.assertRaises(coordinate.Deferred) as result:
                coordinate.before(self.root, ["api", "graphql", "--hostname", "ghe.example:8443", "-f", "query={viewer{id}}"])
            self.assertGreaterEqual(result.exception.retry_after, 119)
            # A distinct port is a distinct service, so it retains its budget.
            self.assertIsNone(coordinate.before(self.root, ["api", "graphql", "--hostname", "ghe.example:9443", "-f", "query={viewer{id}}"] ))
        self.assertEqual(coordinate.normalize_authority("https://ghe.example:443/graphql"), "ghe.example")
        self.assertEqual(coordinate.normalize_authority("ghe.example:443"), "ghe.example")
        self.assertEqual(coordinate.normalize_authority("http://ghe.example:80/graphql"), "ghe.example")
        for value in ("https://ghe.example:443", "http://ghe.example:443", "http://ghe.example:80", "ghe.example:8443", "https://[::1]:8443/graphql"):
            canonical = coordinate.normalize_authority(value)
            self.assertEqual(coordinate.normalize_authority(canonical), canonical)
        self.assertEqual(coordinate.normalize_authority("api.internal:8443"), "api.internal:8443")
        self.assertEqual(coordinate.normalize_authority("https://api.github.com:443/graphql"), "github.com")
        self.assertEqual(coordinate.host_for(["api", "https://ghe.example:8443/api/graphql"]), "ghe.example:8443")
        self.assertEqual(coordinate.host_for(["pr", "view", "https://ghe.example:8443/o/r/pull/1"]), "ghe.example:8443")

    def test_comment_body_urls_do_not_select_the_budget(self):
        self.assertEqual(coordinate.host_for(["pr", "comment", "42", "--repo", "CarsonBurke/cagents", "--body", "https://example.com/evidence"]), "github.com")
        self.assertEqual(coordinate.host_for(["pr", "comment", "42", "--body", "--hostname=wrong.example"]), "github.com")
        self.assertEqual(coordinate.host_for(["pr", "comment", "42", "--repo=forge.example/o/r", "--body", "https://wrong.example"]), "forge.example")
        self.assertEqual(coordinate.host_for(["pr", "comment", "42", "-Rforge.example/o/r", "--body", "https://wrong.example"]), "forge.example")
        self.assertEqual(coordinate.host_for(["pr", "comment", "https://forge.example/o/r/pull/42", "--body", "https://wrong.example"]), "forge.example")
        self.assertEqual(coordinate.host_for(["api", "https://api.forge.example/repos/o/r/issues", "-f", "body=https://wrong.example"]), "api.forge.example")

    def test_read_and_content_classification(self):
        for args in [
            ["pr", "checks", "1", "--watch"], ["pr", "view", "1"],
            ["api", "repos/o/r/pulls/1"], ["api", "-H", "Accept: x", "graphql", "-f", "query={viewer{id}}"],
            ["api", "graphql", "--raw-field=query=query Test{viewer{id}}"],
            ["api", "repos/o/r/issues", "-XGET", "-f", "state=open"],
        ]:
            self.assertFalse(coordinate.is_write(args), args)
        for args in [
            ["pr", "comment", "1"], ["-R", "o/r", "pr", "create"],
            ["api", "graphql", "-f", "query=mutation Add{addComment(input:$input){clientMutationId}}"],
            ["api", "graphql", "--input", "-"], ["api", "repos/o/r/issues", "--input", "-"],
            ["api", "repos/o/r/issues/1", "-XPATCH"], ["api", "repos/o/r/issues", "-f", "title=test"],
        ]:
            self.assertTrue(coordinate.is_write(args), args)

    def test_primary_reset_and_retry_after_take_later_deadline(self):
        row = {"status": 403, "headers": {"x-ratelimit-remaining": "0", "x-ratelimit-reset": "200", "retry-after": "120"}}
        self.assertEqual(coordinate.retry_deadline(row, 100, 0), 220)
        row["headers"]["retry-after"] = "20"
        self.assertEqual(coordinate.retry_deadline(row, 100, 0), 200)

    def test_secondary_does_not_wait_for_unexhausted_primary_reset(self):
        row = {"status": 200, "rate_limited": True, "headers": {"x-ratelimit-remaining": "4000", "x-ratelimit-reset": "999999", "retry-after": "30"}}
        self.assertEqual(coordinate.retry_deadline(row, 100, 4), 130)
        row["headers"].pop("retry-after")
        self.assertEqual(coordinate.retry_deadline(row, 100, 0), 160)
        self.assertEqual(coordinate.retry_deadline(row, 100, 1), 220)
        self.assertEqual(coordinate.retry_deadline(row, 100, 99), 3700)
        self.assertIsNone(coordinate.retry_deadline({"status": 403, "headers": {}}, 100, 0))

    def test_retry_after_http_date(self):
        row = {"status": 429, "headers": {"retry-after": "Thu, 01 Jan 1970 00:03:20 GMT"}}
        self.assertEqual(coordinate.retry_deadline(row, 100, 0), 200)

    def test_one_response_cools_down_new_read_and_write_callers(self):
        now = time.time()
        coordinate.observed(self.root, {"host": "api.github.com", "status": 429, "headers": {"retry-after": "0.15"}}, now=now)
        before = time.monotonic()
        self.assertIsNone(coordinate.before(self.root, ["pr", "view", "1"]))
        self.assertGreaterEqual(time.monotonic() - before, 0.12)
        with coordinate.locked(self.root, "github.com") as state:
            self.assertEqual(state["failures"], 1)
        # An earlier success cannot erase a later in-flight rate response.
        coordinate.observed(self.root, {"host": "api.github.com", "status": 200}, now=now + 0.01)
        with coordinate.locked(self.root, "github.com") as state:
            self.assertGreater(state["cooldown_until"], now)
            self.assertEqual(state["failures"], 1)

    def test_local_commands_bypass_hour_cooldown_and_content_spacing(self):
        with coordinate.locked(self.root, "github.com") as state:
            state["cooldown_until"] = time.time() + 3600
            state["next_write"] = time.time() + 3600
        with patch.dict(os.environ, {"CAGENTS_GH_NONBLOCKING": "1"}):
            for args in [["--version"], ["help", "pr"], ["completion"], ["auth", "token"], ["config", "set", "editor", "vi"], ["alias", "set", "x", "pr view"]]:
                self.assertIsNone(coordinate.before(self.root, args), args)
        self.assertFalse(coordinate.local_command(["pr", "comment", "1", "--body", "--help"]))
        self.assertTrue(coordinate.is_write(["pr", "comment", "1", "--body", "--help"]))

    def test_nonblocking_server_cooldown_is_explicit_and_cleans_ticket(self):
        with coordinate.locked(self.root, "github.com") as state:
            state["cooldown_until"] = time.time() + 3600
        with patch.dict(os.environ, {"CAGENTS_GH_NONBLOCKING": "1"}):
            with self.assertRaises(coordinate.Deferred) as result:
                coordinate.before(self.root, ["pr", "comment", "1"])
        self.assertGreaterEqual(result.exception.retry_after, 3599)
        self.assertTrue(result.exception.stderr().startswith(b"cagents-gh: deferred "))
        self.assertIn(b"\nRetry-After: ", result.exception.stderr())
        self.assertFalse(list((self.root / "coordination").glob("*.ticket")))

    def test_nonblocking_content_wait_has_no_retry_after(self):
        with coordinate.locked(self.root, "github.com") as state:
            state["next_write"] = time.time() + 3600
        with patch.dict(os.environ, {"CAGENTS_GH_NONBLOCKING": "1"}):
            with self.assertRaises(coordinate.Deferred) as result:
                coordinate.before(self.root, ["pr", "comment", "1"])
        self.assertIsNone(result.exception.retry_after)
        self.assertNotIn(b"Retry-After", result.exception.stderr())
        self.assertFalse(list((self.root / "coordination").glob("*.ticket")))

    def test_content_invocations_are_serial_and_spaced_reads_do_not_wait(self):
        context = multiprocessing.get_context("fork")
        queue = context.Queue()
        first = context.Process(target=writer, args=(str(self.root), queue, 0.3, 0.15))
        first.start()
        start = queue.get(timeout=3)
        second = context.Process(target=writer, args=(str(self.root), queue, 0, 0.15))
        second.start()
        before = time.monotonic()
        self.assertIsNone(coordinate.before(self.root, ["pr", "checks", "1"]))
        self.assertLess(time.monotonic() - before, 0.1)
        following = queue.get(timeout=3)
        self.assertGreaterEqual(following - start, 0.28)
        first.join(timeout=3)
        second.join(timeout=3)
        self.assertEqual(first.exitcode, 0)
        self.assertEqual(second.exitcode, 0)

    def test_dead_tickets_are_removed_and_descriptor_survives_exec(self):
        directory, key = coordinate.paths(self.root, "github.com")
        dead = directory / f"{key}.1.99999999.ticket"
        dead.write_text(json.dumps({"pid": 99999999, "start": "1"}))
        fd = coordinate.before(self.root, ["pr", "comment", "1"])
        try:
            self.assertFalse(dead.exists())
            self.assertTrue(os.get_inheritable(fd))
        finally:
            os.close(fd)

    def test_oldest_live_ticket_cannot_be_overtaken(self):
        directory, key = coordinate.paths(self.root, "github.com")
        oldest = directory / f"{key}.1.{os.getpid()}.ticket"
        oldest.write_text(json.dumps({"pid": os.getpid(), "start": coordinate.process_start(os.getpid())}))
        timer = threading.Timer(0.2, lambda: oldest.unlink())
        timer.start()
        before = time.monotonic()
        fd = coordinate.before(self.root, ["pr", "comment", "1"])
        os.close(fd)
        timer.join()
        self.assertGreaterEqual(time.monotonic() - before, 0.18)

    def test_disabled_guard_has_no_side_effects(self):
        with patch.dict(os.environ, {"CAGENTS_GH_COORDINATE": "0"}):
            self.assertIsNone(coordinate.before(self.root, ["pr", "comment", "1"]))
            coordinate.observed(self.root, {"status": 429, "headers": {"retry-after": "100"}})
        self.assertFalse((self.root / "coordination").exists())


if __name__ == "__main__":
    unittest.main()
