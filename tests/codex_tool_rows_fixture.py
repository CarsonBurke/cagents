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


class CodexToolRowsFixture(unittest.TestCase):
    @contextmanager
    def connected_peer(self, never_confirm_close):
        with mock.patch.dict(os.environ, {
                "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_CLOSE": "1" if never_confirm_close else "0"}):
            scenario = PEER["Scenario"]()
        server, client = socket.socketpair()
        server.settimeout(2)
        client.settimeout(2)
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
            worker.join(2)
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
                self.assertTrue(paused.wait(5), "fixture did not reach its delayed turn")
                self.confirm_interrupt(scenario, scenario.thread, lock)
                self.assertTrue(worker.is_alive(), "the untouched child still owns delayed work")
                self.confirm_interrupt(scenario, scenario.child, lock)
                worker.join(5)
                self.assertFalse(worker.is_alive(), "cancelled playback slept for its original 600 seconds")
            finally:
                # Keep failures bounded without leaving a delayed fixture thread.
                with lock:
                    scenario.restore({"thread": scenario.thread, "child": scenario.child,
                                      "latest": None, "child_latest": None})
                worker.join(5)
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
            paused.wait(5)
            return pause(*args)

        batches = [([{"_sleep": 600}, *child_remaining], child_generation),
                   ([{"_sleep": 600}, *followup[pause_at + 1:]], parent_generation)]
        with mock.patch.object(scenario, "_pause", side_effect=enter_pause):
            workers = [threading.Thread(target=scenario.play, args=(frames, lambda _: None, lock, generation))
                       for frames, generation in batches]
            for worker in workers:
                worker.start()
            try:
                paused.wait(5)
                self.confirm_interrupt(scenario, scenario.thread, lock)
                workers[1].join(5)
                self.assertFalse(workers[1].is_alive(), "followup cancellation must not lose its wakeup")
                self.assertTrue(workers[0].is_alive(), "the background child has not been interrupted")
                self.confirm_interrupt(scenario, scenario.child, lock)
                workers[0].join(5)
                self.assertFalse(workers[0].is_alive(), "background cancellation must not lose its wakeup")
            finally:
                scenario.restore({"thread": scenario.thread, "child": scenario.child,
                                  "latest": None, "child_latest": None})
                for worker in workers:
                    worker.join(5)
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
            if not release.wait(2):
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
            self.assertTrue(paused.wait(2))
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
            worker.join(2)
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
                    self.assertTrue(done.wait(2), "the default peer did not close its connection")
                    self.assertEqual(client.recv(1), b"")

    def test_never_confirm_close_does_not_hold_a_disconnected_peer(self):
        with self.connected_peer(True) as (client, done):
            client.shutdown(socket.SHUT_WR)
            self.assertTrue(done.wait(2), "transport EOF left the fixture worker waiting")
            self.assertEqual(client.recv(1), b"")


if __name__ == "__main__":
    unittest.main()
