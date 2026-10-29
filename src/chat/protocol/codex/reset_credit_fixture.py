"""Offline JSON-lines peer for the owning Codex/runtime/controller tests.

No provider executable, account directory, credential, or network is used.
"""
import json
import os
import sys

mode, trace = sys.argv[1:]
account = {"account": {"type": "chatgpt"}, "workspaceRouting": {
    "chatgptAccountId": "actual-account", "backendOrigin": "https://chatgpt.com"}}
limits_reads = 0
consumes = 0
redeemed = False


def notification(method, params):
    return {"method": method, "params": {"threadId": "thread-exact", **params}}


def delta(text):
    return notification("item/agentMessage/delta", {
        "turnId": "preserved-running-turn", "itemId": "stream", "delta": text})


def emit(*frames):
    # One atomic pipe write: Limits and the login invalidation are available
    # to the runtime's same input batch, before its queued consume can start.
    os.write(1, ("".join(json.dumps(frame) + "\n" for frame in frames)).encode())


emit(notification("turn/started", {"turn": {
    "id": "preserved-running-turn", "status": "inProgress", "items": []}}), delta("running;"))
for line in sys.stdin:
    request = json.loads(line)
    with open(trace, "a", encoding="utf-8") as output:
        output.write(json.dumps(request) + "\n")
    method = request["method"]
    result = {}
    if method == "account/read":
        result = account
    elif method == "account/rateLimits/read":
        limits_reads += 1
        other = mode.startswith("resolve-") or (mode.startswith("lost-") and consumes > 0)
        credit = "other-credit" if other else "actual-credit"
        count = 0 if redeemed else 1
        result = {"accountId": "actual-account", "rateLimits": {"primary": {
            "usedPercent": 0 if redeemed else 96, "resetsAt": 4000000000}},
            "rateLimitResetCredits": {"availableCount": count, "credits": [] if not count else [
                {"id": credit, "resetType": "codexRateLimits", "status": "available"}]}}
        if mode == "buffer-login" and limits_reads == 3:
            emit({"id": request["id"], "result": result},
                 notification("account/updated", {"authMode": "chatgpt"}), delta("after-login;"))
            continue
    elif method == "account/rateLimitResetCredit/consume":
        consumes += 1
        if mode.startswith("lost") and consumes == 1:
            redeemed = mode == "lost"
            emit(delta("ack-suppressed;"))
            continue
        outcome = mode.removeprefix("resolve-") if mode.startswith("resolve-") else (
            "alreadyRedeemed" if redeemed else "reset")
        if request["params"]["creditId"] == "other-credit":
            outcome = "reset"
        redeemed = outcome in ("reset", "alreadyRedeemed")
        result = {"outcome": outcome}
        emit(delta("reconciled;"))
    elif method == "turn/interrupt":
        emit({"id": request["id"], "result": {}}, notification("turn/completed", {"turn": {
            "id": "preserved-running-turn", "status": "interrupted", "items": []}}))
        continue
    else:
        raise AssertionError("unexpected provider operation: " + method)
    emit({"id": request["id"], "result": result})
