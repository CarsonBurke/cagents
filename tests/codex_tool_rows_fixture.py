#!/usr/bin/env python3
"""Offline app-server lifecycle contracts; no provider or daemon is started."""
from pathlib import Path
import runpy
import unittest


PEER = runpy.run_path(str(Path(__file__).resolve().parent / "fixtures/codex-tool-rows.py"))


class CodexToolRowsFixture(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
