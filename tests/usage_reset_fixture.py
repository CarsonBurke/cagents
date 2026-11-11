#!/usr/bin/python3
"""Offline contracts for the reset peers; no provider or Rust build is used."""
import http.client
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

SOURCE = Path(__file__).resolve().parent / "fixtures/usage-reset-fixture.py"
SPEC = importlib.util.spec_from_file_location("usage_reset_fixture", SOURCE)
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


class UsageResetFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="usage-reset-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        FIXTURE.seed(self.root)
        self.claude = self.root / "home/.claude"
        self.codex = self.root / "home/.codex"

    def read(self, home):
        return json.loads((home / "fixture-reset-state.json").read_text())

    def claim(self, grant="fable-fixture", key="same-key"):
        return FIXTURE.consume_claude(self.claude, {
            "program": "cedar_ember", "grant_id": grant, "request_id": key})

    def test_fable_claim_is_idempotent_and_preserves_account_windows(self):
        before = self.read(self.claude)["usage"]["rate_limits"]
        code, response, disconnect = self.claim()
        self.assertEqual((code, response["result"], disconnect), (200, "reset", False))
        self.assertEqual(response["cleared"], ["seven_day_overage_included"])
        self.assertEqual(self.claim(), (code, response, disconnect))
        state = self.read(self.claude)
        limits = state["usage"]["rate_limits"]
        self.assertEqual(state["consume_count"], 1)
        self.assertEqual(limits["model_scoped"][0]["utilization"], 0)
        for window in ("five_hour", "seven_day"):
            self.assertEqual(limits[window], before[window])
        self.assertEqual(json.loads((self.claude / "fixture-usage.json").read_text()), state["usage"])
        frames = [json.loads(line) for line in (self.claude / "fixture-frames.jsonl").read_text().splitlines()]
        self.assertEqual(frames[0]["rate_limit_info"]["unifiedWindows"]["seven_day_overage_included"]["utilization"], 0)
        self.assertEqual(frames[-2]["subtype"], "success")

    def test_account_claim_preserves_fable_and_unknown_key_cannot_spend_it(self):
        with FIXTURE.state_file(self.claude) as state:
            state["usage"]["rate_limits"]["cedar_ember"]["next_grant_id"] = "account-fixture"
        self.assertEqual(self.claim()[1]["result"], "ineligible")
        self.assertEqual(self.claim("account-fixture")[1]["result"], "reset")
        limits = self.read(self.claude)["usage"]["rate_limits"]
        self.assertEqual(limits["model_scoped"][0]["utilization"], 100)
        self.assertEqual([limits[window]["utilization"] for window in ("five_hour", "seven_day")], [0, 0])
        self.assertEqual(self.claim("account-fixture", "another-key")[1]["result"], "already_used")
        self.assertEqual(self.read(self.claude)["consume_count"], 1)

    def test_codex_no_credit_and_same_key_replay(self):
        params = {"idempotencyKey": "codex-key"}
        first = FIXTURE.consume_codex(self.codex, params)
        self.assertEqual(first, ({"outcome": "reset"}, False))
        self.assertEqual(FIXTURE.consume_codex(self.codex, params), first)
        state = self.read(self.codex)
        self.assertEqual(state["consume_count"], 1)
        self.assertEqual(state["limits"]["rateLimitResetCredits"]["availableCount"], 1)
        self.assertTrue(state["limits"]["ordinaryUsageAllowed"])
        self.assertEqual(FIXTURE.consume_codex(self.codex, {"idempotencyKey": "second"})[0], {"outcome": "reset"})
        self.assertEqual(FIXTURE.consume_codex(self.codex, {"idempotencyKey": "third"})[0], {"outcome": "noCredit"})

    def test_http_post_send_disconnect_keeps_claim_and_refreshes_real_fixture_state(self):
        with FIXTURE.state_file(self.claude) as state:
            state["mode"] = "disconnect"
        server = FIXTURE.server(self.root)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        path = f"/api/organizations/{FIXTURE.ORGANIZATION}/reset_rate_limits"
        body = json.dumps({"program": "cedar_ember", "grant_id": "fable-fixture", "request_id": "post-send"})
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
        self.addCleanup(connection.close)
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json"}
        connection.request("POST", path, body=body, headers=headers)
        with self.assertRaises(http.client.RemoteDisconnected):
            connection.getresponse()
        self.assertEqual(self.read(self.claude)["consume_count"], 1)
        connection.close()
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
        self.addCleanup(connection.close)
        connection.request("GET", "/api/oauth/usage?cedar_ember=1&skip_spend=1", headers=headers)
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        limits = json.loads(response.read())
        self.assertEqual(limits["cedar_ember"]["grants"][0]["resets_left"], 1)
        connection.request("POST", path, body=body, headers=headers)
        replay = connection.getresponse()
        self.assertEqual(replay.status, 200)
        self.assertEqual(json.loads(replay.read())["result"], "reset")
        self.assertEqual(self.read(self.claude)["consume_count"], 1)
        connection.request("GET", "/api/oauth/profile", headers=headers)
        profile = json.loads(connection.getresponse().read())
        self.assertEqual(profile["account"]["email"], "fixture@example.test")
        self.assertEqual(profile["organization"]["uuid"], FIXTURE.ORGANIZATION)
        connection.request("GET", "/api/oauth/usage", headers={"Authorization": "Bearer real-looking-token"})
        self.assertEqual(connection.getresponse().status, 401)

    def test_http_200_unconfirmed_spends_once_and_replay_reports_already_used(self):
        with FIXTURE.state_file(self.claude) as state:
            state["mode"] = "unconfirmed"
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
        self.addCleanup(connection.close)
        path = f"/api/organizations/{FIXTURE.ORGANIZATION}/reset_rate_limits"
        body = json.dumps({"program": "cedar_ember", "grant_id": "fable-fixture", "request_id": "unconfirmed-key"})
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json"}
        connection.request("POST", path, body=body, headers=headers)
        first = connection.getresponse()
        self.assertEqual(first.status, 200)
        self.assertEqual(json.loads(first.read()), {"result": "unavailable", "reason": "reset_unconfirmed"})
        state = self.read(self.claude)
        self.assertEqual(state["consume_count"], 1)
        self.assertEqual(state["usage"]["rate_limits"]["model_scoped"][0]["utilization"], 0)
        connection.request("POST", path, body=body, headers=headers)
        replay = connection.getresponse()
        self.assertEqual(replay.status, 200)
        self.assertEqual(json.loads(replay.read())["result"], "already_used")
        state = self.read(self.claude)
        self.assertEqual(state["consume_count"], 1)
        self.assertEqual(state["usage"]["rate_limits"]["cedar_ember"]["grants"][0]["resets_left"], 1)

    def test_codex_stdio_peer_reads_and_consumes_without_a_socket(self):
        peer = SOURCE.with_name("codex-usage-resets.py")
        requests = [
            {"id": 1, "method": "initialize", "params": {}},
            {"id": 2, "method": "account/rateLimits/read"},
            {"id": 3, "method": "account/rateLimitResetCredit/consume", "params": {"idempotencyKey": "stdio-key"}},
            {"id": 4, "method": "account/rateLimits/read"}]
        process = subprocess.run(["/usr/bin/python3", "-I", str(peer), "app-server"],
                                 input="".join(json.dumps(frame) + "\n" for frame in requests), text=True,
                                 env={**os.environ, "CAGENTS_USAGE_RESETS_FIXTURE_ROOT": str(self.root),
                                      "CODEX_HOME": str(self.codex)}, capture_output=True, timeout=5)
        self.assertEqual(process.returncode, 0, process.stderr)
        frames = [json.loads(line) for line in process.stdout.splitlines()]
        self.assertEqual(frames[1]["result"]["rateLimitResetCredits"]["availableCount"], 2)
        self.assertEqual(frames[2]["result"], {"outcome": "reset"})
        self.assertEqual(frames[3]["result"]["rateLimitResetCredits"]["availableCount"], 1)

    def test_codex_post_send_disconnect_reuses_key_across_processes(self):
        with FIXTURE.state_file(self.codex) as state:
            state["mode"] = "disconnect"
        peer = SOURCE.with_name("codex-usage-resets.py")
        claim = {"id": 1, "method": "account/rateLimitResetCredit/consume",
                 "params": {"idempotencyKey": "lost-response"}}
        environment = {**os.environ, "CAGENTS_USAGE_RESETS_FIXTURE_ROOT": str(self.root),
                       "CODEX_HOME": str(self.codex)}
        sent = subprocess.run(["/usr/bin/python3", "-I", str(peer), "app-server"],
                              input=json.dumps(claim) + "\n", text=True, env=environment,
                              capture_output=True, timeout=5)
        self.assertEqual((sent.returncode, sent.stdout), (0, ""), sent.stderr)
        self.assertEqual(self.read(self.codex)["consume_count"], 1)
        replay = subprocess.run(["/usr/bin/python3", "-I", str(peer), "app-server"],
                                input=json.dumps(claim) + "\n", text=True, env=environment,
                                capture_output=True, timeout=5)
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual(json.loads(replay.stdout)["result"], {"outcome": "reset"})
        self.assertEqual(self.read(self.codex)["consume_count"], 1)

    def test_seed_tightens_native_provider_homes_inside_the_private_root(self):
        with tempfile.TemporaryDirectory(prefix="usage-reset-native-") as directory:
            root = Path(directory)
            for provider in ("claude", "codex"):
                home = root / "home" / ("." + provider)
                home.mkdir(mode=0o755, parents=True)
                home.chmod(0o755)
                (home / "existing-fixture-data").write_text("keep")
            FIXTURE.seed(root)
            for provider in ("claude", "codex"):
                home = root / "home" / ("." + provider)
                self.assertEqual(home.stat().st_mode & 0o777, 0o700)
                self.assertEqual((home / "existing-fixture-data").read_text(), "keep")
                self.assertTrue((home / "fixture-reset-state.json").is_file())

    def test_seed_refuses_a_symlinked_home_without_writing_through_it(self):
        with tempfile.TemporaryDirectory(prefix="usage-reset-native-") as directory:
            root = Path(directory)
            target = root / "unrelated"
            target.mkdir()
            (root / "home").symlink_to(target, target_is_directory=True)
            with self.assertRaises(OSError):
                FIXTURE.seed(root)
            self.assertEqual(list(target.iterdir()), [])

    def test_recurring_session_reset_clears_only_the_session_and_records_each_post(self):
        before = self.read(self.claude)["usage"]["rate_limits"]
        response = FIXTURE.consume_claude(self.claude, {"program": "juniper_tide"})
        self.assertEqual((response[0], response[1]["result"], response[2]), (200, "reset", False))
        state = self.read(self.claude)
        limits = state["usage"]["rate_limits"]
        self.assertEqual(limits["five_hour"]["utilization"], 0)
        for window in ("seven_day", "model_scoped", "cedar_ember"):
            self.assertEqual(limits[window], before[window])
        self.assertFalse(limits["juniper_tide"]["available"])
        self.assertEqual(FIXTURE.consume_claude(self.claude, {"program": "juniper_tide"})[1]["result"], "already_used")
        state = self.read(self.claude)
        self.assertEqual(state["consume_count"], 1)
        self.assertEqual(state["session_post_count"], 2)

    def test_depleted_mode_reports_limits_without_any_available_reset(self):
        for provider in ("claude", "codex"):
            FIXTURE.configure(self.root, provider, "depleted")
        claude = self.read(self.claude)
        limits = claude["usage"]["rate_limits"]
        self.assertEqual(limits["model_scoped"][0]["utilization"], 100)
        self.assertEqual([item["resets_left"] for item in limits["cedar_ember"]["grants"]], [0, 0])
        self.assertIsNone(limits["cedar_ember"]["next_grant_id"])
        self.assertFalse(limits["juniper_tide"]["available"])
        frames = [json.loads(line) for line in (self.claude / "fixture-frames.jsonl").read_text().splitlines()]
        self.assertEqual(frames[0]["rate_limit_info"]["status"], "rejected")
        self.assertEqual(frames[-2]["subtype"], "error_during_execution")
        self.assertEqual(self.claim()[1]["result"], "already_used")
        codex = self.read(self.codex)
        self.assertEqual(codex["limits"]["rateLimitResetCredits"]["availableCount"], 0)
        self.assertFalse(codex["limits"]["ordinaryUsageAllowed"])
        self.assertEqual([codex["limits"]["rateLimits"][window]["usedPercent"] for window in ("primary", "secondary")], [100, 100])
        self.assertEqual(FIXTURE.consume_codex(self.codex, {"idempotencyKey": "depleted"})[0], {"outcome": "noCredit"})
        self.assertEqual(self.read(self.claude)["consume_count"], 0)
        self.assertEqual(self.read(self.codex)["consume_count"], 0)

    def test_modes_require_the_marked_fixture_before_writing(self):
        before = self.read(self.claude)
        (self.root / ".cagents-reset-fixture").unlink()
        with self.assertRaisesRegex(ValueError, "fixture marker"):
            FIXTURE.configure(self.root, "claude", "depleted")
        self.assertEqual(self.read(self.claude), before)

    def test_seed_never_overwrites_a_previous_inventory(self):
        before = self.read(self.claude)
        with self.assertRaisesRegex(ValueError, "already exists"):
            FIXTURE.seed(self.root)
        self.assertEqual(self.read(self.claude), before)


if __name__ == "__main__":
    unittest.main()
