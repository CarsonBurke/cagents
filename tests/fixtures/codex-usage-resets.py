#!/usr/bin/python3
"""Offline Codex usage/reset peer, supporting stdio probes and Unix WebSockets.

The fixture's persisted credits are invented. Every consume is local, and a
post-send disconnect mode verifies refreshing state and reusing the same key.
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


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


reset = load("usage_reset_fixture", "usage-reset-fixture.py")
peer = load("usage_reset_codex_transport", "codex-tool-rows.py")


class Scenario(peer.Scenario):
    def request(self, frame, codex_home):
        method, identifier = frame.get("method"), frame.get("id")
        if identifier is None:
            return []
        home = Path(codex_home)
        if method == "account/rateLimits/read":
            with reset.state_file(home, readonly=True) as state:
                result = json.loads(json.dumps(state["limits"]))
            if frame.get("params", {}).get("excludeResetCreditDetails"):
                result["rateLimitResetCredits"]["credits"] = None
        elif method == "account/rateLimitResetCredit/consume":
            try:
                result, disconnect = reset.consume_codex(home, frame.get("params", {}))
            except ValueError as error:
                return [{"id": identifier, "error": {"code": -32000, "message": str(error)}}]
            if disconnect:
                raise ConnectionAbortedError("fixture disconnected after consuming")
        elif method == "account/read":
            result = {"account": {"type": "chatgpt", "email": "codex@fixture.invalid", "planType": "plus"}}
        elif method == "turn/start":
            if frame.get("params", {}).get("threadId") != self.thread:
                return [{"id": identifier, "error": {"code": -32602, "message": "Prompt the fixture thread"}}]
            with reset.state_file(home, readonly=True) as state:
                allowed = state["limits"].get("ordinaryUsageAllowed") is True
            turn = str(uuid.uuid4())
            started = {"id": turn, "status": "inProgress", "items": []}
            responses = [{"id": identifier, "result": {"turn": started}},
                         {"method": "turn/started", "params": {"threadId": self.thread, "turn": started}}]
            if allowed:
                answer = {"id": f"fixture-reset-answer-{turn}", "type": "agentMessage",
                          "text": "The fixture task can continue.", "phase": None}
                responses.extend([
                    {"method": "item/started", "params": {"threadId": self.thread, "turnId": turn,
                                                           "item": {**answer, "text": ""}}},
                    {"method": "item/agentMessage/delta", "params": {"threadId": self.thread,
                                                                    "turnId": turn, "itemId": answer["id"],
                                                                    "delta": answer["text"]}},
                    {"method": "item/completed", "params": {"threadId": self.thread, "turnId": turn,
                                                             "item": answer}}])
                self.latest = {"id": turn, "status": "completed", "items": [answer]}
            else:
                self.latest = {"id": turn, "status": "failed", "items": [],
                               "error": {"message": "You've hit your usage limit", "codexErrorInfo": "usageLimitExceeded"}}
            responses.append({"method": "turn/completed", "params": {"threadId": self.thread, "turn": self.latest}})
            return responses
        else:
            return super().request(frame, codex_home)
        return [{"id": identifier, "result": result}]


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.167.0 (offline usage-resets fixture)")
        return
    root = reset.private_directory(Path(os.environ["CAGENTS_USAGE_RESETS_FIXTURE_ROOT"]))
    home = reset.private_directory(Path(os.environ["CODEX_HOME"]))
    if not home.is_relative_to(root) or not (root / ".cagents-reset-fixture").is_file():
        raise SystemExit("reset peer requires a marked fixture root and CODEX_HOME beneath it")
    scenario, request_lock = Scenario(), threading.Lock()
    args = sys.argv[1:]
    address = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--listen=")), None)
    if "--listen" in args:
        address = args[args.index("--listen") + 1]
    if not address:
        if "app-server" not in args:
            raise SystemExit("reset peer requires app-server or --version")
        for line in sys.stdin:
            try:
                responses = scenario.request(json.loads(line), str(home))
            except ConnectionAbortedError:
                return
            for response in responses:
                print(json.dumps(response), flush=True)
        return
    runtime = reset.private_directory(Path(os.environ["XDG_RUNTIME_DIR"]))
    if not address.startswith("unix://"):
        raise SystemExit("reset peer requires --listen unix://PATH")
    path = Path(address.removeprefix("unix://"))
    if not peer.transport.private_listener(path.parent, runtime) or path.exists() or path.is_symlink():
        raise SystemExit("reset peer requires a fresh private socket")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen()
    while True:
        connection, _ = listener.accept()

        def worker(connection=connection):
            try:
                peer.serve(connection, str(home), scenario, request_lock)
            except (EOFError, ConnectionError):
                pass
            finally:
                connection.close()

        threading.Thread(target=worker, daemon=True).start()


if __name__ == "__main__":
    main()
