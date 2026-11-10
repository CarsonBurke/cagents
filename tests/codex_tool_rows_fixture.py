#!/usr/bin/env python3
"""Offline app-server lifecycle contracts; no provider or daemon is started."""
from contextlib import contextmanager
from pathlib import Path
import os
import runpy
import socket
import threading
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
        origin = replies[0]["result"]["turn"]["id"]
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        self.publish(scenario, replies[:pause], origin)
        return origin, replies[pause + 1:]

    def publish(self, scenario, replies, origin=None):
        return [published for reply in replies
                if (published := scenario.publish(reply, origin)) is not None]

    def interrupt(self, scenario, thread, turn):
        return scenario.request({"id": 99, "method": "turn/interrupt", "params": {
            "threadId": thread, "turnId": turn}}, "/private/codex")

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

    def test_parent_interrupt_confirms_exact_turn_and_discards_its_delayed_success(self):
        scenario = PEER["Scenario"]()
        origin, delayed = self.paused_turn(scenario)
        child_turn = scenario.child_latest["id"]
        confirmed = self.publish(scenario, self.interrupt(scenario, scenario.thread, origin))
        self.assertEqual(confirmed[0], {"id": 99, "result": {}})
        receipts = [frame["params"] for frame in confirmed if frame.get("method") == "turn/completed"]
        self.assertEqual([(item["threadId"], item["turn"]["id"], item["turn"]["status"])
                          for item in receipts], [(scenario.thread, origin, "interrupted")])
        self.assertEqual(scenario.child_latest["status"], "inProgress")
        remaining = self.publish(scenario, delayed, origin)
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
        next_origin, _ = self.paused_turn(scenario)
        self.assertEqual(self.publish(scenario, confirmed[1:]), [])
        self.assertEqual(scenario.latest["id"], next_origin)
        self.assertEqual(scenario.latest["status"], "inProgress")

    def test_child_interrupt_does_not_interrupt_parent_or_resurrect_child(self):
        scenario = PEER["Scenario"]()
        origin, delayed = self.paused_turn(scenario)
        confirmed = self.publish(scenario, self.interrupt(scenario, scenario.child, scenario.child_latest["id"]))
        receipts = [frame["params"] for frame in confirmed if frame.get("method") == "turn/completed"]
        self.assertEqual(len(receipts), 1)
        self.assertEqual(receipts[0]["threadId"], scenario.child)
        self.assertEqual(receipts[0]["turn"]["status"], "interrupted")
        self.assertEqual(scenario.latest["status"], "inProgress")
        emitted = self.publish(scenario, delayed, origin)
        self.assertFalse(any(frame["params"]["threadId"] == scenario.child for frame in emitted))
        self.assertEqual(scenario.latest["status"], "completed")
        self.assertEqual(scenario.child_latest["status"], "interrupted")
        self.assertEqual([item["agentsStates"][scenario.child]["status"]
                          for item in scenario.latest["items"] if "agentsStates" in item],
                         ["interrupted", "interrupted"])

    def test_interrupt_rejects_wrong_and_stale_identifiers_without_changing_turn(self):
        scenario = PEER["Scenario"]()
        origin, delayed = self.paused_turn(scenario)
        for thread, turn in [("unknown", origin), (scenario.thread, "stale"),
                             (scenario.child, origin), (scenario.thread, None)]:
            with self.subTest(thread=thread, turn=turn):
                reply = self.interrupt(scenario, thread, turn)
                self.assertEqual(reply[0]["error"]["code"], -32602)
                self.assertEqual(scenario.interrupting, set())
                self.assertEqual(scenario.latest["status"], "inProgress")
        self.publish(scenario, delayed, origin)
        self.assertIn("error", self.interrupt(scenario, scenario.thread, origin)[0])
        next_origin, _ = self.paused_turn(scenario)
        self.assertNotEqual(next_origin, origin)
        self.assertIn("error", self.interrupt(scenario, scenario.thread, origin)[0])
        self.assertEqual(self.publish(scenario, delayed, origin), [])
        self.assertEqual(scenario.latest["id"], next_origin)
        self.assertEqual(scenario.latest["status"], "inProgress")

    def test_never_confirm_interrupt_acknowledges_but_keeps_target_active(self):
        for targets in (("parent",), ("child",), ("parent", "child")):
            with self.subTest(targets=targets), mock.patch.dict(os.environ, {
                    "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT": "1"}):
                scenario = PEER["Scenario"]()
                origin, delayed = self.paused_turn(scenario)
                threads = [scenario.child if target == "child" else scenario.thread for target in targets]
                for target in threads:
                    turn = scenario.latest_turn(target)["id"]
                    replies = self.interrupt(scenario, target, turn)
                    self.assertEqual(replies, [{"id": 99, "result": {}}])
                    self.publish(scenario, replies)
                emitted = self.publish(scenario, delayed, origin)
                for target in threads:
                    self.assertFalse(any(frame.get("method") == "turn/completed"
                                         and frame["params"]["threadId"] == target for frame in emitted))
                    self.assertEqual(scenario.latest_turn(target)["status"], "inProgress")
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

        def wait(_duration):
            paused.set()
            if not release.wait(2):
                raise TimeoutError("test did not release the scripted turn")

        def playback():
            try:
                PEER["play"](replies, scenario, lock, sent.append, origin, pause=wait)
            except BaseException as error:
                failures.append(error)

        worker = threading.Thread(target=playback)
        worker.start()
        try:
            self.assertTrue(paused.wait(2))
            with lock:
                interrupt = self.interrupt(scenario, scenario.thread, origin)
            PEER["play"](interrupt, scenario, lock, sent.append)
            self.assertEqual(scenario.latest["status"], "interrupted")
            self.assertEqual(scenario.child_latest["status"], "inProgress")
            with lock:
                interrupt = self.interrupt(scenario, scenario.child, scenario.child_latest["id"])
            PEER["play"](interrupt, scenario, lock, sent.append)
            self.assertEqual(scenario.child_latest["status"], "interrupted")
            confirmed = len(sent)
        finally:
            release.set()
            worker.join(2)
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
