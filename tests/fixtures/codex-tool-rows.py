#!/usr/bin/env python3
"""Offline Codex peer that plays one turn of tool and event items.

Each prompt to the fixture's thread starts a turn that streams an MCP call, a
dynamic and a function tool, collaboration calls, a pause, a hook prompt,
image generation, a context compaction and a turn diff, first as started and
then, after a short pause, as completed. It exists to check how each item
reads in the native transcript, running and settled.

Requires a private CAGENTS_TOOL_ROWS_FIXTURE_ROOT and CODEX_HOME beneath it,
and a private XDG_RUNTIME_DIR. Launch it through the ordinary development
runner as the isolated Codex executable. It models the app-server protocol
only: no provider, credentials, network service or tool is contacted.
"""
import importlib.util
import json
import os
from pathlib import Path
import socket
import sys
import threading
import time
import uuid

sys.dont_write_bytecode = True
_source = Path(__file__).with_name("terminal-escape-codex.py")
_spec = importlib.util.spec_from_file_location("codex_fixture_transport", _source)
transport = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(transport)

PAUSE = float(os.environ.get("CAGENTS_TOOL_ROWS_PAUSE", "6"))


def items(child):
    """Each item as it starts, and as it completes."""
    mcp = {"type": "mcpToolCall", "server": "codex_apps", "tool": "linear_get_issue",
           "appContext": {"connectorId": "linear", "appName": "Linear",
                          "actionName": "get_issue"},
           "arguments": {"id": "ENG-42"}}
    failed_mcp = {"type": "mcpToolCall", "server": "docs", "tool": "search_pages",
                  "arguments": None}
    collab = {"type": "collabAgentToolCall", "senderThreadId": "", "receiverThreadIds": [child],
              "agentsStates": {child: {"status": "running", "agentNickname": "Quinn"}}}
    return [
        ({**mcp, "id": "mcp", "status": "inProgress"},
         {**mcp, "id": "mcp", "status": "completed",
          "result": {"content": [{"type": "text", "text": "ENG-42: Tool rows say Called"},
                                 {"type": "text", "text": "Status: In progress"}],
                     "structuredContent": None}}),
        ({**failed_mcp, "id": "mcp-failed", "status": "inProgress"},
         {**failed_mcp, "id": "mcp-failed", "status": "failed",
          "error": {"message": "The docs connector is not authorized."}}),
        ({"id": "dynamic", "type": "dynamicToolCall", "tool": "lookup_symbol",
          "arguments": {"symbol": "hover::view"}, "status": "inProgress"},
         {"id": "dynamic", "type": "dynamicToolCall", "tool": "lookup_symbol",
          "arguments": {"symbol": "hover::view"}, "status": "completed", "success": True,
          "contentItems": [{"type": "inputText", "text": "src/app/chat_ui/hover.rs:39"}]}),
        (None,
         {"id": "function", "type": "functionCallOutput", "name": "read_notes",
          "output": "Notes: keep the copy control beside the header."}),
        ({**collab, "id": "message", "tool": "sendMessage", "prompt": "Check the docs too.",
          "status": "inProgress"},
         {**collab, "id": "message", "tool": "sendMessage", "prompt": "Check the docs too.",
          "status": "completed"}),
        ({**collab, "id": "interrupt", "tool": "interruptAgent", "status": "inProgress"},
         {**collab, "id": "interrupt", "tool": "interruptAgent", "status": "interrupted"}),
        ({"id": "sleep", "type": "sleep", "durationMs": 30_000},
         {"id": "sleep", "type": "sleep", "durationMs": 30_000}),
        ({"id": "activity", "type": "subAgentActivity", "kind": "started",
          "agentThreadId": child, "agentPath": "/root/review"},
         {"id": "activity", "type": "subAgentActivity", "kind": "completed",
          "agentThreadId": child, "agentPath": "/root/review"}),
        (None,
         {"id": "hook", "type": "hookPrompt", "fragments": [
             {"hookRunId": "run-1", "text": "Follow the repository style guide."},
             {"hookRunId": "run-2", "text": "Run the narrow tests before finishing."}]}),
        ({"id": "image", "type": "imageGeneration", "status": "inProgress",
          "inputImages": [], "revisedPrompt": "A fox reading tool rows"},
         {"id": "image", "type": "imageGeneration", "status": "failed", "inputImages": [],
          "revisedPrompt": "A fox reading tool rows",
          "failure": {"type": "usageLimitExceeded", "limitId": "images"}}),
        ({"id": "compact", "type": "contextCompaction"},
         {"id": "compact", "type": "contextCompaction"}),
    ]


