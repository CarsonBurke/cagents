#!/usr/bin/python3
"""Offline Claude stream-JSON fixture for actual native #1757 validation.

Copy to <private native1757-*/bin/claude>. The fixture responds through the
normal CLI/SDK protocol and writes provider-owned history. It never edits the
app registry, app session store, capability, or daemon receipts. It has no
network calls, subprocesses, or signal operations. Its complete command budget
is one text-only FIRST_SEND_1757 and one UNSENT_SAVE_RECOVERY_1757_FOCUS_RETURNED
with exactly one image. Each is accepted once, in its own SDK process, with at
most two SDK starts. A failed first save must consume no provider start.
"""
import base64
import hashlib
import json
import os
import pathlib
import stat
import sys
import time
import uuid

os.umask(0o077)
root = pathlib.Path(__file__).resolve().parent.parent
assert root.name.startswith("native1757-"), "fixture root must be task-owned"
assert root.stat().st_uid == os.getuid()
assert stat.S_IMODE(root.stat().st_mode) == 0o700

if sys.argv[1:2] == ["--version"]:
    print("2.1.286 (offline first-send fixture)")
    raise SystemExit
if sys.argv[1:2] == ["auth"]:
    print(json.dumps(dict(loggedIn=True, email="fixture@example.test", subscriptionType="pro")))
    raise SystemExit

def argument(name):
    found = []
    for index, item in enumerate(sys.argv[1:], 1):
        if item.startswith(name + "="):
            found.append(item.split("=", 1)[1])
        elif item == name:
            assert index + 1 < len(sys.argv)
            found.append(sys.argv[index + 1])
    assert len(found) <= 1
    return found[0] if found else None


def record(name, value):
    path = root / name
    data = (json.dumps(value, ensure_ascii=True) + "\n").encode()
    with path.open("ab") as output:
        assert output.tell() + len(data) <= 32768
        output.write(data)


usage_probe = [
    "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    "--no-session-persistence", "--strict-mcp-config", "--setting-sources", "",
    "--disable-slash-commands",
]
if sys.argv[1:] == usage_probe:
    # Only the exact account::probe_claude no-turn query is exempt. It never
    # accepts a prompt or emits SDK/session/history data, and spends no turn.
    record("probe-attempts.jsonl", {"pid": os.getpid(), "kind": "usage-only"})
    print("offline usage unavailable", file=sys.stderr)
    raise SystemExit(69)


start = {"pid": os.getpid(), "resume_present": any(
    item == "--resume" or item.startswith("--resume=") for item in sys.argv[1:])}
# Collect only a canonical UUID hint without rejecting malformed/duplicate
# operands. Every non-probe invocation must be counted before it can refuse.
session_hints = []
for index, item in enumerate(sys.argv[1:], 1):
    if item.startswith("--session-id="):
        session_hints.append(item.split("=", 1)[1])
    elif item == "--session-id" and index + 1 < len(sys.argv):
        session_hints.append(sys.argv[index + 1])
if len(session_hints) == 1:
    try:
        if str(uuid.UUID(session_hints[0])) == session_hints[0]:
            start["session"] = session_hints[0]
    except ValueError:
        pass
# Log every real SDK invocation, including any attempt beyond the budget.
# Exclusive slots bound concurrent processes without rewriting prior receipts.
record("provider-starts.jsonl", start)
start_slot = None
for slot in (1, 2):
    try:
        with (root / f"provider-start-{slot}.json").open("x") as output:
            output.write(json.dumps(start))
        start_slot = slot
        break
    except FileExistsError:
        pass
assert start_slot is not None, "the two-start SDK fixture budget is exhausted"

account = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"]).resolve(strict=True)
assert account == (root / "account").resolve(strict=True)
assert pathlib.Path.cwd().resolve() == (root / "project").resolve(strict=True)
session = argument("--session-id")
assert argument("--resume") is None, "UI-only reopen must adopt the original actor"
assert session and str(uuid.UUID(session)) == session

project_key = "".join(c if c.isascii() and c.isalnum() else "-" for c in os.getcwd())
history = account / "projects" / project_key / (session + ".jsonl")
parent = None
history_bytes = 0
accepted_in_process = False
commands = {
    "first-send": {"token": "FIRST_SEND_1757", "images": 0, "slot": 1,
                   "receipt": "accepted-first-send.json", "release": "release-native-identity"},
    "recovery": {"token": "UNSENT_SAVE_RECOVERY_1757_FOCUS_RETURNED", "images": 1, "slot": 2,
                 "receipt": "accepted-recovery.json", "release": "release-recovery-native-identity"},
}


def send(value, persist=False):
    global parent, history_bytes
    if persist:
        history.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        stored = dict(value, sessionId=session, parentUuid=parent)
        raw = json.dumps(stored, ensure_ascii=False) + "\n"
        history_bytes += len(raw.encode())
        assert history_bytes <= 4 * 1024 * 1024
        with history.open("a") as output:
            output.write(raw)
        parent = value.get("uuid", parent)
    print(json.dumps(value), flush=True)


