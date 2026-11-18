#!/usr/bin/env python3
"""Offline app-server lifecycle contracts; no provider or daemon is started."""
from contextlib import contextmanager
from pathlib import Path
import os
import runpy
import socket
import threading
import tempfile
import unittest
from unittest import mock


PEER = runpy.run_path(str(Path(__file__).resolve().parent / "fixtures/codex-tool-rows.py"))
# Bounds only a hang; a passing wait returns as soon as its event arrives.
HANG_GUARD = 60


class CodexToolRowsFixture(unittest.TestCase):
    @contextmanager
    def connected_peer(self, never_confirm_close):
        with mock.patch.dict(os.environ, {
                "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_CLOSE": "1" if never_confirm_close else "0"}):
            scenario = PEER["Scenario"]()
        server, client = socket.socketpair()
        server.settimeout(HANG_GUARD)
        client.settimeout(HANG_GUARD)
        done, failures = threading.Event(), []

        def serve():
            try:
                PEER["serve"](server, "/private/codex", scenario, threading.Lock())
            except (EOFError, ConnectionError):
                pass
            except BaseException as error:
                failures.append(error)
            finally:
                server.close()
                done.set()

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            client.sendall(b"GET / HTTP/1.1\r\nHost: fixture\r\nUpgrade: websocket\r\n"
                           b"Connection: Upgrade\r\nSec-WebSocket-Key: dGVzdC1maXh0dXJl\r\n\r\n")
            handshake = bytearray()
            while not handshake.endswith(b"\r\n\r\n"):
                handshake.extend(PEER["transport"].exact(client, 1))
            self.assertTrue(handshake.startswith(b"HTTP/1.1 101 Switching Protocols\r\n"))
            yield client, done
        finally:
            client.close()
            worker.join(HANG_GUARD)
            self.assertFalse(worker.is_alive(), "fixture worker survived its peer's disconnect")
            if failures:
                raise failures[0]

    def paused_turn(self, scenario):
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        generations = scenario.playback(replies)
        origin = replies[0]["result"]["turn"]["id"]
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        self.publish(scenario, replies[:pause], generations)
        return origin, replies[pause + 1:], generations

    def publish(self, scenario, replies, generations=None):
        sent = []
        scenario.play(replies, sent.append, threading.Lock(),
                      scenario.playback(replies) if generations is None else generations)
        return sent

    def interrupt(self, scenario, thread, turn):
        return scenario.request({"id": 99, "method": "turn/interrupt", "params": {
            "threadId": thread, "turnId": turn}}, "/private/codex")

    def start_paused(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        generations = scenario.playback(replies)
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        scenario.play(replies[:pause], lambda _: None, threading.Lock(), generations)
        return scenario, replies[pause + 1:], generations

    def confirm_interrupt(self, scenario, thread, request_lock=None):
        lock = request_lock or threading.Lock()
        with lock:
            replies = scenario.request({"id": 2, "method": "turn/interrupt", "params": {
                "threadId": thread, "turnId": scenario._latest(thread)["id"]}}, "/private/codex")
            generations = scenario.playback(replies)
        sent = []
        scenario.play(replies, sent.append, lock, generations)
        self.assertEqual(sent[0], {"id": 2, "result": {}})
        completed = [frame for frame in sent if frame.get("method") == "turn/completed"]
        self.assertEqual(len(completed), 1)
        self.assertEqual(completed[0]["params"]["threadId"], thread)
        self.assertEqual(completed[0]["params"]["turn"]["status"], "interrupted")
        return sent

    def test_private_diagnostics_scene_preserves_original_failed_turn_without_child(self):
        with tempfile.TemporaryDirectory(prefix="codex-row-diagnostics-") as directory:
            home = Path(directory)
            (home / "fixture-row-diagnostics").touch()
            # This marker takes precedence even after the web scene ran.
            (home / "fixture-web-rows").touch()
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, str(home))
            generations = scenario.playback(replies)
            pause = next(i for i, reply in enumerate(replies) if "_sleep" in reply)
            self.publish(scenario, replies[:pause], generations)
            self.assertEqual(scenario.latest["status"], "inProgress")
            self.assertIsNone(scenario.child_latest)
            sent = self.publish(scenario, replies[pause + 1:], generations)
            error = next(frame["params"] for frame in sent if frame.get("method") == "error")
            self.assertFalse(error["willRetry"])
            self.assertEqual(scenario.latest["status"], "failed")
            self.assertEqual(scenario.latest["items"], [])
            self.assertEqual(scenario.latest["error"], error["error"])
            self.assertIn("fixture-original-diagnostic", error["error"]["message"])
            self.assertEqual(scenario.request({"id": 2, "method": "thread/list"}, str(home))[0]
                             ["result"]["data"], [])
            resumed = scenario.request({"id": 3, "method": "thread/resume", "params": {
                "threadId": scenario.thread}}, str(home))[0]["result"]
            self.assertEqual(resumed["initialTurnsPage"]["data"], [scenario.latest])

    def test_private_web_scene_publishes_results_without_child_or_diff(self):
        with tempfile.TemporaryDirectory(prefix="codex-web-rows-") as directory:
            home = Path(directory)
            (home / "fixture-web-rows").touch()
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, str(home))
            generations = scenario.playback(replies)
            pause = next(i for i, reply in enumerate(replies) if "_sleep" in reply)
            self.publish(scenario, replies[:pause], generations)
            self.assertIsNone(scenario.child_latest)
            self.assertEqual(scenario.latest["items"][0]["type"], "webSearch")
            self.assertEqual(scenario.latest["items"][0]["results"], [])
            self.assertFalse(any(reply.get("method") == "turn/diff/updated" for reply in replies))
            self.assertFalse(any(reply.get("params", {}).get("threadId") == scenario.child
                                 for reply in replies))
            self.publish(scenario, replies[pause + 1:], generations)
            self.assertEqual(scenario.latest["status"], "completed")
            search, answer = scenario.latest["items"]
            self.assertEqual(search["type"], "webSearch")
            self.assertEqual(len(search["results"]), 2)
            self.assertTrue(all(result["url"].startswith("https://example.test/")
                                for result in search["results"]))
            self.assertEqual(answer["text"], "Web references ready.")
            threads = scenario.request({"id": 2, "method": "thread/list"}, str(home))
            self.assertEqual(threads[0]["result"]["data"], [])
            resumed = scenario.request({"id": 3, "method": "thread/resume", "params": {
                "threadId": scenario.thread}}, str(home))
            self.assertEqual(resumed[0]["result"]["initialTurnsPage"]["data"], [scenario.latest])

    def test_private_plan_has_a_step_in_progress_while_the_turn_works(self):
        with tempfile.TemporaryDirectory(prefix="codex-plan-") as directory:
            home = Path(directory)
            (home / "fixture-plan").write_text("30\n")
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, str(home))
            pause = next(i for i, reply in enumerate(replies) if "_sleep" in reply)
            self.assertEqual(replies[pause]["_sleep"], 30.0)
            plans = [(i, reply["params"]["plan"]["plan"]) for i, reply in enumerate(replies)
                     if reply.get("method") == "turn/plan/updated"]
            self.assertEqual(len(plans), 2)
            (working, steps), (settled, done) = plans
            self.assertLess(working, pause)
            self.assertGreater(settled, pause)
            self.assertEqual([step["status"] for step in steps],
                             ["completed", "in_progress", "pending"])
            self.assertTrue(all(step["status"] == "completed" for step in done))
            self.assertEqual(replies[-1]["method"], "turn/completed")

    def test_unnamed_child_is_named_only_by_its_own_thread_read(self):
        with tempfile.TemporaryDirectory(prefix="codex-unnamed-child-") as directory:
            home = Path(directory)
            (home / "fixture-unnamed-child").touch()
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, str(home))
            states = [reply["params"]["item"]["agentsStates"][scenario.child]
                      for reply in replies
                      if reply.get("params", {}).get("item", {}).get("type") == "collabAgentToolCall"]
            self.assertTrue(states)
            self.assertTrue(all(state["agentNickname"] is None for state in states))
            listed = scenario.request({"id": 2, "method": "thread/list"}, str(home))
            self.assertEqual([thread["agentNickname"] for thread in listed[0]["result"]["data"]],
                             [None])
            read = scenario.request({"id": 3, "method": "thread/resume", "params": {
                "threadId": scenario.child}}, str(home))
            self.assertEqual(read[0]["result"]["thread"]["agentNickname"], "Quinn")

    def test_interrupt_private_web_scene_fences_delayed_results(self):
        with tempfile.TemporaryDirectory(prefix="codex-web-rows-") as directory:
            home = Path(directory)
            (home / "fixture-web-rows").touch()
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, str(home))
            generations = scenario.playback(replies)
            pause = next(i for i, reply in enumerate(replies) if "_sleep" in reply)
            self.publish(scenario, replies[:pause], generations)
            interrupted = self.interrupt(scenario, scenario.thread, scenario.latest["id"])
            self.publish(scenario, interrupted)
            self.assertEqual(scenario.latest["status"], "interrupted")
            self.assertEqual(self.publish(scenario, replies[pause + 1:], generations), [])
            self.assertIsNone(scenario.child_latest)
            self.assertNotIn("answer", [item["id"] for item in scenario.latest["items"]])
    def test_rate_limits_come_from_the_fixture_home_when_it_has_them(self):
        import json
        import tempfile
        scenario = PEER["Scenario"]()
        request = {"id": 3, "method": "account/rateLimits/read"}
        with tempfile.TemporaryDirectory() as home:
            [missing] = scenario.request(request, home)
            self.assertEqual(missing["error"]["code"], -32601)
            limits = {"rateLimits": {"credits": {"hasCredits": True, "balance": "12.5"}}}
            Path(home, "fixture-rate-limits.json").write_text(json.dumps(limits))
            self.assertEqual(scenario.request(request, home), [{"id": 3, "result": limits}])

    def test_each_account_home_has_its_own_login_and_takes_over_a_switched_thread(self):
        import json
        with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
            first, second = Path(directory, "first"), Path(directory, "second")
            for home in (first, second):
                home.mkdir()
            read = {"id": 1, "method": "account/read"}
            [shared] = PEER["Scenario"]().request(read, str(first))
            self.assertEqual(shared["result"]["account"]["email"], "fixture@invalid")
            Path(second, "fixture-account-email").write_text("second@invalid\n")
            [own] = PEER["Scenario"]().request(read, str(second))
            self.assertEqual(own["result"]["account"]["email"], "second@invalid")

            original = PEER["Scenario"]()
            original.request({"id": 2, "method": "thread/start", "params": {}}, str(first))
            self.assertEqual(list(first.glob("sessions/*/*/*")), [], "rollouts are opt-in")
            Path(first, "fixture-rollouts").touch()
            original = PEER["Scenario"]()
            original.request({"id": 2, "method": "thread/start", "params": {"cwd": directory}},
                             str(first))
            [written] = first.glob(f"sessions/*/*/*/rollout-*-{original.thread}.jsonl")
            meta = json.loads(written.read_text())
            self.assertEqual((meta["type"], meta["payload"]["id"], meta["payload"]["cwd"]),
                             ("session_meta", original.thread, directory))

            resume = {"id": 3, "method": "thread/resume", "params": {"threadId": original.thread}}
            replacement = PEER["Scenario"]()
            [unknown] = replacement.request(resume, str(second))
            self.assertEqual(unknown["error"]["message"], "Unknown fixture thread")
            copy = second / written.relative_to(first)
            copy.parent.mkdir(parents=True)
            copy.write_bytes(written.read_bytes())
            [resumed] = replacement.request(resume, str(second))
            self.assertEqual(resumed["result"]["thread"]["id"], original.thread)
            self.assertEqual(replacement.thread, original.thread)

    def test_model_catalog_and_resolved_selection_come_from_the_fixture_home(self):
        import json
        import tempfile
        scenario = PEER["Scenario"]()
        catalog = {"data": [{"id": "offline-mini", "model": "offline-mini",
                             "displayName": "Offline Mini", "isDefault": True,
                             "defaultReasoningEffort": "medium",
                             "supportedReasoningEfforts": [{"reasoningEffort": effort}
                                                           for effort in ("low", "medium", "high")]}],
                   "nextCursor": None}
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-models.json").write_text(json.dumps(catalog))
            self.assertEqual(scenario.request({"id": 1, "method": "model/list"}, home),
                             [{"id": 1, "result": catalog}])
            [started] = scenario.request({"id": 2, "method": "thread/start", "params": {}}, home)
            self.assertEqual(started["result"]["model"], "offline-mini")
            self.assertIsNone(started["result"]["reasoningEffort"])
            [explicit] = scenario.request({"id": 3, "method": "thread/start", "params": {
                "model": "offline-mini", "config": {"model_reasoning_effort": "high"}}}, home)
            self.assertEqual(explicit["result"]["reasoningEffort"], "high")
            restored = PEER["Scenario"]()
            restored.restore(scenario.state())
            [resumed] = restored.request({"id": 4, "method": "thread/resume", "params": {
                "threadId": scenario.thread}}, home)
            self.assertEqual(resumed["result"]["model"], "offline-mini")
            self.assertEqual(resumed["result"]["reasoningEffort"], "high")
            restored.request({"id": 5, "method": "turn/start", "params": {
                "threadId": restored.thread, "model": "offline-mini", "effort": "low"}}, home)
            [resumed] = restored.request({"id": 6, "method": "thread/resume", "params": {
                "threadId": restored.thread}}, home)
            self.assertEqual(resumed["result"]["reasoningEffort"], "low")

    def test_unconfirmed_model_preserves_busy_turn_and_reports_when_resumed_after_marker_removal(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            marker = Path(home, "fixture-unconfirmed-model")
            marker.touch()
            scenario = PEER["Scenario"]()
            [started] = scenario.request({"id": 1, "method": "thread/start", "params": {}}, home)
            self.assertNotIn("model", started["result"])
            self.assertIn("thread", started["result"])
            replies = scenario.request({"id": 2, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, home)
            self.assertTrue(any(frame.get("method") == "turn/started" for frame in replies))
            self.assertEqual(scenario.latest["status"], "inProgress")
            [unconfirmed] = scenario.request({"id": 3, "method": "thread/resume", "params": {
                "threadId": scenario.thread}}, home)
            self.assertNotIn("model", unconfirmed["result"])
            marker.unlink()
            [confirmed] = scenario.request({"id": 4, "method": "thread/resume", "params": {
                "threadId": scenario.thread}}, home)
            self.assertEqual(confirmed["result"]["model"], "fixture-mini")
            self.assertEqual(confirmed["result"]["initialTurnsPage"]["data"][0]["status"], "inProgress")

    def test_an_empty_fixture_model_catalog_reports_its_cause(self):
        import tempfile
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-models.json").write_text('{"data": []}')
            with self.assertRaisesRegex(ValueError, "nonempty model/list result"):
                PEER["Scenario"]().request({"id": 1, "method": "model/list"}, home)

    def test_opt_in_turn_audit_is_bounded_and_excludes_prompt_input(self):
        import json
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            audit = Path(home, "fixture-turn-audit.json")
            scenario = PEER["Scenario"]()
            request = {"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread, "model": "offline-mini", "effort": "high",
                "input": [{"type": "text", "text": "private fixture prompt"}]}}
            scenario.request(request, home)
            self.assertFalse(audit.exists(), "auditing must be explicitly enabled")
            audit.write_text(json.dumps([{"turnId": f"old-{index}"} for index in range(20)]))
            # A fresh scenario accepts this turn immediately, without playing
            # the earlier turn or its synthetic collaboration notifications.
            scenario = PEER["Scenario"]()
            request["params"]["threadId"] = scenario.thread
            replies = scenario.request(request, home)
            records = json.loads(audit.read_text())
            self.assertEqual(len(records), 16)
            self.assertEqual(records[0], {"turnId": "old-5"})
            self.assertEqual(records[-1], {
                "turnId": replies[0]["result"]["turn"]["id"],
                "model": "offline-mini", "effort": "high"})
            self.assertNotIn("private fixture prompt", audit.read_text())
            self.assertEqual(list(Path(home).glob(".fixture-turn-audit-*")), [])
            scenario = PEER["Scenario"]()
            scenario.request({"id": 2, "method": "turn/start", "params": {
                "threadId": scenario.thread, "effort": None}}, home)
            self.assertEqual(json.loads(audit.read_text())[-1] | {"turnId": None}, {
                "turnId": None, "effort": None})

    def test_turn_audit_preserves_concurrent_processes_sharing_one_private_home(self):
        import json
        import multiprocessing
        context = multiprocessing.get_context("fork")
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            audit = Path(home, "fixture-turn-audit.json")
            audit.write_text("[]")
            barrier = context.Barrier(8)

            def record(index):
                scenario = PEER["Scenario"]()
                barrier.wait(HANG_GUARD)
                scenario.request({"id": index, "method": "turn/start", "params": {
                    "threadId": scenario.thread, "model": f"offline-{index}", "effort": "high"}}, home)

            workers = [context.Process(target=record, args=(index,)) for index in range(8)]
            try:
                for worker in workers:
                    worker.start()
                for worker in workers:
                    worker.join(HANG_GUARD)
                    self.assertEqual(worker.exitcode, 0, "audit writer did not finish successfully")
            finally:
                for worker in workers:
                    if worker.is_alive():
                        worker.terminate()
                    worker.join(HANG_GUARD)
            records = json.loads(audit.read_text())
            self.assertEqual(len(records), 8)
            self.assertEqual({row["model"] for row in records}, {f"offline-{index}" for index in range(8)})
            self.assertEqual(len({row["turnId"] for row in records}), 8)
            self.assertTrue(all(row["effort"] == "high" for row in records))

    def test_malformed_turn_audit_does_not_mutate_the_selection_or_turn(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-turn-audit.json").write_text("{}")
            scenario = PEER["Scenario"]()
            scenario.model, scenario.reasoning = "offline-original", "low"
            with self.assertRaisesRegex(ValueError, "must hold an array"):
                scenario.request({"id": 1, "method": "turn/start", "params": {
                    "threadId": scenario.thread, "model": "offline-new", "effort": "high"}}, home)
            self.assertEqual((scenario.model, scenario.reasoning), ("offline-original", "low"))
            self.assertIsNone(scenario.latest)
            self.assertIsNone(scenario.child_latest)

    def test_native_evidence_can_hold_a_turn_without_environment_forwarding(self):
        import tempfile
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-turn-pause").write_text("20\n")
            scenario = PEER["Scenario"]()
            responses = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, home)
            self.assertEqual([frame["_sleep"] for frame in responses if "_sleep" in frame], [20])
            for invalid in ("NaN", "-1", "301", "not seconds"):
                Path(home, "fixture-turn-pause").write_text(invalid)
                scenario = PEER["Scenario"]()
                with self.assertRaisesRegex(ValueError, "seconds between 0 and 300"):
                    scenario.request({"id": 1, "method": "turn/start", "params": {
                        "threadId": scenario.thread}}, home)

    def test_a_held_turn_start_answers_after_its_delay_and_accepts_a_steer_once_known(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-turn-start-delay").write_text("5\n")
            scenario = PEER["Scenario"]()
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread}}, home)
            self.assertEqual(replies[0], {"_sleep": 5})
            self.assertEqual(replies[1]["id"], 1)
            turn = replies[1]["result"]["turn"]["id"]
            self.assertEqual(replies[2]["method"], "turn/started")
            steer = scenario.request({"id": 2, "method": "turn/steer", "params": {
                "threadId": scenario.thread, "expectedTurnId": turn,
                "input": [{"type": "text", "text": "and then"}]}}, home)
            self.assertEqual(steer, [{"id": 2, "result": {"turnId": turn}}])
            stale = scenario.request({"id": 3, "method": "turn/steer", "params": {
                "threadId": scenario.thread, "expectedTurnId": "earlier",
                "input": [{"type": "text", "text": "and then"}]}}, home)
            self.assertIn("error", stale[0])
            for invalid in ("NaN", "-1", "31"):
                Path(home, "fixture-turn-start-delay").write_text(invalid)
                scenario = PEER["Scenario"]()
                with self.assertRaisesRegex(ValueError, "seconds between 0 and 30"):
                    scenario.request({"id": 1, "method": "turn/start", "params": {
                        "threadId": scenario.thread}}, home)

    def test_a_home_that_echoes_reports_the_prompt_and_steer_in_the_turn(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as home:
            Path(home, "fixture-user-items").touch()
            Path(home, "fixture-turn-pause").write_text("0\n")
            scenario = PEER["Scenario"]()
            scenario.history = []
            asked = [{"type": "text", "text": "List the open pull requests."}]
            replies = scenario.request({"id": 1, "method": "turn/start", "params": {
                "threadId": scenario.thread, "input": asked}}, home)
            turn = replies[0]["result"]["turn"]["id"]
            pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
            generations = scenario.playback(replies)
            sent = self.publish(scenario, replies[:pause], generations)
            echoed = [frame["params"]["item"] for frame in sent if frame.get("method") == "item/completed"
                      and frame["params"]["item"]["type"] == "userMessage"]
            self.assertEqual(echoed, [{"id": f"{turn}-user", "type": "userMessage", "content": asked}])
            steered = [{"type": "text", "text": "Also note any failures."}]
            steer = scenario.request({"id": 2, "method": "turn/steer", "params": {
                "threadId": scenario.thread, "expectedTurnId": turn, "input": steered}}, home)
            self.assertEqual(steer[0], {"id": 2, "result": {"turnId": turn}})
            self.publish(scenario, steer[1:])
            finished = self.publish(scenario, replies[pause + 1:], generations)
            completed = next(frame["params"]["turn"] for frame in finished
                             if frame.get("method") == "turn/completed"
                             and frame["params"]["threadId"] == scenario.thread)
            users = [item["content"] for item in completed["items"] if item["type"] == "userMessage"]
            self.assertEqual(users, [asked, steered])
            self.assertEqual(completed["items"][-1]["type"], "agentMessage")

    def test_parent_and_child_complete_after_the_pause_without_phantom_work(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        for reply in replies[:pause]:
            scenario.observe(reply)
        for thread in (scenario.thread, scenario.child):
            self.assertEqual(scenario.metadata(thread)["status"]["type"], "active")
            page = scenario.request({"id": 2, "method": "thread/turns/list", "params": {
                "threadId": thread}}, "/private/codex")[0]["result"]
            self.assertEqual(page["data"][0]["status"], "inProgress")

        remaining = replies[pause + 1:]
        collaboration = [index for index, reply in enumerate(remaining)
                         if reply.get("method") == "item/completed"
                         and reply["params"]["item"]["type"] == "collabAgentToolCall"]
        child_end = next(index for index, reply in enumerate(remaining)
                         if reply.get("method") == "turn/completed"
                         and reply["params"]["threadId"] == scenario.child)
        parent_end = next(index for index, reply in enumerate(remaining)
                          if reply.get("method") == "turn/completed"
                          and reply["params"]["threadId"] == scenario.thread)
        self.assertLess(max(collaboration), child_end)
        self.assertLess(child_end, parent_end)
        for reply in remaining:
            scenario.observe(reply)

        for thread in (scenario.thread, scenario.child):
            resumed = scenario.request({"id": 3, "method": "thread/resume", "params": {
                "threadId": thread}}, "/private/codex")[0]["result"]
            self.assertEqual(resumed["thread"]["status"]["type"], "idle")
            self.assertEqual(resumed["initialTurnsPage"]["data"][0]["status"], "completed")
        # Replaying the parent's settled collaboration items must not recreate
        # a permanently Running child after its lifecycle receipt was received.
        settled = scenario.latest["items"]
        children = [item["agentsStates"][scenario.child]["status"] for item in settled
                    if item["type"] == "collabAgentToolCall"]
        self.assertEqual(children, ["completed", "completed"])

    def test_interrupt_parent_completes_it_without_stopping_background_child(self):
        scenario, remaining, generations = self.start_paused()
        interrupted = self.confirm_interrupt(scenario, scenario.thread)
        self.assertEqual(scenario.metadata(scenario.thread)["status"]["type"], "idle")
        self.assertEqual(scenario.metadata(scenario.child)["status"]["type"], "active")
        turn = interrupted[1]["params"]["turn"]
        self.assertTrue(turn["items"], "interrupted history must keep the steps already shown")
        self.assertNotIn("answer", [item["id"] for item in turn["items"]])
        self.assertTrue(all(item.get("status") != "inProgress" for item in turn["items"]))
        self.assertTrue(any(item.get("agentsStates", {}).get(scenario.child, {}).get("status") == "running"
                            for item in turn["items"]))
        sent = []
        scenario.play(remaining, sent.append, threading.Lock(), generations)
        self.assertTrue(all(frame.get("params", {}).get("threadId") == scenario.child for frame in sent))
        self.assertEqual(scenario.latest["status"], "interrupted")
        self.assertEqual(scenario.child_latest["status"], "completed")

    def test_interrupt_child_does_not_end_parent_or_report_late_child_success(self):
        scenario, remaining, generations = self.start_paused()
        self.confirm_interrupt(scenario, scenario.child)
        self.assertEqual(scenario.latest["status"], "inProgress")
        self.assertEqual(scenario.child_latest["status"], "interrupted")
        sent = []
        scenario.play(remaining, sent.append, threading.Lock(), generations)
        self.assertEqual(scenario.latest["status"], "completed")
        self.assertEqual(scenario.child_latest["status"], "interrupted")
        self.assertFalse(any(frame.get("method") == "turn/completed"
                             and frame["params"]["threadId"] == scenario.child for frame in sent))
        states = [item["agentsStates"][scenario.child]["status"] for item in scenario.latest["items"]
                  if item["type"] == "collabAgentToolCall"]
        self.assertEqual(states, ["interrupted", "interrupted"])

    def test_interrupting_both_scopes_cancels_a_long_playback_promptly(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        next(frame for frame in replies if "_sleep" in frame)["_sleep"] = 600
        generations, lock, sent = scenario.playback(replies), threading.Lock(), []
        paused = threading.Event()
        pause = scenario._pause

        def enter_pause(*args):
            paused.set()
            return pause(*args)

        with mock.patch.object(scenario, "_pause", side_effect=enter_pause):
            worker = threading.Thread(target=scenario.play, args=(replies, sent.append, lock, generations))
            worker.start()
            try:
                self.assertTrue(paused.wait(HANG_GUARD), "fixture did not reach its delayed turn")
                self.confirm_interrupt(scenario, scenario.thread, lock)
                self.assertTrue(worker.is_alive(), "the untouched child still owns delayed work")
                self.confirm_interrupt(scenario, scenario.child, lock)
                worker.join(HANG_GUARD)
                self.assertFalse(worker.is_alive(), "cancelled playback slept for its original 600 seconds")
            finally:
                # Keep failures bounded without leaving a delayed fixture thread.
                with lock:
                    scenario.restore({"thread": scenario.thread, "child": scenario.child,
                                      "latest": None, "child_latest": None})
                worker.join(HANG_GUARD)
        self.assertFalse(any(frame.get("method") == "turn/completed" for frame in sent),
                         "the cancelled original playback must publish no success")

    def test_next_parent_turn_preserves_child_and_fences_the_previous_turn(self):
        scenario, remaining, previous = self.start_paused()
        self.confirm_interrupt(scenario, scenario.thread)
        child_turn = scenario.child_latest["id"]
        replies = scenario.request({"id": 3, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        next_turn = scenario.latest["id"]
        self.assertEqual(scenario.child_latest["id"], child_turn)

        self.assertFalse(any(frame.get("method") == "turn/started"
                             and frame["params"]["threadId"] == scenario.child for frame in replies))
        sent = []
        scenario.play(remaining, sent.append, threading.Lock(), previous)
        self.assertEqual(scenario.latest["id"], next_turn)
        self.assertEqual(scenario.latest["status"], "inProgress")
        self.assertTrue(all(frame["params"]["threadId"] == scenario.child for frame in sent))
        self.assertEqual(scenario.child_latest["id"], child_turn)

    def test_followup_and_background_playbacks_wake_for_their_own_interruptions(self):
        scenario, child_remaining, child_generation = self.start_paused()
        self.confirm_interrupt(scenario, scenario.thread)
        followup = scenario.request({"id": 3, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        parent_generation = scenario.playback(followup)
        pause_at = next(index for index, frame in enumerate(followup) if "_sleep" in frame)
        lock, paused, pause = threading.Lock(), threading.Barrier(3), scenario._pause
        scenario.play(followup[:pause_at], lambda _: None, lock, parent_generation)

        def enter_pause(*args):
            paused.wait(HANG_GUARD)
            return pause(*args)

        batches = [([{"_sleep": 600}, *child_remaining], child_generation),
                   ([{"_sleep": 600}, *followup[pause_at + 1:]], parent_generation)]
        with mock.patch.object(scenario, "_pause", side_effect=enter_pause):
            workers = [threading.Thread(target=scenario.play, args=(frames, lambda _: None, lock, generation))
                       for frames, generation in batches]
            for worker in workers:
                worker.start()
            try:
                paused.wait(HANG_GUARD)
                self.confirm_interrupt(scenario, scenario.thread, lock)
                workers[1].join(HANG_GUARD)
                self.assertFalse(workers[1].is_alive(), "followup cancellation must not lose its wakeup")
                self.assertTrue(workers[0].is_alive(), "the background child has not been interrupted")
                self.confirm_interrupt(scenario, scenario.child, lock)
                workers[0].join(HANG_GUARD)
                self.assertFalse(workers[0].is_alive(), "background cancellation must not lose its wakeup")
            finally:
                scenario.restore({"thread": scenario.thread, "child": scenario.child,
                                  "latest": None, "child_latest": None})
                for worker in workers:
                    worker.join(HANG_GUARD)
    def test_wrong_interrupt_scope_and_unsubscribe_do_not_end_work(self):
        scenario, _, _ = self.start_paused()
        for thread, turn in ((scenario.thread, "stale-turn"), ("unknown-thread", scenario.latest["id"])):
            replies = scenario.request({"id": 4, "method": "turn/interrupt", "params": {
                "threadId": thread, "turnId": turn}}, "/private/codex")
            self.assertIn("error", replies[0])
        self.assertEqual(scenario.request({"id": 5, "method": "thread/unsubscribe", "params": {
            "threadId": scenario.thread}}, "/private/codex"), [{"id": 5, "result": {}}])
        self.assertEqual(scenario.latest["status"], "inProgress")
        self.assertEqual(scenario.child_latest["status"], "inProgress")

    def test_optional_prediction_has_successful_response_and_correlated_absence(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 4, "method": "thread/prediction/request", "params": {
            "threadId": scenario.thread, "sourceTurnId": "finished-turn"}}, "/private/codex")
        self.assertEqual(replies, [
            {"id": 4, "result": {}},
            {"method": "thread/prediction/updated", "params": {
                "threadId": scenario.thread, "sourceTurnId": "finished-turn",
                "result": {"type": "completed", "text": None}}},
        ])

    def test_interrupt_history_stays_active_until_its_receipt_is_published(self):
        scenario, remaining, generations = self.start_paused()
        turn = scenario.latest["id"]
        replies = self.interrupt(scenario, scenario.thread, turn)
        self.assertEqual(scenario.latest["status"], "inProgress")
        self.assertEqual(scenario.metadata(scenario.thread)["status"]["type"], "active")
        sent = self.publish(scenario, remaining, generations)
        self.assertTrue(all(frame["params"]["threadId"] == scenario.child for frame in sent))
        self.assertEqual(scenario.latest["status"], "inProgress")
        self.publish(scenario, replies)
        self.assertEqual(scenario.latest["status"], "interrupted")

    def test_parent_interrupt_confirms_exact_turn_and_discards_its_delayed_success(self):
        scenario = PEER["Scenario"]()
        origin, delayed, generations = self.paused_turn(scenario)
        child_turn = scenario.child_latest["id"]
        confirmed = self.publish(scenario, self.interrupt(scenario, scenario.thread, origin))
        self.assertEqual(confirmed[0], {"id": 99, "result": {}})
        receipts = [frame["params"] for frame in confirmed if frame.get("method") == "turn/completed"]
        self.assertEqual([(item["threadId"], item["turn"]["id"], item["turn"]["status"])
                          for item in receipts], [(scenario.thread, origin, "interrupted")])
        self.assertEqual(scenario.child_latest["status"], "inProgress")
        remaining = self.publish(scenario, delayed, generations)
        self.assertEqual([(frame["method"], frame["params"]["threadId"])
                          for frame in remaining], [
                              ("turn/completed", scenario.child),
                              ("thread/status/changed", scenario.child),
                          ])
        self.assertEqual(remaining[0]["params"]["turn"]["id"], child_turn)
        self.assertEqual(scenario.latest["status"], "interrupted")
        self.assertEqual(scenario.child_latest["status"], "completed")
        for thread in (scenario.thread, scenario.child):
            self.assertEqual(scenario.metadata(thread)["status"]["type"], "idle")
        self.assertFalse(any(item.get("text") == "Done." for item in scenario.latest["items"]))
        self.assertTrue(all(item.get("status") != "inProgress" for item in scenario.latest["items"]))
        next_origin, _, _ = self.paused_turn(scenario)
        self.assertEqual(self.publish(scenario, confirmed[1:]), [])
        self.assertEqual(scenario.latest["id"], next_origin)
        self.assertEqual(scenario.latest["status"], "inProgress")

    def test_child_interrupt_does_not_interrupt_parent_or_resurrect_child(self):
        scenario = PEER["Scenario"]()
        origin, delayed, generations = self.paused_turn(scenario)
        confirmed = self.publish(scenario, self.interrupt(scenario, scenario.child, scenario.child_latest["id"]))
        receipts = [frame["params"] for frame in confirmed if frame.get("method") == "turn/completed"]
        self.assertEqual(len(receipts), 1)
        self.assertEqual(receipts[0]["threadId"], scenario.child)
        self.assertEqual(receipts[0]["turn"]["status"], "interrupted")
        self.assertEqual(scenario.latest["status"], "inProgress")
        emitted = self.publish(scenario, delayed, generations)
        self.assertFalse(any(frame["params"]["threadId"] == scenario.child for frame in emitted))
        self.assertEqual(scenario.latest["status"], "completed")
        self.assertEqual(scenario.child_latest["status"], "interrupted")
        self.assertEqual([item["agentsStates"][scenario.child]["status"]
                          for item in scenario.latest["items"] if "agentsStates" in item],
                         ["interrupted", "interrupted"])

    def test_interrupt_rejects_wrong_and_stale_identifiers_without_changing_turn(self):
        scenario = PEER["Scenario"]()
        origin, delayed, generations = self.paused_turn(scenario)
        for thread, turn in [("unknown", origin), (scenario.thread, "stale"),
                             (scenario.child, origin), (scenario.thread, None)]:
            with self.subTest(thread=thread, turn=turn):
                reply = self.interrupt(scenario, thread, turn)
                self.assertEqual(reply[0]["error"]["code"], -32602)
                self.assertEqual(scenario.interrupting, set())
                self.assertEqual(scenario.latest["status"], "inProgress")
        self.publish(scenario, delayed, generations)
        self.assertIn("error", self.interrupt(scenario, scenario.thread, origin)[0])
        next_origin, _, _ = self.paused_turn(scenario)
        self.assertNotEqual(next_origin, origin)
        self.assertIn("error", self.interrupt(scenario, scenario.thread, origin)[0])
        self.assertEqual(self.publish(scenario, delayed, generations), [])
        self.assertEqual(scenario.latest["id"], next_origin)
        self.assertEqual(scenario.latest["status"], "inProgress")

    def test_never_confirm_interrupt_acknowledges_but_keeps_target_active(self):
        for targets in (("parent",), ("child",), ("parent", "child")):
            with self.subTest(targets=targets), mock.patch.dict(os.environ, {
                    "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT": "1"}):
                scenario = PEER["Scenario"]()
                origin, delayed, generations = self.paused_turn(scenario)
                threads = [scenario.child if target == "child" else scenario.thread for target in targets]
                for target in threads:
                    turn = scenario._latest(target)["id"]
                    replies = self.interrupt(scenario, target, turn)
                    self.assertEqual(replies, [{"id": 99, "result": {}}])
                    self.publish(scenario, replies)
                emitted = self.publish(scenario, delayed, generations)
                for target in threads:
                    self.assertFalse(any(frame.get("method") == "turn/completed"
                                         and frame["params"]["threadId"] == target for frame in emitted))
                    self.assertEqual(scenario._latest(target)["status"], "inProgress")
                    self.assertEqual(scenario.metadata(target)["status"]["type"], "active")
                # A new script must not replace the still-owned child turn.
                next_turn = scenario.request({"id": 100, "method": "turn/start", "params": {
                    "threadId": scenario.thread}}, "/private/codex")
                self.assertIn("error", next_turn[0])

    def test_delayed_playback_stays_settled_after_parent_and_explicit_child_interrupts(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        origin = replies[0]["result"]["turn"]["id"]
        lock, paused, release = threading.Lock(), threading.Event(), threading.Event()
        sent, failures = [], []

        def wait(_duration, _generations):
            paused.set()
            if not release.wait(HANG_GUARD):
                raise TimeoutError("test did not release the scripted turn")
            return True

        def playback():
            try:
                scenario.play(replies, sent.append, lock, scenario.playback(replies))
            except BaseException as error:
                failures.append(error)

        worker = threading.Thread(target=playback)
        patch = mock.patch.object(scenario, "_pause", side_effect=wait)
        patch.start()
        worker.start()
        try:
            self.assertTrue(paused.wait(HANG_GUARD))
            with lock:
                interrupt = self.interrupt(scenario, scenario.thread, origin)
            scenario.play(interrupt, sent.append, lock, scenario.playback(interrupt))
            self.assertEqual(scenario.latest["status"], "interrupted")
            self.assertEqual(scenario.child_latest["status"], "inProgress")
            with lock:
                interrupt = self.interrupt(scenario, scenario.child, scenario.child_latest["id"])
            scenario.play(interrupt, sent.append, lock, scenario.playback(interrupt))
            self.assertEqual(scenario.child_latest["status"], "interrupted")
            confirmed = len(sent)
        finally:
            release.set()
            worker.join(HANG_GUARD)
            patch.stop()
        self.assertFalse(worker.is_alive())
        if failures:
            raise failures[0]
        self.assertEqual(len(sent), confirmed, "late scripted frames escaped the interruption fence")
        self.assertEqual(scenario.latest["status"], "interrupted")

    def test_close_confirmation_can_be_withheld_until_peer_disconnect(self):
        for never_confirm in (False, True):
            with self.subTest(never_confirm=never_confirm), self.connected_peer(never_confirm) as (client, done):
                client.sendall(PEER["transport"].frame(b"", opcode=8))
                if never_confirm:
                    client.settimeout(0.05)
                    with self.assertRaises(TimeoutError):
                        client.recv(1)
                    self.assertFalse(done.is_set(), "a WebSocket close frame is not a transport disconnect")
                else:
                    self.assertTrue(done.wait(HANG_GUARD), "the default peer did not close its connection")
                    self.assertEqual(client.recv(1), b"")

    def test_never_confirm_close_does_not_hold_a_disconnected_peer(self):
        with self.connected_peer(True) as (client, done):
            client.shutdown(socket.SHUT_WR)
            self.assertTrue(done.wait(HANG_GUARD), "transport EOF left the fixture worker waiting")
            self.assertEqual(client.recv(1), b"")


if __name__ == "__main__":
    unittest.main()
