#!/usr/bin/python3
"""Offline contracts for the reset peers; no provider or Rust build is used."""
import http.client
import importlib.util
import json
import os
import select
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

SOURCE = Path(__file__).resolve().parent / "fixtures/usage-reset-fixture.py"
SPEC = importlib.util.spec_from_file_location("usage_reset_fixture", SOURCE)
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)
# Bounds only a hang; a passing wait returns as soon as its answer arrives.
HANG_GUARD = 60
# Claude's service offers reset programs only to Claude Code's CLI surface.
CLI_AGENT = "claude-cli/0.0.0-fixture (external, cli)"


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

    def test_http_reads_and_service_start_preserve_custom_frames_and_published_files(self):
        frames = self.claude / "fixture-frames.jsonl"
        frames.write_text('{"_sleep":60}\n{"_end":true}\n')
        paths = [self.claude / name for name in (
            "fixture-reset-state.json", "fixture-usage.json", "fixture-frames.jsonl")]

        def published_files():
            return [(path.read_bytes(), path.stat().st_ino, path.stat().st_mtime_ns) for path in paths]

        before = published_files()
        server = FIXTURE.server(self.root)
        self.assertEqual(published_files(), before, "service metadata read republished fixture data")
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        for path in ("/api/oauth/profile", "/api/oauth/usage?cedar_ember=1&skip_spend=1",
                     "/api/oauth/usage?at_wall=1&skip_spend=1"):
            with self.subTest(path=path):
                connection.request("GET", path, headers={"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}"})
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertTrue(json.loads(response.read()))
                self.assertEqual(published_files(), before, "read-only request republished fixture data")

    def test_reset_programs_are_offered_only_to_the_cli_surface(self):
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)

        def read(agent):
            headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}"}
            if agent is not None:
                headers["User-Agent"] = agent
            connection.request("GET", "/api/oauth/usage?at_wall=1&skip_spend=1", headers=headers)
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            return json.loads(response.read())

        for agent in (None, "claude-code/0.0.0-fixture"):
            with self.subTest(agent=agent):
                usage = read(agent)
                self.assertEqual(usage["cedar_ember"]["ineligible_reason"], "surface")
                self.assertEqual(usage["cedar_ember"]["grants"], [])
                self.assertFalse(usage["juniper_tide"]["eligible"])
                self.assertEqual(usage["seven_day"]["utilization"], 96)
        usage = read(CLI_AGENT)
        self.assertTrue(usage["cedar_ember"]["eligible"])
        self.assertEqual({item["id"]: item["percent_used"] for item in usage["cedar_ember"]["grants"]}, {
            "fable-fixture": {"seven_day_overage_included": 100},
            "account-fixture": {"five_hour": 82, "seven_day": 96}})
        self.assertTrue(usage["juniper_tide"]["available"])

    def test_fable_unused_mode_leaves_the_fable_reset_nothing_to_refill(self):
        before = self.read(self.claude)
        FIXTURE.configure(self.root, "claude", "fable-unused")
        state = self.read(self.claude)
        limits = state["usage"]["rate_limits"]
        self.assertEqual(limits["model_scoped"][0]["utilization"], 0)
        self.assertFalse(limits["cedar_ember"]["at_limit"])
        self.assertEqual(limits["cedar_ember"]["grants"], before["usage"]["rate_limits"]["cedar_ember"]["grants"])
        self.assertEqual(state["consume_count"], 0)
        served = FIXTURE.served_usage(limits, CLI_AGENT)
        self.assertEqual(served["cedar_ember"]["grants"][0]["percent_used"], {"seven_day_overage_included": 0})
        with self.assertRaisesRegex(ValueError, "Claude-only"):
            FIXTURE.configure(self.root, "codex", "fable-unused")

    def test_frame_publication_preserves_an_open_turn_reader(self):
        path = self.claude / "fixture-frames.jsonl"
        old = path.read_bytes()
        with path.open("rb", buffering=0) as reader:
            prefix = reader.read(23)
            old_inode = os.fstat(reader.fileno()).st_ino
            self.assertEqual(self.claim()[1]["result"], "reset")
            self.assertEqual(prefix + reader.read(), old)
            self.assertNotEqual(path.stat().st_ino, old_inode)
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        frames = [json.loads(line) for line in path.read_text().splitlines()]
        self.assertEqual(frames[0]["rate_limit_info"]["status"], "allowed")
        self.assertEqual(frames[-2]["subtype"], "success")
        self.assertEqual(frames[-1], {"_end": True})

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
        # Like Claude's service, any usable grant can be claimed; the next
        # grant is only the one Claude Code suggests first.
        self.assertEqual(self.read(self.claude)["usage"]["rate_limits"]["cedar_ember"]["next_grant_id"],
                         "fable-fixture")
        self.assertEqual(self.claim("account-fixture")[1]["result"], "reset")
        limits = self.read(self.claude)["usage"]["rate_limits"]
        self.assertEqual(limits["model_scoped"][0]["utilization"], 100)
        self.assertEqual([limits[window]["utilization"] for window in ("five_hour", "seven_day")], [0, 0])
        self.assertEqual(self.claim("account-fixture", "another-key")[1]["result"], "already_used")
        self.assertEqual(self.read(self.claude)["consume_count"], 1)

    def test_a_spend_from_another_surface_is_refused_without_spending(self):
        before = self.read(self.claude)
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        body = json.dumps({"program": "cedar_ember", "grant_id": "account-fixture", "request_id": "surface-key"})
        for agent in ("claude-code/0.0.0-fixture", None):
            with self.subTest(agent=agent):
                connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
                self.addCleanup(connection.close)
                headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json"}
                if agent:
                    headers["User-Agent"] = agent
                connection.request("POST", f"/api/organizations/{FIXTURE.ORGANIZATION}/reset_rate_limits",
                                   body=body, headers=headers)
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read()), {"result": "ineligible", "reason": "surface"})
                self.assertEqual(self.read(self.claude), before)

    def test_next_grant_selection_preserves_state_and_leaves_both_grants_claimable(self):
        before = self.read(self.claude)
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json",
                   "User-Agent": CLI_AGENT}
        for name, identifier, other, scopes in (
                ("account", "account-fixture", "fable-fixture", ["five_hour", "seven_day"]),
                ("fable", "fable-fixture", "account-fixture", ["seven_day_overage_included"])):
            with self.subTest(selection=name):
                FIXTURE.configure_next_grant(self.root, name)
                expected = json.loads(json.dumps(before))
                expected["usage"]["rate_limits"]["cedar_ember"]["next_grant_id"] = identifier
                self.assertEqual(self.read(self.claude), expected)
                connection.request("GET", "/api/oauth/usage?cedar_ember=1&skip_spend=1", headers=headers)
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                program = json.loads(response.read())["cedar_ember"]
                self.assertEqual(program["next_grant_id"], identifier)
                selected = next(item for item in program["grants"] if item["id"] == identifier)
                self.assertEqual(selected["clears"], scopes)
                self.assertEqual([item["resets_left"] for item in program["grants"]], [2, 1])
                other_grant = next(item for item in program["grants"] if item["id"] == other)
                self.assertTrue(other_grant["usable_now"])

    def test_a_paused_or_unusable_grant_is_refused_without_spending(self):
        for change in ("paused", "unusable"):
            with self.subTest(change=change):
                with FIXTURE.state_file(self.claude) as state:
                    grant = state["usage"]["rate_limits"]["cedar_ember"]["grants"][1]
                    grant["paused"] = change == "paused"
                    grant["usable_now"] = change != "unusable"
                self.assertEqual(self.claim("account-fixture", f"refused-{change}")[1],
                                 {"result": "ineligible", "reason": "not_usable"})
                self.assertEqual(self.read(self.claude)["consume_count"], 0)

    def test_next_grant_selection_refuses_invalid_unmarked_or_unavailable_state(self):
        before = self.read(self.claude)
        with self.assertRaisesRegex(ValueError, "unknown next fixture grant"):
            FIXTURE.configure_next_grant(self.root, "invent-another-grant")
        self.assertEqual(self.read(self.claude), before)
        (self.root / ".cagents-reset-fixture").unlink()
        with self.assertRaisesRegex(ValueError, "private fixture marker"):
            FIXTURE.configure_next_grant(self.root, "account")
        self.assertEqual(self.read(self.claude), before)
        for unavailable in ("missing", "exhausted", "paused", "unusable", "ineligible"):
            with self.subTest(unavailable=unavailable), tempfile.TemporaryDirectory(prefix="usage-reset-grant-") as directory:
                root = Path(directory)
                FIXTURE.seed(root)
                home = root / "home/.claude"
                with FIXTURE.state_file(home) as state:
                    program = state["usage"]["rate_limits"]["cedar_ember"]
                    selected = next(item for item in program["grants"] if item["id"] == "account-fixture")
                    if unavailable == "missing":
                        program["grants"].remove(selected)
                    elif unavailable == "exhausted":
                        selected["resets_left"] = 0
                    elif unavailable == "paused":
                        selected["paused"] = True
                    elif unavailable == "unusable":
                        selected["usable_now"] = False
                    else:
                        program["eligible"] = False
                before = self.read(home)
                with self.assertRaisesRegex(ValueError, "fixture grant is unavailable"):
                    FIXTURE.configure_next_grant(root, "account")
                self.assertEqual(self.read(home), before)

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
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json",
                   "User-Agent": CLI_AGENT}
        connection.request("POST", path, body=body, headers=headers)
        with self.assertRaises(http.client.RemoteDisconnected):
            connection.getresponse()
        self.assertEqual(self.read(self.claude)["consume_count"], 1)
        connection.close()
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
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
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        path = f"/api/organizations/{FIXTURE.ORGANIZATION}/reset_rate_limits"
        body = json.dumps({"program": "cedar_ember", "grant_id": "fable-fixture", "request_id": "unconfirmed-key"})
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}", "Content-Type": "application/json",
                   "User-Agent": CLI_AGENT}
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
                                      "CODEX_HOME": str(self.codex)}, capture_output=True, timeout=HANG_GUARD)
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
                              capture_output=True, timeout=HANG_GUARD)
        self.assertEqual((sent.returncode, sent.stdout), (0, ""), sent.stderr)
        self.assertEqual(self.read(self.codex)["consume_count"], 1)
        replay = subprocess.run(["/usr/bin/python3", "-I", str(peer), "app-server"],
                                input=json.dumps(claim) + "\n", text=True, env=environment,
                                capture_output=True, timeout=HANG_GUARD)
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

    def test_limited_mode_preserves_remaining_credits_and_prior_claims(self):
        self.claim()
        FIXTURE.consume_codex(self.codex, {"idempotencyKey": "first"})
        before_claude, before_codex = self.read(self.claude), self.read(self.codex)
        for provider in ("claude", "codex"):
            FIXTURE.configure(self.root, provider, "limited")
        claude, codex = self.read(self.claude), self.read(self.codex)
        for before, after in ((before_claude, claude), (before_codex, codex)):
            self.assertEqual(after["consume_count"], 1)
            self.assertEqual(after["consume_count"], before["consume_count"])
            self.assertEqual(after["claims"], before["claims"])
        limits = claude["usage"]["rate_limits"]
        self.assertEqual(limits["model_scoped"][0]["utilization"], 100)
        self.assertEqual(limits["cedar_ember"]["grants"], before_claude["usage"]["rate_limits"]["cedar_ember"]["grants"])
        self.assertEqual(limits["juniper_tide"], before_claude["usage"]["rate_limits"]["juniper_tide"])
        self.assertFalse(codex["limits"]["ordinaryUsageAllowed"])
        self.assertEqual([codex["limits"]["rateLimits"][window]["usedPercent"] for window in ("primary", "secondary")], [100, 100])
        self.assertEqual(codex["limits"]["rateLimitResetCredits"], before_codex["limits"]["rateLimitResetCredits"])

    def test_codex_turn_recovers_after_each_reset_and_keeps_completed_history(self):
        peer = SOURCE.with_name("codex-usage-resets.py")
        process = subprocess.Popen(["/usr/bin/python3", "-I", str(peer), "app-server"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   bufsize=0, env={**os.environ, "CAGENTS_USAGE_RESETS_FIXTURE_ROOT": str(self.root),
                                                   "CODEX_HOME": str(self.codex)})
        buffered = bytearray()

        def send(identifier, method, params=None, turn=False):
            process.stdin.write((json.dumps({"id": identifier, "method": method, "params": params or {}}) + "\n").encode())
            process.stdin.flush()
            frames = []
            while True:
                while b"\n" not in buffered:
                    ready, _, _ = select.select([process.stdout], [], [], HANG_GUARD)
                    self.assertTrue(ready, f"fake Codex did not answer within {HANG_GUARD} seconds")
                    part = os.read(process.stdout.fileno(), 8192)
                    self.assertTrue(part, "fake Codex exited before answering")
                    buffered.extend(part)
                line, _, remaining = buffered.partition(b"\n")
                buffered[:] = remaining
                frame = json.loads(line)
                frames.append(frame)
                if (turn and frame.get("method") == "turn/completed") or (not turn and frame.get("id") == identifier):
                    return frames

        try:
            state_path = self.codex / "fixture-reset-state.json"
            before_read = (state_path.read_bytes(), state_path.stat().st_ino, state_path.stat().st_mtime_ns)
            send(1, "initialize")
            thread = send(2, "thread/start")[0]["result"]["thread"]["id"]
            inventory = send(20, "account/rateLimits/read")[0]["result"]
            self.assertEqual(inventory["rateLimitResetCredits"]["availableCount"], 2)
            first = send(3, "turn/start", {"threadId": thread}, turn=True)
            self.assertEqual(first[-1]["params"]["turn"]["status"], "failed")
            self.assertEqual((state_path.read_bytes(), state_path.stat().st_ino, state_path.stat().st_mtime_ns),
                             before_read, "Codex inventory/turn reads republished fixture state")
            self.assertEqual(send(4, "account/rateLimitResetCredit/consume", {"idempotencyKey": "recover-first"})[0]["result"], {"outcome": "reset"})
            recovered = send(5, "turn/start", {"threadId": thread}, turn=True)
            self.assertEqual(recovered[-1]["params"]["turn"]["status"], "completed")
            item = next(frame["params"]["item"] for frame in recovered if frame.get("method") == "item/completed")
            self.assertEqual((item["type"], item["text"]), ("agentMessage", "The fixture task can continue."))
            history = send(6, "thread/turns/list", {"threadId": thread})[0]["result"]["data"]
            self.assertEqual(history[0]["items"], [item])
            FIXTURE.configure(self.root, "codex", "limited")
            limited = send(7, "turn/start", {"threadId": thread}, turn=True)
            self.assertEqual(limited[-1]["params"]["turn"]["status"], "failed")
            self.assertEqual(self.read(self.codex)["consume_count"], 1)
            self.assertEqual(send(8, "account/rateLimitResetCredit/consume", {"idempotencyKey": "recover-second"})[0]["result"], {"outcome": "reset"})
            last = send(9, "turn/start", {"threadId": thread}, turn=True)
            self.assertEqual(last[-1]["params"]["turn"]["status"], "completed")
            reading = send(10, "account/rateLimits/read")[0]["result"]
            self.assertEqual(reading["rateLimitResetCredits"]["availableCount"], 0)
            self.assertTrue(reading["ordinaryUsageAllowed"])
            self.assertEqual(self.read(self.codex)["consume_count"], 2)
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=HANG_GUARD)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=HANG_GUARD)
                raise
            finally:
                process.stdout.close()
                process.stderr.close()
        self.assertEqual(process.returncode, 0)

    def test_modes_require_the_marked_fixture_before_writing(self):
        before = self.read(self.claude)
        (self.root / ".cagents-reset-fixture").unlink()
        with self.assertRaisesRegex(ValueError, "fixture marker"):
            FIXTURE.configure(self.root, "claude", "depleted")
        self.assertEqual(self.read(self.claude), before)

    def test_default_plan_stays_max_without_a_tier_and_cli_omits_new_metadata(self):
        server = FIXTURE.server(self.root)
        self.addCleanup(server.server_close)
        state = self.read(self.claude)
        self.assertEqual(state["profile"]["organization"]["plan_display_name"], "Max")
        self.assertNotIn("rate_limit_tier", state["profile"]["organization"])
        marker = json.loads((self.claude / "fixture-reset-api.json").read_text())
        self.assertEqual(marker["subscription_type"], "max")
        self.assertNotIn("rate_limit_tier", marker)
        usage = json.loads((self.claude / "fixture-usage.json").read_text())
        self.assertEqual(usage["subscription_type"], "max")
        self.assertNotIn("plan_display_name", usage)
        self.assertNotIn("rate_limit_tier", usage)
        script = SOURCE.with_name("chat-composer-claude.sh")
        process = subprocess.run(["/bin/sh", str(script), "auth", "status", "--json"],
                                 env={"HOME": str(self.root / "home"), "CLAUDE_CONFIG_DIR": str(self.claude),
                                      "PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, process.stderr)
        auth = json.loads(process.stdout)
        self.assertEqual(auth["email"], state["profile"]["account"]["email"])
        self.assertNotIn("planDisplayName", auth)
        self.assertNotIn("rateLimitTier", auth)
        self.assertFalse((self.claude / ".credentials.json").exists())

    def test_all_five_plans_publish_dynamic_profile_and_cached_metadata_without_consuming(self):
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        before = self.read(self.claude)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        headers = {"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}"}
        expected = [
            ("pro", "pro", "Pro", None),
            ("max5x", "max", "Max", "default_claude_max_5x"),
            ("max20x", "max", "Max", "default_claude_max_20x"),
            ("ultra", "max", "Ultra", "default_claude_max_20x"),
            ("unknown", "max", "Fixture Horizon", "fixture_horizon_unknown_tier")]
        for name, subscription, label, tier in expected:
            with self.subTest(plan=name):
                FIXTURE.configure_plan(self.root, name)
                connection.request("GET", "/api/oauth/profile", headers=headers)
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                profile = json.loads(response.read())
                self.assertEqual(profile["account"], before["profile"]["account"])
                self.assertEqual(profile["organization"]["uuid"], before["profile"]["organization"]["uuid"])
                self.assertEqual(profile["organization"]["plan_display_name"], label)
                self.assertEqual(profile["organization"].get("rate_limit_tier"), tier)
                if name == "ultra":
                    self.assertEqual(profile["organization"]["organization_type"], "claude_max")
                    self.assertIn("seat_tier", profile["organization"])
                    self.assertIsNone(profile["organization"]["seat_tier"])
                else:
                    self.assertNotIn("seat_tier", profile["organization"])
                marker = json.loads((self.claude / "fixture-reset-api.json").read_text())
                self.assertEqual(marker["base_url"], f"http://127.0.0.1:{server.server_port}")
                self.assertTrue(marker["fixture"])
                self.assertEqual(marker["subscription_type"], subscription)
                self.assertEqual(marker.get("rate_limit_tier"), tier)
                state = self.read(self.claude)
                self.assertEqual(state["usage"]["subscription_type"], subscription)
                self.assertEqual(state["usage"]["rate_limits"], before["usage"]["rate_limits"])
                for key in ("consume_count", "claims", "mode"):
                    self.assertEqual(state[key], before[key])
                usage = json.loads((self.claude / "fixture-usage.json").read_text())
                self.assertNotIn("plan_display_name", usage)
                self.assertNotIn("rate_limit_tier", usage)
        self.assertEqual(self.read(self.claude)["consume_count"], 0)

    def test_plan_selected_before_serving_survives_service_start(self):
        FIXTURE.configure_plan(self.root, "max20x")
        self.assertFalse((self.claude / "fixture-reset-api.json").exists())
        server = FIXTURE.server(self.root)
        self.addCleanup(server.server_close)
        marker = json.loads((self.claude / "fixture-reset-api.json").read_text())
        self.assertEqual(marker["subscription_type"], "max")
        self.assertEqual(marker["rate_limit_tier"], "default_claude_max_20x")

    def test_plan_switch_requires_marked_fixture_and_refuses_invalid_names(self):
        before = self.read(self.claude)
        with self.assertRaisesRegex(ValueError, "unknown fixture plan"):
            FIXTURE.configure_plan(self.root, "invent-another-plan")
        self.assertEqual(self.read(self.claude), before)
        (self.root / ".cagents-reset-fixture").unlink()
        with self.assertRaisesRegex(ValueError, "private fixture marker"):
            FIXTURE.configure_plan(self.root, "ultra")
        self.assertEqual(self.read(self.claude), before)

    def test_login_command_refreshes_profile_and_auth_without_changing_reset_state(self):
        self.assertEqual(self.claim()[1]["result"], "reset")
        FIXTURE.configure_plan(self.root, "max20x")
        server = FIXTURE.server(self.root)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        before = self.read(self.claude)
        before_codex = self.read(self.codex)
        unchanged_paths = [self.claude / name for name in (
            "fixture-reset-api.json", "fixture-usage.json", "fixture-frames.jsonl")]
        unchanged = [path.read_bytes() for path in unchanged_paths]
        process = subprocess.run(["/usr/bin/python3", "-I", "-B", str(SOURCE), "login",
                                  str(self.root), "new-login@example.com"],
                                 capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, process.stderr)
        expected = json.loads(json.dumps(before))
        expected["profile"]["account"]["email"] = "new-login@example.com"
        self.assertEqual(self.read(self.claude), expected)
        self.assertEqual(self.read(self.codex), before_codex)
        self.assertEqual([path.read_bytes() for path in unchanged_paths], unchanged)
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=HANG_GUARD)
        self.addCleanup(connection.close)
        connection.request("GET", "/api/oauth/profile",
                           headers={"Authorization": f"Bearer {FIXTURE.FAKE_TOKEN}"})
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        self.assertEqual(json.loads(response.read()), expected["profile"])
        process = subprocess.run(["/bin/sh", str(SOURCE.with_name("chat-composer-claude.sh")),
                                  "auth", "status", "--json"],
                                 env={"HOME": str(self.root / "home"), "CLAUDE_CONFIG_DIR": str(self.claude),
                                      "PATH": "/usr/bin:/bin"},
                                 capture_output=True, text=True, timeout=HANG_GUARD)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(json.loads(process.stdout), {
            "loggedIn": True, "email": "new-login@example.com", "subscriptionType": "pro"})

    def test_login_selected_before_serving_preserves_other_auth_and_marker_fields(self):
        auth_path = self.claude / "fixture-auth-status.json"
        auth = {"loggedIn": True, "email": "fixture@example.test", "subscriptionType": "max",
                "organizationId": FIXTURE.ORGANIZATION}
        FIXTURE.write_json(auth_path, auth)
        FIXTURE.configure_login(self.root, "before-serve@example.com")
        self.assertFalse((self.claude / "fixture-reset-api.json").exists())
        server = FIXTURE.server(self.root)
        self.addCleanup(server.server_close)
        marker_path = self.claude / "fixture-reset-api.json"
        marker = json.loads(marker_path.read_text())
        marker["email"] = "before-serve@example.com"
        FIXTURE.write_json(marker_path, marker)
        before = self.read(self.claude)
        FIXTURE.configure_login(self.root, "another-login@example.com")
        before["profile"]["account"]["email"] = "another-login@example.com"
        auth["email"] = "another-login@example.com"
        marker["email"] = "another-login@example.com"
        self.assertEqual(self.read(self.claude), before)
        self.assertEqual(json.loads(auth_path.read_text()), auth)
        self.assertEqual(json.loads(marker_path.read_text()), marker)

    def test_login_switch_requires_valid_email_and_private_fixture_marker(self):
        before = self.read(self.claude)
        for email in (None, "", "missing-at-sign", "someone@gmail.com", "someone@example.com.evil",
                      "someone@test", "a@b@example.com", "spaced name@example.com", "@example.com"):
            with self.subTest(email=email), self.assertRaisesRegex(ValueError, "reserved example domain"):
                FIXTURE.configure_login(self.root, email)
            self.assertEqual(self.read(self.claude), before)
        root_marker = self.root / ".cagents-reset-fixture"
        root_marker.unlink()
        with self.assertRaisesRegex(ValueError, "private fixture marker"):
            FIXTURE.configure_login(self.root, "new-login@example.com")
        self.assertEqual(self.read(self.claude), before)
        target = self.root / "unrelated-marker"
        target.touch()
        root_marker.symlink_to(target)
        with self.assertRaisesRegex(ValueError, "private fixture marker"):
            FIXTURE.configure_login(self.root, "new-login@example.com")
        self.assertEqual(self.read(self.claude), before)
        self.assertFalse((self.claude / "fixture-auth-status.json").exists())

    def test_login_switch_refuses_invalid_or_symlinked_service_marker_and_auth_status(self):
        before = self.read(self.claude)
        marker_path = self.claude / "fixture-reset-api.json"
        for marker in ({"fixture": False}, []):
            with self.subTest(marker=marker):
                FIXTURE.write_json(marker_path, marker)
                with self.assertRaisesRegex(ValueError, "invalid fake service marker"):
                    FIXTURE.configure_login(self.root, "new-login@example.com")
                self.assertEqual(self.read(self.claude), before)
                self.assertFalse((self.claude / "fixture-auth-status.json").exists())
        marker_path.unlink()
        target = self.root / "unrelated-marker.json"
        FIXTURE.write_json(target, {"fixture": True})
        marker_path.symlink_to(target)
        with self.assertRaises(OSError):
            FIXTURE.configure_login(self.root, "new-login@example.com")
        self.assertEqual(self.read(self.claude), before)
        self.assertEqual(json.loads(target.read_text()), {"fixture": True})
        marker_path.unlink()
        auth_path = self.claude / "fixture-auth-status.json"
        FIXTURE.write_json(auth_path, [])
        with self.assertRaisesRegex(ValueError, "invalid fake auth status"):
            FIXTURE.configure_login(self.root, "new-login@example.com")
        self.assertEqual(self.read(self.claude), before)
        auth_path.unlink()
        auth_path.symlink_to(target)
        with self.assertRaises(OSError):
            FIXTURE.configure_login(self.root, "new-login@example.com")
        self.assertEqual(self.read(self.claude), before)
        self.assertEqual(json.loads(target.read_text()), {"fixture": True})

    def test_seed_never_overwrites_a_previous_inventory(self):
        before = self.read(self.claude)
        with self.assertRaisesRegex(ValueError, "already exists"):
            FIXTURE.seed(self.root)
        self.assertEqual(self.read(self.claude), before)


if __name__ == "__main__":
    unittest.main()
