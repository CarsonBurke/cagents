#!/usr/bin/python3
"""Offline stream-json peer fixture; every nested command uses the shipped CLI.

The daemon issues each provider process its own capability. subprocess.Popen
inherits that environment unchanged; this fixture never examines or copies it.
This is not Claude/model qualification or a native-window controller.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

CLI = str(Path(__file__).parent / "cagents")
ROOT = Path.home().parent
UNIT = "PUBLIC_PEER_λ🦀\n"
TOOL = "PUBLIC_TOOL_RESULT_λ🦀"
PRIVATE = "EXCLUDED_RAW_TOOL_INPUT"
THINKING = "EXCLUDED_REASONING"
state = {}
session = str(uuid.uuid4())
arguments = iter(sys.argv[1:])
for argument in arguments:
    if argument in ("--session-id", "--resume"):
        session = next(arguments)
    elif argument.startswith(("--session-id=", "--resume=")):
        session = argument.split("=", 1)[1]


def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def cli(arguments, success=True):
    # File-backed, finite capture, including pages larger than a pipe buffer.
    # There is deliberately no env= argument and no credential access here.
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as error:
        child = subprocess.Popen(
            [CLI, "--device", "local", "--no-start", "--json", *arguments],
            stdin=subprocess.DEVNULL,
            stdout=output,
            stderr=error,
        )
        try:
            deadline = time.monotonic() + 25
            while child.poll() is None:
                assert time.monotonic() < deadline, "nested CLI deadline"
                assert output.tell() <= 2 * 1024 * 1024, "nested CLI stdout bound"
                assert error.tell() <= 64 * 1024, "nested CLI stderr bound"
                time.sleep(0.01)
            status = child.wait()
            output.seek(0)
            raw = output.read(2 * 1024 * 1024 + 1)
            assert len(raw) <= 2 * 1024 * 1024
            value = json.loads(raw)
            assert (status == 0) == success, "unexpected nested CLI exit"
            assert value.get("ok") is success, "unexpected nested CLI result"
            if not success:
                assert "data" not in value, "refusal published peer data"
            return value, len(raw)
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)


def data(arguments):
    return cli(arguments)[0]["data"]


def discover(agent):
    directory = data(["ls"])
    me = directory["me"]
    assert me is not None and "Session" in me["delegation"]["owner"]
    targets = [
        entry for entry in directory["sessions"]
        if entry.get("delegation", {}).get("agent") == agent
    ]
    assert len(targets) == 1
    target = targets[0]
    assert target["delegation"]["owner"] == "User"
    assert target["relation"] == "Peer" and target["surface"] == "Chat"
    uuid.UUID(target["id"])
    return me, target


def read_all(agent):
    me, target = discover(agent)
    assert me["channel"] == target["channel"] == state["channel"]
    cursor = None
    assistant = bytearray()
    tools = []
    offsets = {}
    largest = most_parts = 0
    for index in range(16):
        arguments = ["chat", "read", target["id"]]
        if cursor is not None:
            arguments += ["--cursor", cursor]
        value, size = cli(arguments)
        page = value["data"]
        assert page["schema_version"] == 1 and page["session"] == target["id"]
        assert page["coverage"] == "Cached"
        assert size <= 128 * 1024 and len(page["parts"]) <= 16
        encoded = json.dumps(page, ensure_ascii=False)
        assert PRIVATE not in encoded and THINKING not in encoded
        largest = max(largest, size)
        most_parts = max(most_parts, len(page["parts"]))
        for part in page["parts"]:
            text = part["text"].encode()
            assert len(text) <= 64 * 1024
            assert part["offset"] == offsets.get(part["item_id"], 0)
            offsets[part["item_id"]] = part["offset"] + len(text)
            kind = part["kind"]
            if isinstance(kind, dict) and "AssistantText" in kind:
                assistant.extend(text)
            elif isinstance(kind, dict) and "ToolOutput" in kind:
                assert kind["ToolOutput"] == {
                    "name": "FixtureTool", "status": "Succeeded"
                }
                tools.append(part["text"])
            else:
                assert kind == "UserText", "unexpected public kind"
        previous = cursor
        cursor = page["next_cursor"]
        if index == 0 and "cursor" not in state:
            assert not page["at_end"] and cursor is not None
            state.update(cursor=cursor, original_session=target["id"])
        if page["at_end"]:
            assert cursor is None
            return dict(
                pages=index + 1,
                largest_page=largest,
                most_parts=most_parts,
                assistant_bytes=len(assistant),
                assistant_sha256=hashlib.sha256(assistant).hexdigest(),
                tool_output="".join(tools),
                target_session=target["id"],
                target_owner=target["delegation"]["owner"],
                caller_session=me["id"],
                caller_owner=me["delegation"]["owner"],
                target_relation=target["relation"],
                at_end=True,
                next_cursor=None,
                excluded_fields_absent=True,
            )
        assert cursor is not None and cursor != previous
    raise AssertionError("peer cache did not end within sixteen pages")


def refused(session_id, cursor=None):
    arguments = ["chat", "read", session_id]
    if cursor is not None:
        arguments += ["--cursor", cursor]
    value, _ = cli(arguments, success=False)
    return value["error"]["kind"]


def reader(command):
    action = command["action"]
    if action == "read":
        report = read_all(state["target"])
        refusal, _ = cli(["agent", "status", state["target"], "--full"], False)
        message = refusal["error"]["message"]
        assert "does not own" in message and "cagents chat read" in message
        cli(["agent", "close", state["target"]], False)
        report["owner_status_hint"] = True
        report["owner_close_refused"] = True
        return report
    if action == "outside":
        _, target = discover(state["target"])
        assert target["channel"] != state["channel"]
        return dict(
            fresh_refused=refused(target["id"]),
            continuation_refused=refused(target["id"], state["cursor"]),
        )
    if action == "returned":
        old = refused(state["original_session"], state["cursor"])
        return dict(old_cursor_refused=old, fresh=read_all(state["target"]))
    if action == "replacement":
        _, target = discover(command["target"])
        assert target["id"] != state["original_session"]
        old = refused(state["original_session"])
        changed = refused(target["id"], state["cursor"])
        return dict(
            old_session_refused=old,
            new_session_old_cursor_refused=changed,
            fresh=read_all(command["target"]),
        )
    raise AssertionError("unknown reader action")


def execute(command):
    if not state:
        state.update(command)
        if state["role"] == "reader":
            return {"reader_ready": True}
        if state["role"] == "target":
            return None
        assert state["role"] == "parent"
        child = data([
            "agent", "spawn", "--provider", "claude", "--surface", "chat",
            "--project", state["project"], "--channel", state["channel"],
            "--name", "peer-reader", "--idempotency-key", "peer-reader",
            "--wait", "--timeout", "10",
            json.dumps(dict(role="reader", target=state["target"],
                            channel=state["channel"])),
        ])
        state["child"] = child["agent"]["id"]
        assert "Child" in child["agent"]["origin"]
        return dict(child_agent=state["child"], child_origin=child["agent"]["origin"],
                    child_session=child["agent"]["session"]["Chat"]["id"])
    if state["role"] == "reader":
        return reader(command)
    assert state["role"] == "parent"
    if command["action"] == "cleanup":
        if state.get("child") is not None:
            data(["agent", "close", state["child"]])
            state["child"] = None
        return {"child_closed": True}
    reply = data([
        "agent", "send", state["child"], json.dumps(command),
        "--wait", "--timeout", "15",
    ])
    return json.loads(reply["run"]["result"]["text"])


for line in sys.stdin:
    assert len(line) <= 1024 * 1024, "bounded fixture input"
    frame = json.loads(line)
    if frame["type"] == "control_request":
        emit(dict(type="control_response", response=dict(
            subtype="success", request_id=frame["request_id"], response={})))
    elif frame["type"] == "user":
        text = "".join(part.get("text", "") for part in frame["message"]["content"])
        message = str(uuid.uuid4())
        try:
            command = json.loads(text)
            if state.get("role", command.get("role")) == "target":
                with (ROOT / "target-prompts").open("a") as log:
                    log.write("prompt\n")
            result = execute(command)
            if result is None:
                emit(dict(type="assistant", uuid=message, message=dict(
                    id=message, role="assistant", content=[
                        dict(type="thinking", thinking=THINKING),
                        dict(type="text", text=state["tag"] + UNIT * 12000),
                        dict(type="tool_use", id="peer-tool", name="FixtureTool",
                             input=dict(private=PRIVATE)),
                    ])))
                emit(dict(type="user", uuid=str(uuid.uuid4()), message=dict(
                    role="user", content=[dict(type="tool_result",
                    tool_use_id="peer-tool", content=TOOL, is_error=False)])))
            else:
                emit(dict(type="assistant", uuid=message, message=dict(
                    id=message, role="assistant", content=[
                        dict(type="text", text=json.dumps(result))])))
        except Exception as failure:
            # Fixed fixture inputs and public errors only; never dump the
            # environment, provider frames, launch recipes or capabilities.
            emit(dict(type="assistant", uuid=message, message=dict(
                id=message, role="assistant", content=[dict(type="text",
                text=json.dumps(dict(fixture_error=str(failure))))])))
        emit(dict(type="result", uuid=str(uuid.uuid4()), subtype="success",
                  is_error=False, session_id=session))
