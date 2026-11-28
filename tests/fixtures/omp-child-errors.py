#!/usr/bin/python3
"""Offline Oh My Pi RPC peer (`omp --mode rpc-ui`) with two failed subagents.

It answers the requests cagents' OMP adapter sends, over stdio, without a
provider, account, model or network. Its one conversation has finished a turn
in which two subagents failed with oversized provider diagnostics:

  Retained worker  a diagnostic larger than the child transcript budget but
                   within the daemon's per-original quota, so the daemon keeps
                   the original and the child shows a shortened preview
  Refused worker   a diagnostic larger than that quota, so only the preview
                   is kept

Each diagnostic ends with a line naming its byte count, which the preview
leaves out; a copy holding that line holds the whole original. A prompt plays
one short turn that changes neither subagent. `--resume ID` reopens the
conversation as ID; otherwise each process starts a new one.

Run it through tests/fixtures/fixture-omp.sh, which checks that HOME is a
marked fixture home. It writes nothing.
"""

import json
import sys
import uuid
from pathlib import Path

sys.dont_write_bytecode = True
for _fixture_parent in Path(__file__).resolve().parents:
    if (_fixture_parent / "scripts/fixture_home.py").is_file():
        sys.path.insert(0, str(_fixture_parent / "scripts"))
        break
from fixture_home import require_fixture_home

# The daemon keeps a child item's original when it exceeds the 64 KiB child
# history budget, up to 1 MiB per original (src/chat/runtime/child_diagnostics.rs).
RETAINED_BYTES = 96 * 1024
REFUSED_BYTES = 1024 * 1024 + 64 * 1024
FIRST_LINE = "The fixture request failed."


def diagnostic(size):
    """A provider failure of at least `size` UTF-8 bytes with a unique last line."""
    lines = [FIRST_LINE]
    total = len(FIRST_LINE.encode())
    index = 0
    while total < size:
        line = f"{index:05}: Provider response from the offline fixture. request-id: local-λ"
        lines.append(line)
        total += len(line.encode()) + 1
        index += 1
    body = "\n".join(lines)
    ending = f"\nEND of the original diagnostic: {len(body.encode())} bytes before this line."
    return body + ending


CHILDREN = {
    "retained-worker": ("Retained worker", RETAINED_BYTES),
    "refused-worker": ("Refused worker", REFUSED_BYTES),
}


def message(role, timestamp, text=None, error=None):
    value = {"role": role, "timestamp": timestamp}
    if error is None:
        value["content"] = [{"type": "text", "text": text}]
    else:
        value.update(content=[], stopReason="error", errorMessage=error)
    if role == "assistant" and error is None:
        value["stopReason"] = "stop"
    return value


class Peer:
    def __init__(self, session):
        self.session = session
        self.clock = 100
        self.history = [
            message("user", 1, "Start both offline workers."),
            message("assistant", 2, "Both workers failed. Their diagnostics are in their tabs."),
        ]

    def send(self, frame):
        sys.stdout.write(json.dumps(frame, ensure_ascii=False) + "\n")
        sys.stdout.flush()

    def answer(self, request, data=None, error=None):
        frame = {"type": "response", "id": request.get("id"), "command": request.get("type")}
        if error is None:
            frame.update(success=True, data={} if data is None else data)
        else:
            frame.update(success=False, error=error)
        self.send(frame)

    def state(self):
        return {
            "sessionId": self.session,
            "sessionName": "OMP diagnostic fixture",
            "isStreaming": False,
            "isCompacting": False,
            "model": {"provider": "fixture", "id": "offline", "name": "Offline fixture"},
            "thinkingLevel": "off",
            "contextUsage": {"tokens": 0, "contextWindow": 200000},
        }

    def prompt(self, request):
        self.answer(request)
        self.clock += 2
        user = message("user", self.clock - 1, str(request.get("message", "")))
        reply = message("assistant", self.clock, "The offline fixture has nothing more to run.")
        self.history += [user, reply]
        self.send({"type": "agent_start"})
        for value in (user, reply):
            self.send({"type": "message_start", "message": value})
            self.send({"type": "message_end", "message": value})
        self.send({"type": "agent_end", "isTerminal": True, "messages": [user, reply]})

    def handle(self, request):
        kind = request.get("type")
        if kind == "negotiate_protocol":
            self.answer(request, {"protocolVersion": 2})
        elif kind == "set_subagent_subscription":
            self.answer(request, {"level": request.get("level", "events")})
        elif kind == "get_state":
            self.answer(request, self.state())
        elif kind == "get_available_models":
            self.answer(request, {"models": [self.state()["model"]]})
        elif kind == "get_available_commands":
            self.answer(request, {"commands": []})
        elif kind == "get_session_stats":
            self.answer(request, {"tokens": {"input": 0, "output": 0}})
        elif kind == "get_messages_page":
            self.answer(request, {"messages": self.history, "nextCursor": None})
        elif kind == "get_subagents":
            self.answer(request, {"subagents": [
                {"id": child, "agent": "task", "description": name,
                 "task": f"{name}: run the offline check", "status": "failed"}
                for child, (name, _) in CHILDREN.items()
            ]})
        elif kind == "get_subagent_messages":
            child = CHILDREN.get(request.get("subagentId"))
            if child is None:
                self.answer(request, error="Unknown subagent")
                return
            name, size = child
            self.answer(request, {"messages": [
                message("user", 10, f"{name}: run the offline check"),
                message("assistant", 11, error=diagnostic(size)),
            ]})
        elif kind == "prompt":
            self.prompt(request)
        else:
            self.answer(request)


def main(arguments):
    require_fixture_home("HOME")
    session = str(uuid.uuid4())
    for index, argument in enumerate(arguments):
        if argument in ("--resume", "-r") and index + 1 < len(arguments):
            session = arguments[index + 1]
        elif argument.startswith("--resume="):
            session = argument.split("=", 1)[1]
    peer = Peer(session)
    peer.send({"type": "ready"})
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(request, dict):
            peer.handle(request)


if __name__ == "__main__":
    main(sys.argv[1:])
