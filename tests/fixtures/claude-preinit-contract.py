#!/usr/bin/python3 -ISB
"""Fixed offline Claude stream-json scenarios; no provider or filesystem access.

Select exactly one --fixture-scenario at process startup. This executable does
not select an account, read environment/configuration/history, spawn children,
use networking, or implement its own startup timer. See the companion contract.
"""
import json
import re
import sys


SCENARIOS = (
    "healthy-advisory",
    "preinit-advisory-pending",
    "preinit-eof",
    "preinit-noise-eight",
    "healthy-noise-reset",
)
MAX_LINE_BYTES = 65536
MAX_INPUT_FRAMES = 32
DEFAULT_SESSION = "00000000-0000-4000-8000-000000001859"
MESSAGE_ID = "00000000-0000-4000-8000-000000185901"
ANSWER = "Offline fixture answer after advisory frames and isolated wrapper output."
BOOLEAN_FLAGS = frozenset((
    "--print", "-p", "--verbose", "--include-partial-messages",
    "--replay-user-messages", "--bare", "--safe-mode", "--no-chrome",
    "--strict-mcp-config", "--disable-slash-commands",
))
TRANSPORT_VALUES = {
    "--input-format": "stream-json",
    "--output-format": "stream-json",
    "--permission-prompt-tool": "stdio",
}
# These values are validated then ignored, never opened, executed or printed.
IGNORED_VALUES = frozenset(("--model", "--agent", "--effort", "--setting-sources"))
MODES = frozenset(("default", "manual", "acceptEdits", "auto", "plan", "dontAsk", "bypassPermissions"))


class ContractError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ContractError(message)


def arguments(argv):
    require(len(argv) <= 128 and sum(len(arg) for arg in argv) <= 65536,
            "fixture argument bound exceeded")
    scenario, session, version = None, None, False
    seen = set()
    index = 0
    while index < len(argv):
        flag, equal, value = argv[index].partition("=")
        index += 1
        require(flag not in seen, "duplicate fixture/Claude argument")
        seen.add(flag)
        if flag == "--version":
            require(not equal, "version takes no value")
            version = True
            continue
        if flag in BOOLEAN_FLAGS:
            require(not equal, "boolean Claude flag takes no value")
            continue
        require(flag in TRANSPORT_VALUES or flag in IGNORED_VALUES
                or flag in ("--fixture-scenario", "--session-id", "--resume", "-r", "--permission-mode"),
                "unsupported fixture/Claude argument")
        if not equal:
            require(index < len(argv), "missing fixture/Claude argument value")
            value = argv[index]
            index += 1
        require(len(value) <= 4096 and "\x00" not in value
                and (bool(value) or flag == "--setting-sources"), "invalid argument value")
        if flag == "--fixture-scenario":
            require(value in SCENARIOS, "unknown fixture scenario")
            scenario = value
        elif flag in TRANSPORT_VALUES:
            require(value == TRANSPORT_VALUES[flag], "fixture requires stream-json stdio transport")
        elif flag in ("--session-id", "--resume", "-r"):
            require(session is None and re.fullmatch(
                r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}", value),
                "fixture needs one UUID session identity")
            session = value
        elif flag == "--permission-mode":
            require(value in MODES, "unsupported Claude permission mode")
    if version:
        require(seen <= {"--version", "--fixture-scenario"}, "version accepts only a fixed scenario prefix")
    else:
        require(scenario is not None, "exactly one fixture scenario is required")
    return scenario, session or DEFAULT_SESSION, version


def unique_members(items):
    result = {}
    for key, value in items:
        require(key not in result, "duplicate stdin JSON member")
        result[key] = value
    return result


def frame():
    raw = sys.stdin.buffer.readline(MAX_LINE_BYTES + 1)
    if not raw:
        return None
    require(len(raw) <= MAX_LINE_BYTES and raw.endswith(b"\n"), "stdin frame bound/framing failure")
    try:
        value = json.loads(raw, object_pairs_hook=unique_members)
    except (ValueError, UnicodeError, RecursionError):
        raise ContractError("invalid stdin JSON") from None
    require(type(value) is dict and type(value.get("type")) is str, "invalid stdin frame")
    return value


