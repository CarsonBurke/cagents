#!/usr/bin/env python3
"""Private owning Codex app-server peer for combined-close HOST contracts.

No network/model calls. Unlike the shared exploratory peer, this executable
requires its own private test root and exact HOME/TMPDIR/socket containment.
It provides a real child process, Unix WebSocket protocol and stable history;
no runtime child/owner metadata is injected by the Rust fixture.
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
import uuid

if "--version" in sys.argv:
    print("codex-cli 0.162.0 (private combined-close fixture)")
    raise SystemExit
root = Path(__file__).resolve().parent.parent
home = Path(os.environ["HOME"]).resolve(strict=True)
if home != root / "home" or root.stat().st_uid != os.getuid() or root.stat().st_mode & 0o077:
    raise SystemExit("requires exact owned private execution fixture root/HOME")
# Fresh native launches deliberately strip CAGENTS_* harness environment. A
# private marker beside this copied executable binds the intended test root.
marker_fd = os.open(root / "combined-close-owner.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
with os.fdopen(marker_fd) as marker:
    metadata = os.fstat(marker.fileno())
    if metadata.st_uid != os.getuid() or metadata.st_mode & 0o077 or metadata.st_nlink != 1:
        raise SystemExit("unsafe fixture root marker")
    if json.loads(marker.read(4097)) != {"root": str(root), "uid": os.getuid()}:
        raise SystemExit("foreign fixture root marker")
codex_home = Path(os.environ.get("CODEX_HOME", str(home / ".codex"))).resolve(strict=True)
if codex_home != home / ".codex":
    raise SystemExit("fixture never joins a foreign Codex account")
runtime_tmp = Path(os.environ["TMPDIR"]).resolve(strict=True)
if runtime_tmp != root / "tmp":
    raise SystemExit("fixture requires its captured private TMPDIR")
args = sys.argv[1:]
if len(args) != 3 or args[:2] != ["app-server", "--listen"] or not args[2].startswith("unix://"):
    raise SystemExit("requires app-server --listen unix://PATH")
path = Path(args[2][len("unix://"):])
parent = path.parent.resolve(strict=True)
if (not path.is_absolute() or parent.parent != runtime_tmp or path.name != "control.sock"
        or not parent.name.startswith("cagents-codex-") or parent.stat().st_uid != os.getuid()
        or parent.stat().st_mode & 0o077 or path.exists() or path.is_symlink()):
    raise SystemExit("fixture never replaces or joins a foreign listener")
uuid.UUID(parent.name[len("cagents-codex-"):])

def private_append(name, value):
    descriptor = os.open(root / name, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(descriptor, "a") as stream:
        stream.write(json.dumps(value, separators=(",", ":")) + "\n")

private_append("codex-processes.jsonl", {"pid": os.getpid(), "start": int(Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19])})
thread_id = str(uuid.uuid4())
turns = []
state_lock = threading.Lock()

def exact(connection, count):
    value = bytearray()
    while len(value) < count:
        part = connection.recv(count - len(value))
        if not part:
            raise EOFError
        value.extend(part)
    return bytes(value)

def send(connection, value, opcode=1):
    data = value if isinstance(value, bytes) else json.dumps(value, separators=(",", ":")).encode()
    length = len(data)
    header = bytes([128 | opcode])
    header += bytes([length]) if length < 126 else bytes([126]) + struct.pack("!H", length) if length < 65536 else bytes([127]) + struct.pack("!Q", length)
    connection.sendall(header + data)

def receive(connection):
    fragments = bytearray()
    while True:
        first, second = exact(connection, 2)
        length = second & 127
        if length == 126:
            length = struct.unpack("!H", exact(connection, 2))[0]
        elif length == 127:
            length = struct.unpack("!Q", exact(connection, 8))[0]
        if length > 1024 * 1024:
            raise ValueError("bounded fixture request exceeded")
        mask = exact(connection, 4) if second & 128 else b""
        data = exact(connection, length)
        if mask:
            data = bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))
        opcode = first & 15
        if opcode == 8:
            raise EOFError
        if opcode == 9:
            send(connection, data, 10)
            continue
        if opcode == 10:
            continue
        if opcode not in (0, 1):
            raise ValueError("text frames only")
        fragments.extend(data)
        if len(fragments) > 1024 * 1024:
            raise ValueError("bounded fragmented request exceeded")
        if first & 128:
            return json.loads(fragments)

def serve(connection):
    header = bytearray()
    while b"\r\n\r\n" not in header:
        header.extend(exact(connection, 1))
        if len(header) > 16384:
            raise ValueError("bounded handshake exceeded")
    key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n") if line.lower().startswith(b"sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    while True:
        request = receive(connection)
        private_append("codex-rpc.jsonl", request)
        identifier = request.get("id")
        if identifier is None:
            continue
        method, params = request["method"], request.get("params", {})
        result = {}
        if method == "initialize":
            result = {"userAgent": "cagents/0.162.0", "codexHome": str(codex_home)}
        elif method in ("thread/start", "thread/resume"):
            if method == "thread/resume" and params["threadId"] != thread_id:
                send(connection, {"id": identifier, "error": {"code": -32602, "message": "foreign fixture thread"}})
                continue
            with state_lock:
                history = list(turns)
            result = {"thread": {"id": thread_id, "status": {"type": "idle"}, "canAcceptDirectInput": True}, "model": "fixture-model", "initialTurnsPage": {"data": history, "nextCursor": None}}
        elif method == "thread/read":
            with state_lock:
                result = {"thread": {"id": thread_id, "turns": list(turns)}}
        elif method == "thread/turns/list":
            with state_lock:
                result = {"data": list(turns), "nextCursor": None}
        elif method in ("model/list", "skills/list", "thread/list", "thread/queue/list"):
            result = {"data": [], "nextCursor": None}
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "fixture@invalid", "planType": "free"}}
        elif method == "turn/start":
            result = {"turn": {"id": "owned-turn"}}
        else:
            send(connection, {"id": identifier, "error": {"code": -32601, "message": "unsupported private fixture method"}})
            continue
        send(connection, {"id": identifier, "result": result})
        if method == "turn/start":
            turn = {"id": "owned-turn", "status": "completed", "items": [{"id": "answer", "type": "agentMessage", "text": "LOCAL_CODEX_CLOSE_RESULT"}]}
            with state_lock:
                turns[:] = [turn]
            send(connection, {"method": "turn/completed", "params": {"threadId": thread_id, "turn": turn}})

listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(str(path))
listener.listen(4)
# The actor has one connection. A bounded allowance permits explicit inspection
# reconnects without a thread-per-request or an unbounded exploratory server.
for _ in range(4):
    connection, _ = listener.accept()
    def worker(connection=connection):
        try:
            serve(connection)
        except (EOFError, ConnectionError):
            pass
        except Exception as error:
            private_append("codex-errors.jsonl", {"error": str(error)})
        finally:
            connection.close()
    threading.Thread(target=worker, daemon=True).start()
