#!/usr/bin/env python3
"""Offline Codex app-server for exact failure chronology, never a real login.

Requires a private synthetic CODEX_HOME with fixture-chronology-enabled. Supports
stdio and Unix-WebSocket transports. All displayed commands/results are data;
no tool or commercial provider is invoked. See docs/chat-chronology-fixture.md.
"""

import base64
import hashlib
import json
import os
from pathlib import Path
import runpy
import socket
import sys
import threading
import time
import uuid


DIAGNOSTIC = (
    "Selected model is at capacity. Please try a different model.\n"
    "request-id: synthetic-capacity-incident\n"
    "Original provider diagnostics remain available."
)
MODEL = "gpt-5.4-mini"
# Reuse the already exercised bounded WebSocket framing and ping handling.
WIRE = runpy.run_path(str(Path(__file__).resolve().with_name("terminal-escape-codex.py")))


class Fixture:
    def __init__(self, home, cwd, delay=None):
        self.home = Path(home).resolve(strict=True)
        if (not (self.home / "fixture-chronology-enabled").is_file()
                or self.home.stat().st_uid != os.getuid() or self.home.stat().st_mode & 0o077):
            raise ValueError("fixture requires a private owned synthetic CODEX_HOME and enable marker")
        self.path = self.home / "fixture-chronology-history.json"
        self.data = {"kind": "offline-chronology-v1", "threadId": str(uuid.uuid4()),
                     "cwd": str(Path(cwd).resolve()), "turns": [],
                     "approvalPolicy": "on-request", "sandbox": {"type": "workspaceWrite"}}
        self.refresh()
        delay_path = self.home / "fixture-chronology-delay"
        self.delay = delay if delay is not None else float(delay_path.read_text()) if delay_path.exists() else 1.0
        if not 0 <= self.delay <= 5:
            raise ValueError("fixture stream delay must be between zero and five seconds")

    def refresh(self):
        if self.path.exists():
            self.data = json.loads(self.path.read_text())
            if self.data.get("kind") != "offline-chronology-v1":
                raise ValueError("refusing a nonfixture history file")

    def save(self):
        temporary = self.path.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.data, ensure_ascii=False) + "\n")
        temporary.replace(self.path)
        # The native /resume catalog reads provider-owned rollout metadata;
        # complete turn hydration is served through thread/turns/list below.
        rollout = self.home / "sessions" / f"rollout-2026-11-01-{self.data['threadId']}.jsonl"
        rollout.parent.mkdir(exist_ok=True)
        rows = [{"type": "session_meta", "payload": {"id": self.data["threadId"],
                 "cwd": self.data["cwd"], "source": "cli"}}]
        for turn in self.data["turns"]:
            prompt = turn["items"][0]["content"]
            rows.append({"type": "event_msg", "payload": {"type": "user_message",
                         "message": "".join(part.get("text", "") for part in prompt)}})
        rollout.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows))

    def thread(self):
        return {"id": self.data["threadId"], "name": "Offline capacity chronology",
                "cwd": self.data["cwd"], "status": {"type": "idle"}, "canAcceptDirectInput": True}

    def page(self):
        return {"data": list(reversed(self.data["turns"])), "nextCursor": None}

    def run_turn(self, send, turn):
        def notify(method, **params):
            send({"method": method, "params": {"threadId": self.data["threadId"],
                  "turnId": turn["id"], **params}})
        notify("turn/started", turn=turn)
        if len(self.data["turns"]) in (1, 3):
            # Exact duplicate errors/completions describe one incident. The
            # later turn uses a new turn ID with byte-identical diagnostics.
            for _ in range(2):
                notify("error", willRetry=False, error={"message": DIAGNOSTIC})
            turn.update(status="failed", error={"message": DIAGNOSTIC})
            self.save()
            for _ in range(2):
                notify("turn/completed", turn=turn)
            return
        parts = ["Recovery resumed. ", "I am checking the saved state. ",
                 "The earlier failure should remain above your follow-up."]
        answer = {"id": "answer", "type": "agentMessage", "text": ""}
        notify("item/started", item=answer)
        for part in parts:
            notify("item/agentMessage/delta", itemId=answer["id"], delta=part)
            time.sleep(self.delay)
        answer["text"] = "".join(parts)
        turn["items"].append(answer)
        notify("item/completed", item=answer)
        command = {"id": "check", "type": "commandExecution", "status": "inProgress",
                   "command": "printf 'Synthetic recovery check passed\\n'", "cwd": self.data["cwd"],
                   "aggregatedOutput": "", "commandActions": []}
        notify("item/started", item=command)
        for part in ("Synthetic recovery ", "check passed\n"):
            notify("item/commandExecution/outputDelta", itemId=command["id"], delta=part)
            time.sleep(self.delay)
        command.update(status="completed", aggregatedOutput="Synthetic recovery check passed\n", exitCode=0, durationMs=100)
        turn["items"].append(command)
        notify("item/completed", item=command)
        followup = {"id": "followup", "type": "agentMessage", "text": "The synthetic recovery check passed. This turn is complete."}
        turn["items"].append(followup)
        notify("item/completed", item=followup)
        turn["status"] = "completed"
        self.save()
        notify("turn/completed", turn=turn)

    def request(self, request, send):
        if "id" not in request:
            return
        method, params = request.get("method"), request.get("params") or {}
        result = {}
        turn = None
        if method == "initialize":
            result = {"userAgent": "codex/0.162.0 offline-chronology", "codexHome": str(self.home)}
        elif method in ("thread/start", "thread/resume"):
            self.refresh()
            if method == "thread/resume" and params.get("threadId") != self.data["threadId"]:
                send({"id": request["id"], "error": {"code": -32602, "message": "fixture requires the exact saved thread"}})
                return
            # Reflect requested native restrictions; never advertise an
            # approval bypass or weaker sandbox to make a test pass.
            if method == "thread/start":
                self.data["cwd"] = params.get("cwd", self.data["cwd"])
                self.data["approvalPolicy"] = params.get("approvalPolicy", "on-request")
                sandbox = params.get("sandbox", "workspace-write")
                self.data["sandbox"] = {"type": "readOnly" if sandbox == "read-only" else "workspaceWrite"}
            result = {"thread": self.thread(), "model": MODEL, "reasoningEffort": "low",
                      "approvalPolicy": self.data["approvalPolicy"], "sandbox": self.data["sandbox"],
                      "initialTurnsPage": self.page()}
        elif method == "thread/read":
            self.refresh()
            result = {"thread": self.thread()}
        elif method == "thread/turns/list":
            self.refresh()
            result = self.page()
        elif method == "model/list":
            result = {"data": [{"id": "fixture-mini", "model": MODEL, "displayName": "Offline chronology fixture",
                      "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "Fixture"}],
                      "defaultReasoningEffort": "low"}], "nextCursor": None}
        elif method in ("skills/list", "thread/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@example.test", "planType": "free"}}
        elif method == "turn/start":
            self.refresh()
            if params.get("threadId") != self.data["threadId"]:
                raise ValueError("fixture cannot start a turn on another thread")
            turn = {"id": f"chronology-turn-{len(self.data['turns']) + 1}", "status": "inProgress", "items": [
                    {"id": "prompt", "type": "userMessage", "content": params["input"]}]}
            self.data["turns"].append(turn)
            self.save()
            result = {"turn": turn}
        send({"id": request["id"], "result": result})
        if turn is not None:
            self.run_turn(send, turn)


