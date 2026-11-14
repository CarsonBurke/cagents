#!/usr/bin/env python3
"""Offline Codex peer that plays one turn of tool and event items.

Each prompt to the fixture's thread starts a turn that streams an MCP call, a
dynamic and a function tool, collaboration calls, a pause, a hook prompt,
image generation, a context compaction and a turn diff, first as started and
then, after a short pause, as completed. It exists to check how each item
reads in the native transcript, running and settled.

Set CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT=1 to acknowledge interruptions
without completing either the interrupted turn or its delayed successful script.
Set CAGENTS_TOOL_ROWS_NEVER_CONFIRM_CLOSE=1 to keep a WebSocket connection open
after a close frame until its peer disconnects, without changing turn settlement.
Set CAGENTS_TOOL_ROWS_HISTORY="COUNT BYTES" to open the thread's first turn
with COUNT earlier messages of about BYTES each, a long-running conversation.

Requires a private CAGENTS_TOOL_ROWS_FIXTURE_ROOT and CODEX_HOME beneath it,
and a private XDG_RUNTIME_DIR. Launch it through the ordinary development
runner as the isolated Codex executable. It models the app-server protocol
only: no provider, credentials, network service or tool is contacted.
"""
import copy
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


def long_history():
    """The earlier messages CAGENTS_TOOL_ROWS_HISTORY asks for, if any."""
    setting = os.environ.get("CAGENTS_TOOL_ROWS_HISTORY")
    if not setting:
        return []
    count, size = map(int, setting.split())
    phrase = "Reviewed the failing step, its logs and the last good run. "
    text = (phrase * (size // len(phrase) + 1))[:size]
    return [{"id": f"history-{at}", "type": "agentMessage", "text": f"Step {at}: {text}", "phase": None}
            for at in range(count)]


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
    settled_collab = {**collab, "agentsStates": {
        child: {"status": "completed", "agentNickname": "Quinn"}}}
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
         {**settled_collab, "id": "message", "tool": "sendMessage", "prompt": "Check the docs too.",
          "status": "completed"}),
        ({**collab, "id": "interrupt", "tool": "interruptAgent", "status": "inProgress"},
         {**settled_collab, "id": "interrupt", "tool": "interruptAgent", "status": "interrupted"}),
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
        self.child_latest = None
        self.model = "fixture-mini"
        self.reasoning = None
        self.never_confirm_interrupt = os.environ.get("CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT") == "1"
        self.never_confirm_close = os.environ.get("CAGENTS_TOOL_ROWS_NEVER_CONFIRM_CLOSE") == "1"
        # Replayed once, by the thread's first turn.
        self.history = long_history()
        self.interrupting = set()
        self._generation = 0
        self._generations = {}
        self._published = set()
        self._changed = threading.Condition()

    def _advance(self, thread):
        self._generation += 1
        self._generations[thread] = self._generation
        self._published = {entry for entry in self._published if entry[0] != thread}
        with self._changed:
            self._changed.notify_all()

    def state(self):
        with self._changed:
            return copy.deepcopy({"thread": self.thread, "child": self.child,
                                  "latest": self.latest, "child_latest": self.child_latest,
                                  "model": self.model, "reasoning": self.reasoning,
                                  "interrupting": sorted(self.interrupting)})

    def restore(self, state):
        """Loading another listener's history invalidates every old playback."""
        with self._changed:
            self._restore(state)

    def _restore(self, state):
        self.thread, self.child = state["thread"], state["child"]
        self.latest = copy.deepcopy(state["latest"])
        self.child_latest = copy.deepcopy(state.get("child_latest"))
        self.model = state.get("model", "fixture-mini")
        self.reasoning = state.get("reasoning")
        self.interrupting = set(map(tuple, state.get("interrupting", ())))
        self._generations.clear()
        for thread in (self.thread, self.child):
            self._advance(thread)

    def _latest(self, thread):
        if thread == self.thread:
            return self.latest
        if thread == self.child:
            return self.child_latest
        return None

    def _store(self, thread, turn):
        if thread == self.thread:
            self.latest = copy.deepcopy(turn)
        else:
            self.child_latest = copy.deepcopy(turn)

    def _child_state(self, item):
        """A parent tool's history cannot overwrite its child's actual outcome."""
        item = copy.deepcopy(item)
        if self.child_latest:
            status = self.child_latest["status"]
            active = status == "inProgress"
            if self.child in item.get("agentsStates", {}):
                item["agentsStates"][self.child]["status"] = "running" if active else status
            if item.get("type") == "subAgentActivity" and item.get("agentThreadId") == self.child:
                item["kind"] = "started" if active else "completed"
        return item

    def metadata(self, thread):
        with self._changed:
            return self._metadata(thread)

    def _metadata(self, thread):
        child = thread == self.child
        latest = self.child_latest if child else self.latest
        return {"id": thread, "name": "Child review" if child else "Offline tool rows",
                "parentThreadId": self.thread if child else None,
                "agentNickname": "Quinn" if child else None,
                "status": {"type": "active" if latest and latest["status"] == "inProgress"
                           else "idle"}, "canAcceptDirectInput": not child}

    def observe(self, notification, generations=None):
        """Accept a current frame, updating the history served on reconnection."""
        with self._changed:
            return self._observe(notification, generations)

    def _observe(self, notification, generations):
        method = notification.get("method")
        if method is None:
            return True  # Correlated responses are never cancelled.
        params = notification.get("params", {})
        thread = params.get("threadId")
        if thread not in (self.thread, self.child):
            return False
        if generations is not None and generations.get(thread) != self._generations.get(thread):
            return False
        latest = self._latest(thread)
        turn_id = params.get("turnId", params.get("turn", {}).get("id"))
        if turn_id is not None and (latest is None or latest["id"] != turn_id):
            return False
        # An interrupt request fences scripted success immediately. History
        # becomes settled only when its exact interrupted receipt is published.
        interrupted_receipt = (method == "turn/completed"
                               and params["turn"]["status"] == "interrupted")
        if turn_id is not None and (thread, turn_id) in self.interrupting and not interrupted_receipt:
            return False
        if method in ("turn/started", "turn/completed"):
            turn = copy.deepcopy(params["turn"])
            if (method == "turn/started" and latest["status"] != "inProgress"
                    or method == "turn/completed" and latest["status"] != "inProgress"
                    and latest != turn):
                return False
            key = (thread, turn_id, method)
            if key in self._published:
                return False
            if thread == self.thread:
                turn["items"] = [self._child_state(item) for item in turn["items"]]
            params["turn"] = turn
            self._store(thread, turn)
            self._published.add(key)
            if thread == self.child and self.latest:
                self.latest["items"] = [self._child_state(item) for item in self.latest["items"]]
            self._changed.notify_all()
        elif turn_id is not None:
            if latest["status"] != "inProgress":
                return False
            if method in ("item/started", "item/completed"):
                item = self._child_state(params["item"]) if thread == self.thread else params["item"]
                params["item"] = item
                present = next((index for index, old in enumerate(latest["items"])
                                if old["id"] == item["id"]), None)
                if present is None:
                    latest["items"].append(copy.deepcopy(item))
                else:
                    latest["items"][present] = copy.deepcopy(item)
        elif method == "thread/status/changed":
            if params["status"] != self.metadata(thread)["status"]:
                return False
        return True

    def playback(self, responses):
        with self._changed:
            return {response["params"]["threadId"]: self._generations.get(response["params"]["threadId"])
                    for response in responses if response.get("method") and "threadId" in response.get("params", {})}

    def _pause(self, seconds, generations):
        deadline = time.monotonic() + seconds
        with self._changed:
            while True:
                if not any(self._generations.get(thread) == generation
                           and self._latest(thread) and self._latest(thread)["status"] == "inProgress"
                           for thread, generation in generations.items()):
                    return False
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return True
                self._changed.wait(remaining)

    def play(self, responses, publish, request_lock, generations):
        """Publish only current frames; stop a cancelled delayed turn promptly."""
        for response in responses:
            if "_sleep" in response:
                if not self._pause(response["_sleep"], generations):
                    return
            else:
                # Fencing, history publication and the wire send share the
                # request lock, so interruption cannot race a late success.
                with request_lock:
                    if self.observe(response, generations):
                        publish(response)

    def notifications(self, turn, starts_child=True):
        def notify(method, **params):
            return {"method": method, "params": {"threadId": self.thread, **params}}
        started, completed = [], []
        for begin, end in items(self.child):
            if begin is not None:
                started.append(notify("item/started", turnId=turn, item=begin))
            completed.append(notify("item/completed", turnId=turn, item=end))
        diff = notify("turn/diff/updated", turnId=turn,
                      diff="diff --git a/README.md b/README.md\n@@ -1 +1 @@\n-old\n+new\n")
        history, self.history = self.history, []
        replayed = [notify("item/completed", turnId=turn, item=item) for item in history]
        settled = [*history, *(end for _, end in items(self.child))]
        finished = {"id": turn, "status": "completed", "items": settled}
        answer = {"id": "answer", "type": "agentMessage", "text": "Done.", "phase": None}
        child_started = [notify("turn/started", threadId=self.child,
                               turn={"id": f"{turn}-child", "status": "inProgress", "items": []})] if starts_child else []
        child_completed = [notify("turn/completed", threadId=self.child,
                                 turn={"id": f"{turn}-child", "status": "completed", "items": []}),
                           notify("thread/status/changed", threadId=self.child, status={"type": "idle"})] if starts_child else []
        return [notify("turn/started", turn={"id": turn, "status": "inProgress", "items": []}),
                *replayed,
                *child_started,
                *started, diff, {"_sleep": PAUSE}, *completed,
                # A subAgentActivity item only renders a row. The child needs
                # its own lifecycle receipt, as an actual app-server sends.
                *child_completed,
                notify("item/completed", turnId=turn, item=answer),
                notify("turn/completed", turn={**finished,
                                               "items": [*settled, answer]})]

    def request(self, frame, codex_home):
        with self._changed:
            return self._request(frame, codex_home)

    def _request(self, frame, codex_home):
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
            latest = self.latest if thread == self.thread else self.child_latest
            if method == "thread/start":
                catalog = self.models(codex_home)["data"]
                default = next((model for model in catalog if model.get("isDefault")), catalog[0])
                self.model = params.get("model") or default["model"]
                self.reasoning = params.get("config", {}).get("model_reasoning_effort")
            result = {"thread": self.metadata(thread), "model": self.model,
                      "reasoningEffort": self.reasoning,
                      "approvalPolicy": "on-request", "sandbox": {"type": "readOnly"},
                      "initialTurnsPage": {"data": [latest] if latest else [], "nextCursor": None}}
        elif method == "thread/read":
            result = {"thread": self.metadata(params["threadId"])}
        elif method == "model/list":
            result = self.models(codex_home)
        elif method == "thread/list":
            result = {"data": [self.metadata(self.child)], "nextCursor": None}
        elif method == "thread/turns/list":
            latest = self.latest if params.get("threadId") == self.thread else self.child_latest
            result = {"data": [latest] if latest else [], "nextCursor": None}
        elif method == "thread/prediction/request":
            # Prediction is optional; a successful request can produce no next
            # prompt. Keep the response and its notification on the real wire.
            result = {}
            after = [{"method": "thread/prediction/updated", "params": {
                "threadId": params["threadId"], "sourceTurnId": params["sourceTurnId"],
                "result": {"type": "completed", "text": None}}}]
        elif method in ("skills/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        elif method == "account/rateLimits/read":
            # Optional usage: CODEX_HOME/fixture-rate-limits.json holds the
            # result, such as fixtures/accounts/codex-rate-limits-read.json's.
            limits = Path(codex_home) / "fixture-rate-limits.json"
            if not limits.is_file():
                return [{"id": identifier, "error": {"code": -32601, "message": "The fixture reports no usage"}}]
            result = json.loads(limits.read_text())
        elif method == "turn/start":
            if params.get("threadId") != self.thread:
                return [{"id": identifier, "error": {"code": -32602, "message": "Prompt the fixture thread"}}]
            if (self.latest and self.latest["status"] == "inProgress"
                    or self.child_latest and self.child_latest["status"] == "inProgress"
                    and (self.child, self.child_latest["id"]) in self.interrupting):
                return [{"id": identifier, "error": {"code": -32602, "message": "Fixture turn is still active"}}]
            if params.get("model"):
                self.model = params["model"]
            if "effort" in params:
                self.reasoning = params["effort"]
            turn = str(uuid.uuid4())
            result = {"turn": {"id": turn, "status": "inProgress", "items": []}}
            self.latest = copy.deepcopy(result["turn"])
            self._advance(self.thread)
            starts_child = not self.child_latest or self.child_latest["status"] != "inProgress"
            if starts_child:
                self.child_latest = {"id": f"{turn}-child", "status": "inProgress", "items": []}
                self._advance(self.child)
            after = self.notifications(turn, starts_child)
        elif method == "turn/interrupt":
            thread = params.get("threadId")
            if thread not in (self.thread, self.child):
                return [{"id": identifier, "error": {"code": -32602, "message": "Unknown fixture thread"}}]
            latest = self._latest(thread)
            if (latest is None or params.get("turnId") != latest["id"]
                    or latest["status"] != "inProgress"):
                return [{"id": identifier, "error": {"code": -32602, "message": "No matching active fixture turn"}}]
            self.interrupting.add((thread, latest["id"]))
            self._advance(thread)
            if not self.never_confirm_interrupt:
                interrupted = copy.deepcopy(latest)
                interrupted["status"] = "interrupted"
                for item in interrupted["items"]:
                    if item.get("status") == "inProgress":
                        item["status"] = "interrupted"
                after = [{"method": "turn/completed", "params": {"threadId": thread, "turn": interrupted}},
                         {"method": "thread/status/changed", "params": {"threadId": thread, "status": {"type": "idle"}}}]
            result = {}
        elif method == "thread/unsubscribe":
            result = {}
        else:
            return [{"id": identifier, "error": {"code": -32601, "message": "Unsupported offline fixture method"}}]
        return [{"id": identifier, "result": result}, *after]

    def models(self, codex_home):
        """Optional recorded model/list result, private to this fixture's home."""
        models = Path(codex_home) / "fixture-models.json"
        if models.is_file():
            catalog = json.loads(models.read_text())
            if not isinstance(catalog.get("data"), list) or not catalog["data"]:
                raise ValueError("fixture-models.json must hold a nonempty model/list result")
            if any(not isinstance(model.get("model"), str) or not model["model"]
                   for model in catalog["data"]):
                raise ValueError("every fixture-models.json model must have a nonempty model ID")
            return catalog
        return {"data": [{"id": "fixture-mini", "model": "fixture-mini",
                          "displayName": "Offline fixture"}], "nextCursor": None}


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
        try:
            request = transport.receive(connection, lambda payload: connection.sendall(transport.frame(payload, opcode=10)))
        except EOFError:
            if scenario.never_confirm_close:
                # A Close frame leaves the transport connected. Withhold its
                # handshake until the peer disconnects; EOF returns immediately.
                while connection.recv(65536):
                    pass
            raise
        with request_lock:
            responses = scenario.request(request, codex_home)
            generations = scenario.playback(responses)
        # A turn's later frames follow its pause on their own thread, so the
        # client's requests in the meantime are still answered.
        def play(responses=responses, generations=generations):
            def publish(response):
                with send:
                    connection.sendall(transport.frame(response))
            try:
                scenario.play(responses, publish, request_lock, generations)
            except (EOFError, ConnectionError):
                pass
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
    if not address and "app-server" in args:
        # The app's account check: one request per line on stdio, answered
        # in turn, until the check closes its end.
        scenario = Scenario()
        for line in sys.stdin:
            for response in scenario.request(json.loads(line), str(home)):
                if "_sleep" not in response:
                    print(json.dumps(response), flush=True)
        return
    if not address or not address.startswith("unix://"):
        raise SystemExit("fixture requires --listen unix://PATH or stdio app-server")
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