def send(value):
    print(json.dumps(value, separators=(",", ":"), sort_keys=True), flush=True)


def noise(count):
    # Deliberately NOT JSON. Payload/count are fixed; no prompt or private data.
    for index in range(count):
        print("offline fixture malformed stdout line " + str(index + 1), flush=True)


def response(request_id, success):
    payload = {"request_id": request_id, "subtype": "success" if success else "error"}
    if success:
        payload["response"] = {}
    else:
        payload["error"] = "Offline fixture supports initialize only"
    send({"type": "control_response", "response": payload})


def wait_for_owner_close():
    # Keep the original process/stdout pipe open. The real runtime owns timing
    # and teardown; EOF/requested close may end this wait. No sleeps or timer.
    for _ in range(MAX_INPUT_FRAMES - 1):
        if frame() is None:
            return
    raise ContractError("pending fixture stdin frame bound exceeded")


def answer(session):
    send({"type": "system", "subtype": "init", "session_id": session})
    send({"type": "future_advisory", "opaque": "fixed offline advisory"})
    send({"type": "system", "subtype": "future_system", "opaque": "fixed offline advisory"})
    noise(1)
    send({"type": "stream_event", "event": {"type": "message_start", "message": {"id": MESSAGE_ID}}})
    send({"type": "stream_event", "event": {"type": "future_stream_event"}})
    send({"type": "stream_event", "event": {"type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}}})
    send({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": ANSWER}}})
    send({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}})
    send({"type": "stream_event", "event": {"type": "message_stop"}})
    send({"type": "result", "subtype": "success", "is_error": False,
          "session_id": session, "usage": {"input_tokens": 1, "output_tokens": 1}})


def main():
    scenario, session, version = arguments(sys.argv[1:])
    if version:
        print("2.1.286 (offline preinit-contract fixture)")
        return 0
    initial = frame()
    require(initial is not None and initial["type"] == "control_request"
            and type(initial.get("request")) is dict
            and initial["request"].get("subtype") == "initialize",
            "fixture needs an initial initialize control request")
    request_id = initial.get("request_id")
    require(type(request_id) is str and 0 < len(request_id) <= 256,
            "invalid initialize request identity")
    if scenario == "preinit-eof":
        return 0  # Unrequested clean stdout EOF, with NO matching success.
    if scenario == "preinit-advisory-pending":
        send({"type": "keep_alive"})
        send({"type": "future_advisory"})
        wait_for_owner_close()
        return 0
    if scenario == "preinit-noise-eight":
        noise(8)
        wait_for_owner_close()
        return 0
    if scenario == "healthy-noise-reset":
        # Fourteen invalid lines total. Each valid advisory resets the actual
        # transport streak, so these must not become eight consecutive lines.
        noise(7)
        send({"type": "keep_alive"})
        noise(7)
        send({"type": "future_advisory"})
    response(request_id, True)  # Only these two healthy scenarios acknowledge.
    answered = False
    for _ in range(MAX_INPUT_FRAMES - 1):
        incoming = frame()
        if incoming is None:
            return 0
        if incoming["type"] == "control_request":
            request_id = incoming.get("request_id")
            require(type(request_id) is str and 0 < len(request_id) <= 256,
                    "invalid control request identity")
            response(request_id, False)
        elif incoming["type"] == "user":
            require(not answered and type(incoming.get("message")) is dict,
                    "fixture accepts exactly one user turn")
            answered = True
            answer(session)
        else:
            raise ContractError("unsupported fixture stdin frame type")
    raise ContractError("healthy fixture stdin frame bound exceeded")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ContractError, BrokenPipeError, OSError):
        # Fixed bounded stderr: never echo arguments, input, cwd or authority.
        print("Offline preinit fixture contract/pipe failure", file=sys.stderr, flush=True)
        raise SystemExit(2)