def serve(connection, fixture):
    header = bytearray()
    while b"\r\n\r\n" not in header:
        header.extend(WIRE["exact"](connection, 1))
        if len(header) > 16384:
            raise ValueError("fixture handshake exceeds limit")
    key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n")
               if line.lower().startswith(b"sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    def send(value, opcode=1):
        connection.sendall(WIRE["frame"](value, opcode))
    while True:
        fixture.request(WIRE["receive"](connection, lambda value: send(value, 10)), send)


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.162.0 (offline chronology fixture)")
        return
    os.umask(0o077)
    home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    # Validate the synthetic marker before binding a socket or reading requests.
    fixture = Fixture(home, Path.cwd())
    args = sys.argv[1:]
    address = next((value.split("=", 1)[1] for value in args if value.startswith("--listen=")), None)
    if "--listen" in args:
        address = args[args.index("--listen") + 1]
    if address is None:
        for line in sys.stdin:
            fixture.request(json.loads(line), lambda value: print(json.dumps(value), flush=True))
        return
    if not address.startswith("unix://"):
        raise ValueError("fixture only listens on a private Unix socket")
    path = Path(address[len("unix://"):])
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]).resolve(strict=True)
    if (not path.parent.resolve(strict=True).is_relative_to(runtime) or runtime.stat().st_uid != os.getuid()
            or runtime.stat().st_mode & 0o077 or path.exists() or path.is_symlink()):
        raise ValueError("fixture requires a fresh socket within the private owned runtime")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen()
    while True:
        connection, _ = listener.accept()
        def worker(connection=connection):
            try:
                serve(connection, Fixture(home, Path.cwd()))
            except (EOFError, ConnectionError):
                pass
            finally:
                connection.close()
        threading.Thread(target=worker, daemon=True).start()


if __name__ == "__main__":
    main()
