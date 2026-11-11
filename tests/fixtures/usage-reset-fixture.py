#!/usr/bin/python3
"""Invented reset inventory and a loopback-only Claude reset service.

All state is under an owned private fixture root. No real provider executable,
credential store or network service is used. Run `seed ROOT`, then `serve ROOT`.
"""
import argparse
import contextlib
import copy
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import pwd
from pathlib import Path
import socket
import stat
import uuid

FAKE_TOKEN = "cagents-fixture-only"
ORGANIZATION = "00000000-0000-4000-8000-000000000001"
EXPIRY = "2030-01-01T00:00:00Z"
RESET_AT = "2027-01-01T12:00:00Z"


def private_directory(path):
    path = Path(path)
    metadata = path.lstat()
    if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid()
            or metadata.st_mode & 0o077 or path.resolve() != path.absolute()):
        raise ValueError("usage reset fixture requires an owned private physical directory")
    return path.resolve(strict=True)


def write_json(path, value):
    temporary = path.with_name(f".{path.name}-{uuid.uuid4()}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


@contextlib.contextmanager
def state_file(home):
    home = private_directory(home)
    lock = os.open(home / "fixture-reset.lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        descriptor = os.open(home / "fixture-reset-state.json", os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor) as stream:
            state = json.load(stream)
        if state.get("fixture") != FAKE_TOKEN:
            raise ValueError("refusing non-fixture reset state")
        yield state
        write_json(home / "fixture-reset-state.json", state)
        if "usage" in state:
            write_json(home / "fixture-usage.json", state["usage"])
            write_claude_frames(home, state)
    finally:
        os.close(lock)


def grant(identifier, windows, count=2):
    return {"id": identifier, "label": "Fixture reset", "resets_total": count,
            "resets_left": count, "starts_at": None, "ends_at": EXPIRY,
            "clears": windows, "paused": False, "usable_now": True,
            "use_requires_limit": False, "percent_used": {}, "blocking": []}


def claude_seed():
    return {"fixture": FAKE_TOKEN, "claims": {}, "consume_count": 0, "mode": "success",
            "usage": {"subscription_type": "max", "rate_limits_available": True,
                      "rate_limits": {
                          "five_hour": {"utilization": 82, "resets_at": RESET_AT},
                          "seven_day": {"utilization": 96, "resets_at": RESET_AT},
                          "model_scoped": [{"display_name": "Fable", "utilization": 100,
                                            "resets_at": RESET_AT}],
                          "juniper_tide": {"eligible": True, "arm": "reset", "available": True,
                                           "resets_per_week": 2, "weekly_resets_at": RESET_AT,
                                           "next_available_at": None},
                          "cedar_ember": {"eligible": True, "at_limit": True,
                                          "exhausted": ["seven_day_overage_included"],
                                          "next_grant_id": "fable-fixture",
                                          "weekly_resets_at": RESET_AT, "cooldown_until": None,
                                          "grants": [grant("fable-fixture", ["seven_day_overage_included"]),
                                                     grant("account-fixture", ["five_hour", "seven_day"], 1)]}}}}


def codex_seed():
    return {"fixture": FAKE_TOKEN, "claims": {}, "consume_count": 0, "mode": "success",
            "limits": {"ordinaryUsageAllowed": False,
                       "rateLimits": {"limitId": "codex", "planType": "plus",
                                      "primary": {"usedPercent": 90, "windowDurationMins": 300,
                                                  "resetsAt": 1798804800},
                                      "secondary": {"usedPercent": 100, "windowDurationMins": 10080,
                                                    "resetsAt": 1798804800}},
                       "rateLimitResetCredits": {"availableCount": 2, "credits": [
                           {"id": f"00000000-0000-4000-8000-00000000000{number}",
                            "grantedAt": 1794000000, "expiresAt": 1893456000,
                            "resetType": "codexRateLimits", "status": "available"}
                           for number in (2, 3)]}}}


def owned_fixture_directory(path, make_private=False):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        metadata = os.fstat(descriptor)
        if metadata.st_uid != os.getuid() or path.resolve() != path.absolute():
            raise ValueError("fixture home must be owned and must not traverse symlinks")
        if make_private:
            os.fchmod(descriptor, 0o700)
    finally:
        os.close(descriptor)


def seed(root):
    root = private_directory(root)
    real_home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()
    if root == real_home:
        raise ValueError("never seed resets in the real account home")
    (root / ".cagents-reset-fixture").touch(mode=0o600, exist_ok=True)
    base = root / "home"
    base.mkdir(mode=0o700, exist_ok=True)
    owned_fixture_directory(base)
    for provider, value in (("claude", claude_seed()), ("codex", codex_seed())):
        home = base / f".{provider}"
        home.mkdir(mode=0o700, exist_ok=True)
        # native-evidence creates these under its already-private root with
        # the ordinary umask. Tighten only a proved owned, physical child.
        owned_fixture_directory(home, make_private=True)
        private_directory(home)
        if (home / "fixture-reset-state.json").exists():
            raise ValueError("reset fixture already exists; restore the saved seed to restart")
        write_json(home / "fixture-reset-state.json", value)
        if provider == "claude":
            write_json(home / "fixture-usage.json", value["usage"])
            write_json(home / "fixture-models.json", {"models": [
                {"value": "default", "displayName": "Default (recommended)"},
                {"value": "fable", "displayName": "Fable 5.2"}]})
            write_claude_frames(home, value)


def write_claude_frames(home, state):
    limits = state["usage"]["rate_limits"]
    fable = next(model["utilization"] for model in limits["model_scoped"]
                 if model["display_name"] == "Fable")
    limited = fable >= 100
    windows = {key: {"utilization": limits[key]["utilization"] / 100,
                     "resetsAt": 1798804800} for key in ("five_hour", "seven_day")}
    windows["seven_day_overage_included"] = {"utilization": fable / 100, "resetsAt": 1798804800}
    frames = [{"type": "rate_limit_event", "rate_limit_info": {
        "status": "rejected" if limited else "allowed",
        "rateLimitType": "seven_day_overage_included", "resetsAt": 1798804800,
        "utilization": fable / 100, "unifiedWindows": windows}}]
    result = {"type": "result", "subtype": "error_during_execution" if limited else "success",
              "is_error": limited}
    if limited:
        result["errors"] = ["You've hit your Fable limit · resets Jan 1, 12pm (UTC)"]
    else:
        frames.append({"type": "assistant", "uuid": "fixture-reset-answer-@TURN@",
                       "message": {"id": "fixture-reset-answer-@TURN@", "role": "assistant",
                                   "content": [{"type": "text", "text": "The fixture task can continue."}]}})
    frames.extend([result, {"_end": True}])
    (home / "fixture-frames.jsonl").write_text("".join(json.dumps(frame) + "\n" for frame in frames))


def consume_claude(home, body):
    if not isinstance(body, dict):
        return 400, {"error": "fixture request must be an object"}, False
    if body.get("program") == "juniper_tide":
        return consume_session(home)
    key, identifier = body.get("request_id"), body.get("grant_id")
    if body.get("program") != "cedar_ember" or not isinstance(key, str) or not key:
        return 400, {"error": "fixture requires cedar_ember, grant_id and request_id"}, False
    with state_file(home) as state:
        if key in state["claims"]:
            claim = state["claims"][key]
            if claim["grant_id"] != identifier:
                return 409, {"error": "idempotency key belongs to another fixture grant"}, False
            return 200, claim["response"], False
        if state["mode"] == "error":
            return 503, {"result": "unavailable"}, False
        limits = state["usage"]["rate_limits"]
        program = limits["cedar_ember"]
        item = next((item for item in program["grants"] if item["id"] == identifier), None)
        if item is None or item["resets_left"] == 0:
            return 200, {"result": "already_used", "resets_left": 0, "cleared": []}, False
        if program["next_grant_id"] != identifier:
            return 200, {"result": "ineligible", "reason": "not_next_grant"}, False
        item["resets_left"] -= 1
        state["consume_count"] += 1
        for window in item["clears"]:
            if window == "seven_day_overage_included":
                for model in limits["model_scoped"]:
                    if model["display_name"] == "Fable":
                        model["utilization"] = 0
            elif window in limits:
                limits[window]["utilization"] = 0
        response = {"result": "reset", "resets_left": item["resets_left"],
                    "cleared": item["clears"], "weekly_resets_at": RESET_AT}
        replay_response = {**response, "result": "already_used"} if state["mode"] == "unconfirmed" else response
        state["claims"][key] = {"grant_id": identifier, "response": replay_response}
        if item["resets_left"] == 0:
            item["usable_now"] = False
            program["next_grant_id"] = next((item["id"] for item in program["grants"]
                                              if item["resets_left"]), None)
        if state["mode"] == "unconfirmed":
            return 200, {"result": "unavailable", "reason": "reset_unconfirmed"}, False
        return 200, response, state["mode"] == "disconnect"


def consume_session(home):
    # Deliberately has no server-side idempotency. Its caller must retain
    # local uncertainty and never resubmit an unsettled request.
    with state_file(home) as state:
        state["session_post_count"] = state.get("session_post_count", 0) + 1
        if state["mode"] == "error":
            return 503, {"result": "unavailable"}, False
        limits = state["usage"]["rate_limits"]
        program = limits.get("juniper_tide")
        if not program or not program.get("eligible") or program.get("arm") != "reset":
            return 200, {"result": "ineligible"}, False
        if not program.get("available"):
            return 200, {"result": "already_used", "next_available_at": program.get("next_available_at")}, False
        program["available"] = False
        program["next_available_at"] = RESET_AT
        limits["five_hour"]["utilization"] = 0
        state["consume_count"] += 1
        return 200, {"result": "reset", "next_available_at": RESET_AT,
                     "weekly_resets_at": program.get("weekly_resets_at")}, state["mode"] == "disconnect"


def consume_codex(home, params):
    key = params.get("idempotencyKey")
    if not isinstance(key, str) or not key:
        raise ValueError("fixture consume requires idempotencyKey")
    with state_file(home) as state:
        credit_id = params.get("creditId")
        if key in state["claims"]:
            prior = state["claims"][key]
            if prior["credit_id"] != credit_id:
                raise ValueError("idempotency key belongs to another fixture credit")
            return prior["response"], False
        if state["mode"] == "error":
            raise ValueError("fixture consume unavailable")
        inventory = state["limits"]["rateLimitResetCredits"]
        credit = next((credit for credit in inventory["credits"] if credit["status"] == "available"
                       and (credit_id is None or credit["id"] == credit_id)), None)
        if credit is None:
            return {"outcome": "noCredit"}, False
        credit["status"] = "redeemed"
        inventory["availableCount"] -= 1
        state["consume_count"] += 1
        state["limits"]["ordinaryUsageAllowed"] = True
        for window in ("primary", "secondary"):
            state["limits"]["rateLimits"][window]["usedPercent"] = 0
        response = {"outcome": "reset"}
        state["claims"][key] = {"credit_id": credit_id, "response": response}
        return response, state["mode"] == "disconnect"


def server(root):
    root = private_directory(root)
    if not (root / ".cagents-reset-fixture").is_file():
        raise ValueError("reset service requires the fixture root marker")
    home = private_directory(root / "home/.claude")
    if home == Path(pwd.getpwuid(os.getuid()).pw_dir).resolve() / ".claude":
        raise ValueError("never serve the real Claude home")

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def response(self, code, body):
            data = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def authorized(self):
            if self.headers.get("Authorization") != f"Bearer {FAKE_TOKEN}":
                self.response(401, {"error": "fixture accepts only its literal fake bearer"})
                return False
            return True

        def do_GET(self):
            if not self.authorized():
                return
            if self.path == "/api/oauth/profile":
                self.response(200, {"account": {"email": "fixture@example.test"},
                                    "organization": {"uuid": ORGANIZATION}})
                return
            if self.path.split("?", 1)[0] != "/api/oauth/usage":
                self.response(404, {})
                return
            with state_file(home) as state:
                self.response(200, copy.deepcopy(state["usage"]["rate_limits"]))

        def do_POST(self):
            if not self.authorized():
                return
            if self.path != f"/api/organizations/{ORGANIZATION}/reset_rate_limits":
                self.response(404, {})
                return
            size = int(self.headers.get("Content-Length", "0"))
            if size <= 0 or size > 8192:
                self.response(400, {})
                return
            try:
                body = json.loads(self.rfile.read(size))
                code, result, disconnect = consume_claude(home, body)
            except (ValueError, KeyError):
                self.response(400, {})
                return
            if disconnect:
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            self.response(code, result)

    http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    write_json(home / "fixture-reset-api.json", {"base_url": f"http://127.0.0.1:{http.server_port}", "fixture": True})
    return http


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("seed", "serve"))
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    if args.command == "seed":
        seed(args.root)
    else:
        http = server(args.root)
        print(json.dumps({"url": f"http://127.0.0.1:{http.server_port}",
                          "token": FAKE_TOKEN, "organization": ORGANIZATION}), flush=True)
        http.serve_forever()


if __name__ == "__main__":
    main()
