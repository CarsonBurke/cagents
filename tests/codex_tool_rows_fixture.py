#!/usr/bin/env python3
"""Offline app-server lifecycle contracts; no provider or daemon is started."""
from pathlib import Path
import runpy
import threading
import unittest
from unittest import mock


PEER = runpy.run_path(str(Path(__file__).resolve().parent / "fixtures/codex-tool-rows.py"))


class CodexToolRowsFixture(unittest.TestCase):
    def start_paused(self):
        scenario = PEER["Scenario"]()
        replies = scenario.request({"id": 1, "method": "turn/start", "params": {
            "threadId": scenario.thread}}, "/private/codex")
        generations = scenario.playback(replies)
        pause = next(index for index, reply in enumerate(replies) if "_sleep" in reply)
        scenario.play(replies[:pause], lambda _: None, threading.Lock(), generations)
        return scenario, replies[pause + 1:], generations

    def interrupt(self, scenario, thread, request_lock=None):
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
        interrupted = self.interrupt(scenario, scenario.thread)
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
        self.interrupt(scenario, scenario.child)
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
                self.interrupt(scenario, scenario.thread, lock)
                self.assertTrue(worker.is_alive(), "the untouched child still owns delayed work")
                self.interrupt(scenario, scenario.child, lock)
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
        self.interrupt(scenario, scenario.thread)
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
        self.interrupt(scenario, scenario.thread)
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
                self.interrupt(scenario, scenario.thread, lock)
                workers[1].join(5)
                self.assertFalse(workers[1].is_alive(), "followup cancellation must not lose its wakeup")
                self.assertTrue(workers[0].is_alive(), "the background child has not been interrupted")
                self.interrupt(scenario, scenario.child, lock)
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


if __name__ == "__main__":
    unittest.main()
