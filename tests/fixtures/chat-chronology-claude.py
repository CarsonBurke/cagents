#!/usr/bin/env python3
"""Offline Claude SDK fixture for capacity-failure chronology.

Use only with the isolated native runner described in
docs/chat-chronology-fixture.md. This writes synthetic provider history under
the supplied provider home; it makes no network calls and runs no tools.
"""

import json
import os
from pathlib import Path
import re
import sys
import time
import uuid


DIAGNOSTIC = (
    "Selected model is at capacity. Please try a different model.\n"
    "request-id: synthetic-capacity-incident\n"
    "Original provider diagnostics remain available."
)


def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def argument(name):
    for index, value in enumerate(sys.argv[1:], 1):
        if value == name:
            return sys.argv[index + 1]
        if value.startswith(name + "="):
            return value.split("=", 1)[1]
    return None


class Fixture:
    def __init__(self):
        os.umask(0o077)
        self.session = argument("--resume") or argument("--session-id") or str(uuid.uuid4())
        # Match the existing SDK fixture's exact project/history path contract.
        config = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude")))
        project = re.sub(r"[^a-zA-Z0-9]", "-", str(Path.cwd().resolve()))
        self.history = config / "projects" / project / (self.session + ".jsonl")
        self.history.parent.mkdir(parents=True, exist_ok=True)
        rows = [json.loads(line) for line in self.history.read_text().splitlines()] if self.history.exists() else []
        self.parent = rows[-1]["uuid"] if rows else None
        self.turn = sum(row.get("fixture_prompt") is True for row in rows)
        # A file, rather than a CAGENTS_* variable, also works after the daemon
        # sanitizes the provider's environment. Tests disable pacing only.
        delay_file = config / "fixture-chronology-delay"
        self.delay = float(delay_file.read_text()) if delay_file.exists() else 1.0
        if not 0 <= self.delay <= 5:
            raise ValueError("fixture stream delay must be between zero and five seconds")

    def record(self, value):
        row = dict(value, sessionId=self.session, parentUuid=self.parent, cwd=str(Path.cwd().resolve()))
        with self.history.open("a") as history:
            # The native resume catalog uses compact wire-key prefilters
            # before parsing candidate provider records.
            history.write(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.parent = value["uuid"]
        emit(value)

    def stream(self, event):
        emit({"type": "stream_event", "event": event})

    def text(self, name, parts):
        message = f"chronology-{name}-{self.turn}"
        self.stream({"type": "message_start", "message": {"id": message, "role": "assistant"}})
        self.stream({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        for part in parts:
            self.stream({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}})
            time.sleep(self.delay)
        self.stream({"type": "content_block_stop", "index": 0})
        self.record({"type": "assistant", "uuid": message, "message": {
            "id": message, "role": "assistant", "content": [{"type": "text", "text": "".join(parts)}]}})
        self.stream({"type": "message_stop"})

    def tool(self):
        message = f"chronology-tool-message-{self.turn}"
        tool = f"chronology-tool-{self.turn}"
        command = "printf 'Synthetic recovery check passed\\n'"
        raw_input = json.dumps({"command": command, "description": "Check synthetic recovery"})
        self.stream({"type": "message_start", "message": {"id": message, "role": "assistant"}})
        self.stream({"type": "content_block_start", "index": 0, "content_block": {
            "type": "tool_use", "id": tool, "name": "Bash", "input": {}}})
        for part in (raw_input[:20], raw_input[20:]):
            self.stream({"type": "content_block_delta", "index": 0, "delta": {
                "type": "input_json_delta", "partial_json": part}})
            time.sleep(self.delay)
        self.stream({"type": "content_block_stop", "index": 0})
        self.record({"type": "assistant", "uuid": message, "message": {
            "id": message, "role": "assistant", "content": [{"type": "tool_use", "id": tool,
                "name": "Bash", "input": json.loads(raw_input)}]}})
        self.stream({"type": "message_stop"})
        time.sleep(self.delay)
        self.record({"type": "user", "uuid": f"chronology-tool-result-{self.turn}", "message": {
            "role": "user", "content": [{"type": "tool_result", "tool_use_id": tool,
                "content": "Synthetic recovery check passed\n"}]}})

    def result(self, failed=False):
        # Results are live SDK frames, not assistant history nodes. Failures
        # also have an explicit saved assistant carrier for exact reopen.
        emit({"type": "result", "uuid": f"chronology-result-{self.turn}",
              "session_id": self.session, "subtype": "error_during_execution" if failed else "success",
              "is_error": failed, "errors": [DIAGNOSTIC] if failed else []})

    def prompt(self, value):
        self.turn += 1
        self.record(dict(value, fixture_prompt=True))
        emit({"type": "command_lifecycle", "command_uuid": value["uuid"], "state": "started"})
        emit({"type": "system", "subtype": "init", "session_id": self.session,
              "claude_code_version": "2.1.286", "model": "haiku"})
        if self.turn in (1, 3):
            carrier = f"chronology-capacity-{self.turn}"
            failure = {"type": "assistant", "uuid": carrier, "error": "overloaded_error",
                       "message": {"id": carrier, "role": "assistant", "content": [
                           {"type": "text", "text": DIAGNOSTIC}]}}
            self.record(failure)
            # Exact repeated carrier and result are delivery duplicates, while
            # turn 3 deliberately uses a fresh carrier with identical text.
            emit(failure)
            self.result(failed=True)
            self.result(failed=True)
        else:
            self.text("recovery", ["Recovery resumed. ", "I am checking the saved state. ",
                                   "The earlier failure should remain above your follow-up."])
            self.tool()
            self.text("followup", ["The synthetic recovery check passed. ", "This turn is complete."])
            self.result()

    def run(self):
        for line in sys.stdin:
            value = json.loads(line)
            if value.get("type") == "control_request":
                metadata = {}
                if value["request"]["subtype"] == "initialize":
                    metadata = {"claude_code_version": "2.1.286", "current_permission_mode": "default",
                                "models": [{"value": "haiku", "displayName": "Offline chronology fixture"}]}
                emit({"type": "control_response", "response": {"subtype": "success",
                     "request_id": value["request_id"], "response": metadata}})
            elif value.get("type") == "user":
                self.prompt(value)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--version"]:
        print("2.1.286 (offline chronology fixture)")
    elif sys.argv[1:2] == ["auth"]:
        emit({"loggedIn": True, "email": "fixture@example.test", "subscriptionType": "pro"})
    else:
        Fixture().run()
