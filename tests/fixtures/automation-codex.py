#!/usr/bin/env python3
"""Offline app-server peer for the delayed automation archive contract.

The reported version exercises the allowlisted decoder path, not qualification
of a real Codex release. No model/API connection or synthetic Holder.confirm
is used. Both gates must be explicitly released by the owning Rust fixture.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import stat
import struct
import sys
import threading
import time
import uuid

if "--version" in sys.argv:
    print("codex-cli 0.162.0")
    raise SystemExit

home = Path(os.environ["HOME"])
root = home.parent
if not root.is_absolute() or root == Path("/") or root.is_symlink():
    raise SystemExit("fixture requires an absolute private test root")
info = root.stat()
if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
    raise SystemExit("fixture test root must be owned and private")
marker = root / "automation-codex-fixture"
if marker.is_symlink() or not marker.is_file():
    raise SystemExit("fixture root marker is missing")
uuid.UUID(marker.read_text().strip())
account = Path(os.environ.get("CODEX_HOME", home / ".codex"))
if account.resolve() != (home / ".codex").resolve():
    raise SystemExit("fixture account must be inside its private test root")
args = sys.argv[1:]
address = next((args[i + 1] for i, arg in enumerate(args[:-1]) if arg == "--listen"), None)
address = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--listen=")), address)
if not address or not address.startswith("unix://"):
    raise SystemExit("fixture requires its daemon-issued Unix listener")
path = Path(address[7:])
if not path.is_absolute() or ".." in path.parts or not path.parent.resolve().is_relative_to((root / "tmp").resolve()):
    raise SystemExit("fixture listener must remain inside its private TMPDIR")
if path.exists() or path.is_symlink():
    raise SystemExit("fixture never replaces a listener")
# A failed parent test cannot leave the peer alive indefinitely.
threading.Timer(45, lambda: os._exit(124)).start()
thread_id = str(uuid.uuid4())
turn_id = str(uuid.uuid4())
def receipt(name, value):
    destination = root / name
    temporary = destination.with_name(destination.name + "." + uuid.uuid4().hex)
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream)
        stream.write("\n")
    os.replace(temporary, destination)

receipt("codex-process.json", {"pid": os.getpid(), "start_ticks": Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19], "thread": thread_id})
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(str(path))
listener.listen()
def exact(connection, count):
    value = b""
    while len(value) < count:
        part = connection.recv(count - len(value))
        if not part:
            raise EOFError
        value += part
    return value

def read_message(connection):
    fragments = b""
    while True:
        first, second = exact(connection, 2)
        length = second & 127
        if length == 126:
            length = struct.unpack("!H", exact(connection, 2))[0]
        elif length == 127:
            length = struct.unpack("!Q", exact(connection, 8))[0]
        if length > 1024 * 1024:
            raise ValueError("fixture request exceeds its bound")
        mask = exact(connection, 4) if second & 128 else b""
        value = exact(connection, length)
        if mask:
            value = bytes(byte ^ mask[i % 4] for i, byte in enumerate(value))
        opcode = first & 15
        if opcode == 8:
            raise EOFError
        if opcode not in (0, 1):
            raise ValueError("unexpected fixture WebSocket opcode")
        fragments += value
        if len(fragments) > 1024 * 1024:
            raise ValueError("fixture fragmented request exceeds its bound")
        if first & 128:
            return json.loads(fragments)

def wait_gate(name):
    deadline = time.monotonic() + 25
    gate = root / name
    while not gate.exists():
        if time.monotonic() >= deadline:
            os._exit(125)
        time.sleep(0.01)
    if gate.is_symlink() or gate.read_text() != thread_id:
        os._exit(126)

def serve(connection):
    header = b""
    while b"\r\n\r\n" not in header:
        if len(header) > 8192:
            raise ValueError("oversized fixture handshake")
        header += exact(connection, 1)
    key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n") if line.lower().startswith(b"sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    send_lock = threading.Lock()
    def send(value):
        data = json.dumps(value, separators=(",", ":")).encode()
        header = bytes([129]) + (bytes([len(data)]) if len(data) < 126 else bytes([126]) + struct.pack("!H", len(data)))
        with send_lock:
            connection.sendall(header + data)
    def notify(method, params):
        send({"method": method, "params": params})
    def confirmed_thread(identifier):
        receipt("codex-thread-requested.json", {"request": identifier, "thread": thread_id})
        wait_gate("release-codex-thread")
        send({"id": identifier, "result": {"thread": {"id": thread_id, "name": "Delayed archive fixture", "status": {"type": "idle"}, "canAcceptDirectInput": True}, "model": "fixture-mini", "reasoningEffort": "low", "approvalPolicy": "never", "sandbox": {"type": "dangerFullAccess"}, "initialTurnsPage": {"data": [], "nextCursor": None}}})
        receipt("codex-thread-confirmed.json", {"request": identifier, "thread": thread_id})
    def finish_turn():
        wait_gate("release-codex-result")
        notify("item/completed", {"threadId": thread_id, "turnId": turn_id, "item": {"id": "archive-result", "type": "agentMessage", "text": "DELAYED_NATIVE_ARCHIVE_RESULT"}})
        notify("turn/completed", {"threadId": thread_id, "turn": {"id": turn_id, "status": "completed", "items": []}})
        receipt("codex-result-sent.json", {"thread": thread_id, "turn": turn_id})
    while True:
        request = read_message(connection)
        method, identifier, params = request.get("method"), request.get("id"), request.get("params", {})
        if identifier is None:
            continue
        if method == "initialize":
            result = {"userAgent": "cagents/0.162.0", "codexHome": str(account)}
        elif method == "thread/start":
            threading.Thread(target=confirmed_thread, args=(identifier,), daemon=True).start()
            continue
        elif method == "turn/start":
            if params.get("threadId") != thread_id or (root / "codex-turn-requested.json").exists():
                raise ValueError("fixture refuses mismatched thread or replayed prompt")
            receipt("codex-turn-requested.json", {"request": identifier, "thread": thread_id, "turn": turn_id})
            send({"id": identifier, "result": {"turn": {"id": turn_id, "status": "inProgress", "items": []}}})
            notify("turn/started", {"threadId": thread_id, "turn": {"id": turn_id, "status": "inProgress", "items": []}})
            threading.Thread(target=finish_turn, daemon=True).start()
            continue
        elif method in ("skills/list", "thread/list", "thread/turns/list", "thread/queue/list", "model/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        else:
            raise ValueError("unsupported fixture method: " + str(method))
        send({"id": identifier, "result": result})

while True:
    connection, _ = listener.accept()
    def worker(connection=connection):
        try:
            serve(connection)
        except (EOFError, ConnectionError, BrokenPipeError):
            pass
        finally:
            connection.close()
    threading.Thread(target=worker, daemon=True).start()
