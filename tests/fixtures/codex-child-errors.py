#!/usr/bin/env python3
"""Offline Codex child-error peer using the existing fixture WebSocket framing.

Use a private CAGENTS_CHILD_ERROR_FIXTURE_ROOT, CODEX_HOME beneath that root,
and a private XDG_RUNTIME_DIR. Launch through the ordinary native development
runner with this script as the isolated Codex executable. Default prompts emit
false/null/missing-retry child diagnostics before an authentic failed turn;
`failed-turn`, `retry`, and `disconnect` select the neighboring cases. This
models protocol/UI behavior only, not provider execution or account recovery.
One fixture process owns one synthetic parent conversation.
"""
import importlib.util
import json
import os
from pathlib import Path
import socket
import sys
import threading
import uuid

sys.dont_write_bytecode = True
_source = Path(__file__).with_name("terminal-escape-codex.py")
_spec = importlib.util.spec_from_file_location("codex_fixture_transport", _source)
transport = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(transport)


class Scenario:
    def __init__(self):
        self.parent, self.child, self.sibling = (str(uuid.uuid4()) for _ in range(3))
        self.turns = {}
        self.latest = {}

    def metadata(self, thread):
        child = thread != self.parent
        return {"id": thread, "name": "Offline child diagnostics" if not child else "Child work",
                "parentThreadId": self.parent if child else None,
                "agentNickname": "Quinn" if thread == self.child else "Nora",
                "status": {"type": "idle"}, "canAcceptDirectInput": not child}

    def notifications(self, text, parent_turn):
        def notify(method, thread, **params):
            return {"method": method, "params": {"threadId": thread, **params}}
        def turn(thread, status, items=None, error=None):
            previous = self.latest.get(thread, {})
            retained = previous.get("items", []) if previous.get("id") == self.turns[thread] else []
            value = {"id": self.turns[thread], "status": status,
                     "items": list(retained if items is None else items)}
            if error is not None:
                value["error"] = {"message": error}
            self.latest[thread] = value
            return value
        self.turns[self.parent] = parent_turn
        frames = [notify("turn/started", self.parent, turn=turn(self.parent, "inProgress"))]
        for thread in (self.child, self.sibling):
            self.turns[thread] = str(uuid.uuid4())
            frames.append(notify("turn/started", thread, turn=turn(thread, "inProgress", [
                {"id": "answer", "type": "agentMessage", "text": "Original child work"}])))
        raw = ("You've hit your session limit · resets 3:40pm (America/Vancouver)\n"
               "request-id: offline-child-original\nOriginal server diagnostics λ")
        if "disconnect" in text:
            frames.append(notify("thread/closed", self.child))
        elif "failed-turn" in text:
            frames.append(notify("turn/completed", self.child,
                                 turn=turn(self.child, "failed", error=raw)))
        else:
            retry = "retry" in text
            for flag in ([True] if retry else [False, None, "missing"]):
                params = {"threadId": self.child, "turnId": self.turns[self.child],
                          "error": {"message": "Reconnecting... 1/2" if retry else raw}}
                if flag != "missing":
                    params["willRetry"] = flag
                frames.append({"method": "error", "params": params})
            frames.append(notify("turn/completed", self.child,
                                 turn=turn(self.child, "completed" if retry else "failed")))
        # Parent and sibling settle by their own genuine lifecycle frames.
        for thread in (self.sibling, self.parent):
            frames.append(notify("turn/completed", thread, turn=turn(thread, "completed")))
        return frames

    def request(self, frame, codex_home):
        identifier, method = frame.get("id"), frame.get("method")
        params = frame.get("params", {})
        if identifier is None:
            return []
        after = []
        if method == "initialize":
            result = {"userAgent": "cagents-offline-child-errors", "codexHome": codex_home}
        elif method in ("thread/start", "thread/resume"):
            thread = params.get("threadId", self.parent)
            if thread not in (self.parent, self.child, self.sibling):
                return [{"id": identifier, "error": {"code": -32602, "message": "Unknown fixture thread"}}]
            latest = self.latest.get(thread)
            result = {"thread": self.metadata(thread), "model": "fixture-mini",
                      "approvalPolicy": "on-request", "sandbox": {"type": "readOnly"},
                      "initialTurnsPage": {"data": [latest] if latest else [], "nextCursor": None}}
        elif method == "thread/read":
            result = {"thread": self.metadata(params["threadId"])}
        elif method == "model/list":
            result = {"data": [{"id": "fixture-mini", "model": "fixture-mini",
                       "displayName": "Offline fixture"}], "nextCursor": None}
        elif method == "thread/list":
            result = {"data": [self.metadata(t) for t in (self.child, self.sibling)], "nextCursor": None}
        elif method == "thread/turns/list":
            latest = self.latest.get(params.get("threadId"))
            result = {"data": [latest] if latest else [], "nextCursor": None}
        elif method in ("skills/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        elif method == "turn/start":
            if params.get("threadId") != self.parent:
                return [{"id": identifier, "error": {"code": -32602, "message": "Prompt the fixture parent"}}]
            turn = str(uuid.uuid4())
            result = {"turn": {"id": turn, "status": "inProgress", "items": []}}
            after = self.notifications(json.dumps(params.get("input", [])), turn)
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
    while True:
        request = transport.receive(connection, lambda payload: connection.sendall(transport.frame(payload, opcode=10)))
        with request_lock:
            responses = scenario.request(request, codex_home)
        for response in responses:
            connection.sendall(transport.frame(response))


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.162.0 (offline child-error fixture)")
        return
    root = Path(os.environ["CAGENTS_CHILD_ERROR_FIXTURE_ROOT"]).resolve(strict=True)
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
    if not path.parent.resolve(strict=True).is_relative_to(runtime) or path.exists() or path.is_symlink():
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
