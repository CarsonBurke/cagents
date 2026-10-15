#!/usr/bin/env python3
"""Offline Unix-WebSocket Codex peer for streaming escape-state validation.

Requires a private CAGENTS_ESCAPE_FIXTURE_ROOT and CODEX_HOME beneath it. The
native validator must observe the running command before creating
ROOT/complete-stream; completed aggregatedOutput cannot establish Stream proof.
No commercial provider, credentials, network service or daemon is contacted.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import sys
import threading
import time
import uuid

FAMILIES = (("osc", "\x1b]", "\x1b\\", "x"),
            ("dcs", "\x1bP", "\x1b\\", "x"),
            ("apc", "\x1b_", "\x1b\\", "x"),
            ("sos", "\x1bX", "\x1b\\", "x"),
            ("pm", "\x1b^", "\x07", "x"),
            ("csi", "\x1b[", "m", "1;"),
            ("esc", "\x1b(", "B", " "))
EXPECTED = "".join(f"{name} stream sentinel\n" for name, *_ in FAMILIES)


def exact(connection, count):
    value = bytearray()
    while len(value) < count:
        part = connection.recv(count - len(value))
        if not part:
            raise EOFError
        value.extend(part)
    return bytes(value)


def receive(connection, pong):
    fragments = bytearray()
    while True:
        first, second = exact(connection, 2)
        length = second & 127
        if length == 126:
            length = struct.unpack("!H", exact(connection, 2))[0]
        elif length == 127:
            length = struct.unpack("!Q", exact(connection, 8))[0]
        if length > 1024 * 1024:
            raise ValueError("fixture request exceeds 1 MiB")
        mask = exact(connection, 4) if second & 128 else b""
        value = exact(connection, length)
        if mask:
            value = bytes(byte ^ mask[index % 4] for index, byte in enumerate(value))
        opcode = first & 15
        if opcode == 8:
            raise EOFError
        if opcode == 9:
            pong(value)
            continue
        if opcode == 10:
            continue
        if opcode not in (0, 1):
            raise ValueError("fixture expects text WebSocket frames")
        fragments.extend(value)
        if len(fragments) > 1024 * 1024:
            raise ValueError("fixture fragmented request exceeds 1 MiB")
        if first & 128:
            return json.loads(fragments)


def frame(value, opcode=1):
    payload = value if isinstance(value, bytes) else json.dumps(value, separators=(",", ":")).encode()
    length = len(payload)
    header = bytes([0x80 | opcode])
    if length < 126:
        header += bytes([length])
    elif length < 65536:
        header += bytes([126]) + struct.pack("!H", length)
    else:
        header += bytes([127]) + struct.pack("!Q", length)
    return header + payload


def atomic_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def run_stream(send, root, thread_id, payload_bytes):
    turn_id = "escape-turn"
    def notify(method, params):
        send({"method": method, "params": {"threadId": thread_id, "turnId": turn_id, **params}})
    notify("turn/started", {"turn": {"id": turn_id, "status": "inProgress", "items": []}})
    command = {"id": "escape-command", "type": "commandExecution",
               "command": "offline fragmented terminal escape fixture", "cwd": str(root),
               "status": "inProgress", "aggregatedOutput": ""}
    notify("item/started", {"item": command})
    emitted = []
    def delta(text):
        notify("item/commandExecution/outputDelta", {"itemId": command["id"], "delta": text})
    for name, prefix, terminator, unit in FAMILIES:
        # Both ESC introducers and ST terminators cross actual RPC boundaries.
        for character in prefix:
            delta(character)
        chunk = (unit * (1024 // len(unit)))
        remaining = payload_bytes
        chunks = 0
        while remaining:
            piece = chunk[:remaining]
            delta(piece)
            remaining -= len(piece)
            chunks += 1
        for character in terminator:
            delta(character)
        sentinel = f"{name} stream sentinel\n"
        delta(sentinel)
        emitted.append({"family": name, "payload_bytes": payload_bytes,
                        "payload_delta_count": chunks, "split_prefix": True,
                        "split_terminator": len(terminator) > 1})
    # This receipt is written after all actual outputDelta sends. No aggregate
    # item or completed turn exists until the independent native proof opens it.
    atomic_json(root / "stream-paused.json", {"pid": os.getpid(), "stage": "pre-aggregate",
        "thread_id": thread_id, "item_id": command["id"], "families": emitted,
        "expected_stream_output": EXPECTED, "completed_aggregate_sent": False})
    deadline = time.monotonic() + 120
    while not (root / "complete-stream").is_file():
        if time.monotonic() >= deadline:
            raise TimeoutError("native stream proof did not release the aggregate gate")
        time.sleep(0.02)
    # Deliberately disagree with the streaming text: a native assertion made
    # after this point cannot accidentally pass using whole-output cleanup.
    command.update(status="completed", exitCode=0, durationMs=100,
                   aggregatedOutput="COMPLETED AGGREGATE REPLACED STREAM\n")
    notify("item/completed", {"item": command})
    notify("turn/completed", {"turn": {"id": turn_id, "status": "completed", "items": []}})
    atomic_json(root / "stream-completed.json", {"stage": "completed", "completed_aggregate_sent": True})


def serve(connection, root, payload_bytes, start_lock, stream_started):
    header = bytearray()
    while b"\r\n\r\n" not in header:
        header.extend(exact(connection, 1))
        if len(header) > 16384:
            raise ValueError("fixture handshake exceeds limit")
    key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n")
               if line.lower().startswith(b"sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    write_lock = threading.Lock()
    def send(value, opcode=1):
        with write_lock:
            connection.sendall(frame(value, opcode))
    thread_id = str(uuid.uuid4())
    while True:
        request = receive(connection, lambda payload: send(payload, opcode=10))
        with (root / "rpc-intake.jsonl").open("a") as log:
            log.write(json.dumps(request, separators=(",", ":")) + "\n")
        identifier = request.get("id")
        method = request.get("method")
        params = request.get("params", {})
        if identifier is None:
            continue
        result = {}
        if method == "initialize":
            result = {"userAgent": "cagents/0.162.0", "codexHome": os.environ["CODEX_HOME"]}
        elif method in ("thread/start", "thread/resume"):
            thread_id = params.get("threadId", thread_id)
            result = {"thread": {"id": thread_id, "name": "Offline terminal stream", "status": {"type": "idle"}, "canAcceptDirectInput": True},
                      "model": "gpt-5.4-mini", "reasoningEffort": "low", "approvalPolicy": "never",
                      "sandbox": {"type": "dangerFullAccess"}, "initialTurnsPage": {"data": [], "nextCursor": None}}
        elif method == "model/list":
            result = {"data": [{"id": "fixture-mini", "model": "gpt-5.4-mini", "displayName": "Offline fixture mini",
                       "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "Fixture"}], "defaultReasoningEffort": "low"}], "nextCursor": None}
        elif method in ("skills/list", "thread/list", "thread/turns/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        elif method == "turn/start":
            with start_lock:
                if stream_started.is_set():
                    raise ValueError("one streaming turn per fresh fixture process")
                stream_started.set()
            result = {"turn": {"id": "escape-turn", "status": "inProgress", "items": []}}
        send({"id": identifier, "result": result})
        if method == "turn/start":
            def worker(active_thread=thread_id):
                try:
                    run_stream(send, root, active_thread, payload_bytes)
                except Exception as error:
                    atomic_json(root / "stream-failed.json", {"error": str(error)})
            threading.Thread(target=worker, daemon=True).start()


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.162.0 (offline terminal stream fixture)")
        return
    root = Path(os.environ["CAGENTS_ESCAPE_FIXTURE_ROOT"]).resolve(strict=True)
    home = Path(os.environ["CODEX_HOME"]).resolve(strict=True)
    if not home.is_relative_to(root) or root.stat().st_uid != os.getuid() or root.stat().st_mode & 0o077:
        raise SystemExit("fixture requires a private owned root and CODEX_HOME beneath it")
    args = sys.argv[1:]
    address = next((argument.split("=", 1)[1] for argument in args if argument.startswith("--listen=")), None)
    if "--listen" in args:
        address = args[args.index("--listen") + 1]
    if not address or not address.startswith("unix://"):
        raise SystemExit("fixture requires --listen unix://PATH")
    path = Path(address[len("unix://"):])
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]).resolve(strict=True)
    if not path.parent.resolve(strict=True).is_relative_to(runtime) or runtime.stat().st_uid != os.getuid() or runtime.stat().st_mode & 0o077:
        raise SystemExit("fixture socket must live beneath the private owned runtime")
    if path.exists() or path.is_symlink():
        raise SystemExit("fixture never replaces an existing socket")
    payload_bytes = int(os.environ.get("CAGENTS_ESCAPE_FIXTURE_BYTES", str(2 * 1024 * 1024)))
    if not 1 <= payload_bytes <= 2 * 1024 * 1024:
        raise SystemExit("fixture payload must be between 1 byte and 2 MiB per family")
    for name in ("provider.json", "stream-paused.json", "stream-completed.json", "stream-failed.json", "complete-stream", "rpc-intake.jsonl"):
        if (root / name).exists() or (root / name).is_symlink():
            raise SystemExit("use a fresh fixture root without previous stream proof/gates")
    atomic_json(root / "provider.json", {"pid": os.getpid(), "payload_bytes_per_family": payload_bytes,
        "start_ticks": Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19]})
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen()
    start_lock, stream_started = threading.Lock(), threading.Event()
    while True:
        connection, _ = listener.accept()
        def worker(connection=connection):
            try:
                serve(connection, root, payload_bytes, start_lock, stream_started)
            except (EOFError, ConnectionError):
                pass
            finally:
                connection.close()
        threading.Thread(target=worker, daemon=True).start()


if __name__ == "__main__":
    main()