for line in sys.stdin:
    frame = json.loads(line)
    if frame.get("type") == "control_request":
        subtype = frame.get("request", {}).get("subtype")
        metadata = {}
        if subtype == "initialize":
            metadata = {
                "claude_code_version": "2.1.286",
                "current_permission_mode": "default",
                "models": [{"value": "haiku", "displayName": "Claude Haiku"}],
            }
        send({"type": "control_response", "response": {
            "subtype": "success", "request_id": frame["request_id"], "response": metadata,
        }})
    elif frame.get("type") == "user":
        message = frame.get("message", {})
        encoded = json.dumps(message, sort_keys=True)
        content = message.get("content", [])
        assert isinstance(content, list) and content and content[0].get("type") == "text"
        text = content[0]["text"]
        matched = [(name, command) for name, command in commands.items() if command["token"] in text]
        assert len(matched) == 1, "only one declared synthetic native test prompt is allowed"
        name, command = matched[0]
        # The actual SDK keeps the user's primary text in its first block;
        # later text blocks carry normal image-reference metadata unchanged.
        body = text.strip()
        if name == "recovery":
            assert body.count("[Image 1]") + body.count("[Image1]") == 1
            body = body.replace("[Image 1]", "").replace("[Image1]", "").strip()
        assert body == command["token"], "undeclared text is outside the synthetic prompt budget"
        assert not accepted_in_process, "each SDK process accepts only one declared command"
        assert start_slot == command["slot"], "SDK start does not match the declared command order"
        image_blocks = [item for item in content if isinstance(item, dict) and item.get("type") == "image"]
        images = len(image_blocks)
        assert images == command["images"], "the declared synthetic image count changed"
        image_sha256 = None
        if name == "recovery":
            source = image_blocks[0].get("source")
            assert isinstance(source, dict) and source.get("type") == "base64"
            assert source.get("media_type") == "image/png", "recovery must preserve the synthetic PNG"
            data = source.get("data")
            maximum_bytes = 64 * 1024
            maximum_encoded = 4 * ((maximum_bytes + 2) // 3)
            assert isinstance(data, str) and 0 < len(data) <= maximum_encoded
            decoded = base64.b64decode(data.encode("ascii"), validate=True)
            assert 0 < len(decoded) <= maximum_bytes
            expected_path = root / "synthetic-image.png"
            assert expected_path.resolve(strict=True) == expected_path
            with os.fdopen(os.open(expected_path, os.O_RDONLY | os.O_NOFOLLOW), "rb") as expected:
                info = os.fstat(expected.fileno())
                assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                assert 0 < info.st_size <= maximum_bytes
                expected_bytes = expected.read(maximum_bytes + 1)
            assert 0 < len(expected_bytes) <= maximum_bytes and expected_bytes.startswith(b"\x89PNG\r\n\x1a\n")
            image_sha256 = hashlib.sha256(decoded).hexdigest()
            assert image_sha256 == hashlib.sha256(expected_bytes).hexdigest(), "recovery image bytes changed"
        count = root / command["receipt"]
        with count.open("x") as output:
            output.write(json.dumps({
                "fixture_command": name,
                "session": session,
                "command": frame["uuid"],
                "message_sha256": hashlib.sha256(encoded.encode()).hexdigest(),
                "images": images,
                "image_sha256": image_sha256,
            }))
        accepted_in_process = True
        received = frame["uuid"]
        # Normal SDK accepts work before its later native identity frame.
        # Omit session_id from these frames so the app must retain the pane
        # after acceptance without borrowing provider identity as protection.
        send({"type": "user", "uuid": received, "parent_tool_use_id": None,
              "message": message}, persist=True)
        send({"type": "command_lifecycle", "command_uuid": received, "state": "started"})
        send({"type": "assistant", "uuid": "first-send-waiting", "message": {
            "id": "first-send-waiting", "role": "assistant",
            "content": [{"type": "text", "text": "First send accepted. Waiting for the fixture to continue."}],
        }}, persist=True)
        record("protocol-states.jsonl", {"state": "accepted-before-native-identity", "session": session,
                                          "fixture_command": name})
        deadline = time.monotonic() + 300
        while not (root / command["release"]).exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("native operator did not release the first-send fixture within five minutes")
            time.sleep(0.02)
        send({"type": "system", "subtype": "init", "session_id": session,
              "claude_code_version": "2.1.286", "model": "haiku"})
        send({"type": "assistant", "uuid": "first-send-complete", "message": {
            "id": "first-send-complete", "role": "assistant",
            "content": [{"type": "text", "text": "First-send fixture complete."}],
        }}, persist=True)
        send({"type": "result", "uuid": "first-send-result", "subtype": "success",
              "is_error": False, "session_id": session,
              "usage": {"input_tokens": 7, "output_tokens": 12}}, persist=True)
        record("protocol-states.jsonl", {"state": "completed", "session": session,
                                          "fixture_command": name})