class Scenario:
    def __init__(self):
        self.thread, self.child = str(uuid.uuid4()), str(uuid.uuid4())
        self.latest = None

    def metadata(self, thread):
        child = thread == self.child
        return {"id": thread, "name": "Child review" if child else "Offline tool rows",
                "parentThreadId": self.thread if child else None,
                "agentNickname": "Quinn" if child else None,
                "status": {"type": "idle"}, "canAcceptDirectInput": not child}

    def notifications(self, turn):
        def notify(method, **params):
            return {"method": method, "params": {"threadId": self.thread, **params}}
        started, completed = [], []
        for begin, end in items(self.child):
            if begin is not None:
                started.append(notify("item/started", turnId=turn, item=begin))
            completed.append(notify("item/completed", turnId=turn, item=end))
        diff = notify("turn/diff/updated", turnId=turn,
                      diff="diff --git a/README.md b/README.md\n@@ -1 +1 @@\n-old\n+new\n")
        settled = [end for _, end in items(self.child)]
        self.latest = {"id": turn, "status": "completed", "items": settled}
        answer = {"id": "answer", "type": "agentMessage", "text": "Done.", "phase": None}
        return [notify("turn/started", turn={"id": turn, "status": "inProgress", "items": []}),
                *started, diff, {"_sleep": PAUSE}, *completed,
                notify("item/completed", turnId=turn, item=answer),
                notify("turn/completed", turn={**self.latest,
                                               "items": [*settled, answer]})]

    def request(self, frame, codex_home):
        identifier, method = frame.get("id"), frame.get("method")
        params = frame.get("params", {})
        if identifier is None:
            return []
        after = []
        if method == "initialize":
            result = {"userAgent": "cagents-offline-tool-rows", "codexHome": codex_home}
        elif method in ("thread/start", "thread/resume"):
            thread = params.get("threadId", self.thread)
            if thread not in (self.thread, self.child):
                return [{"id": identifier, "error": {"code": -32602, "message": "Unknown fixture thread"}}]
            latest = self.latest if thread == self.thread else None
            result = {"thread": self.metadata(thread), "model": "fixture-mini",
                      "approvalPolicy": "on-request", "sandbox": {"type": "readOnly"},
                      "initialTurnsPage": {"data": [latest] if latest else [], "nextCursor": None}}
        elif method == "thread/read":
            result = {"thread": self.metadata(params["threadId"])}
        elif method == "model/list":
            result = {"data": [{"id": "fixture-mini", "model": "fixture-mini",
                       "displayName": "Offline fixture"}], "nextCursor": None}
        elif method == "thread/list":
            result = {"data": [self.metadata(self.child)], "nextCursor": None}
        elif method == "thread/turns/list":
            latest = self.latest if params.get("threadId") == self.thread else None
            result = {"data": [latest] if latest else [], "nextCursor": None}
        elif method in ("skills/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        elif method == "turn/start":
            if params.get("threadId") != self.thread:
                return [{"id": identifier, "error": {"code": -32602, "message": "Prompt the fixture thread"}}]
            turn = str(uuid.uuid4())
            result = {"turn": {"id": turn, "status": "inProgress", "items": []}}
            after = self.notifications(turn)
        elif method in ("thread/unsubscribe", "turn/interrupt"):
            result = {}
        else:
            return [{"id": identifier, "error": {"code": -32601, "message": "Unsupported offline fixture method"}}]
        return [{"id": identifier, "result": result}, *after]


def serve(connection, codex_home, scenario, request_lock):
    header = bytearray()
    while b"\r\n\r\n" not in header:
        header.extend(transport.exact(connection, 1))
        if len(header) > 16384:
            raise ValueError("fixture handshake exceeds limit")
    key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n")
               if line.lower().startswith(b"sec-websocket-key:"))
    accept = transport.base64.b64encode(transport.hashlib.sha1(
        key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    send = threading.Lock()
    while True:
        request = transport.receive(connection, lambda payload: connection.sendall(transport.frame(payload, opcode=10)))
        with request_lock:
            responses = scenario.request(request, codex_home)
        # A turn's later frames follow its pause on their own thread, so the
        # client's requests in the meantime are still answered.
        def play(responses=responses):
            for response in responses:
                if "_sleep" in response:
                    time.sleep(response["_sleep"])
                    continue
                with send:
                    connection.sendall(transport.frame(response))
        if any("_sleep" in response for response in responses):
            threading.Thread(target=play, daemon=True).start()
        else:
            play()


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.162.0 (offline tool-rows fixture)")
        return
    root = Path(os.environ["CAGENTS_TOOL_ROWS_FIXTURE_ROOT"]).resolve(strict=True)
    home = Path(os.environ["CODEX_HOME"]).resolve(strict=True)
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]).resolve(strict=True)
    for private in (root, runtime):
        if private.stat().st_uid != os.getuid() or private.stat().st_mode & 0o077:
            raise SystemExit("fixture requires private owned root/runtime directories")
    if not home.is_relative_to(root):
        raise SystemExit("fixture CODEX_HOME must be beneath its private root")
    args = sys.argv[1:]
    address = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--listen=")), None)
    if "--listen" in args:
        address = args[args.index("--listen") + 1]
    if not address or not address.startswith("unix://"):
        raise SystemExit("fixture requires --listen unix://PATH")
    path = Path(address[len("unix://"):])
    if not transport.private_listener(path.parent, runtime) or path.exists() or path.is_symlink():
        raise SystemExit("fixture requires a fresh socket beneath its private runtime")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen()
    scenario, request_lock = Scenario(), threading.Lock()
    while True:
        connection, _ = listener.accept()
        def worker(connection=connection):
            try:
                serve(connection, os.environ["CODEX_HOME"], scenario, request_lock)
            except (EOFError, ConnectionError):
                pass
            finally:
                connection.close()
        threading.Thread(target=worker, daemon=True).start()


if __name__ == "__main__":
    main()
