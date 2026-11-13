#!/usr/bin/python3
"""Run the offline Codex peer with both installed-build socket conventions.

Older cagents builds allocate their app-server socket under an owned 0700
/tmp/cagents-codex-UUID directory. Accept that exact convention in addition to
sockets below the private runtime; the shared native-evidence peer stays strict.
Every launch, request and failure is recorded beneath the rehearsal root.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import stat
import sys
import threading
import time
import uuid

sys.dont_write_bytecode = True


def private_directory(path):
    """Validate the named directory itself, rather than following a symlink."""
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
        raise ValueError(f"fixture requires an owned private directory: {path}")
    return path.resolve(strict=True)


def configure_peer(root):
    """Restore fixture settings after native launches filter CAGENTS_* values."""
    path = root / "codex-fixture.json"
    if not path.exists() and not path.is_symlink():
        return
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor) as stream:
        settings = json.load(stream)
    names = {"pause": "CAGENTS_TOOL_ROWS_PAUSE",
             "never_confirm_interrupt": "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_INTERRUPT",
             "never_confirm_close": "CAGENTS_TOOL_ROWS_NEVER_CONFIRM_CLOSE"}
    if not isinstance(settings, dict) or settings.keys() - names.keys():
        raise ValueError("unknown Codex fixture setting")
    for key, value in settings.items():
        if key == "pause":
            if type(value) not in (int, float) or not 0 < value <= 300:
                raise ValueError("Codex fixture pause must be between 0 and 300 seconds")
            os.environ[names[key]] = str(value)
        else:
            if type(value) is not bool:
                raise ValueError("Codex fixture confirmation settings must be boolean")
            os.environ[names[key]] = "1" if value else "0"


def listener_path(args, runtime):
    address = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--listen=")), None)
    if "--listen" in args:
        address = args[args.index("--listen") + 1]
    if not address or not address.startswith("unix://"):
        raise ValueError("fixture requires --listen unix://PATH")
    path = Path(address[len("unix://"):])
    if not path.is_absolute() or path.name != "control.sock":
        raise ValueError("fixture requires an absolute control.sock path")
    parent = private_directory(path.parent)
    if parent != path.parent:
        raise ValueError("fixture socket directory must not traverse symlinks")
    if not parent.is_relative_to(runtime):
        # Do not accept arbitrary /tmp sockets or use a fake XDG_RUNTIME_DIR:
        # this is the old build's exact per-conversation, owned private shape.
        prefix = "cagents-codex-"
        identifier = parent.name.removeprefix(prefix)
        if (parent.parent != Path("/tmp") or not parent.name.startswith(prefix)
                or str(uuid.UUID(identifier)) != identifier):
            raise ValueError("fixture socket must be beneath its private runtime or an old-build /tmp/cagents-codex-UUID directory")
    if path.exists() or path.is_symlink():
        raise ValueError("fixture never replaces an existing socket")
    return path



def scenario_class(peer, storage, listener):
    """Persist app-server state only; this does not manufacture Codex rollouts.

    Listener indexes isolate independent account probes and conversations.
    Thread indexes let a successor listener resume an explicitly named thread.
    """
    storage.mkdir(mode=0o700, exist_ok=True)
    private_directory(storage)
    listener_key = hashlib.sha256(str(listener).encode()).hexdigest()
    index = storage / f"listener-{listener_key}.json"

    def thread_path(identifier):
        if str(uuid.UUID(identifier)) != identifier:
            raise ValueError("fixture thread identity must be a canonical UUID")
        return storage / f"thread-{identifier}.json"

    class RecordedScenario(peer.Scenario):
        def __init__(self):
            super().__init__()
            if index.exists() or index.is_symlink():
                self.load(index)

        def load(self, path):
            descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(descriptor) as stream:
                state = json.load(stream)
            if (not {"thread", "child", "latest"} <= set(state)
                    or set(state) - {"thread", "child", "latest", "child_latest", "interrupting"}):
                raise ValueError("invalid persisted rehearsal app-server state")
            thread_path(state["thread"])
            thread_path(state["child"])
            for turn in (state["latest"], state.get("child_latest")):
                if turn is not None and not isinstance(turn, dict):
                    raise ValueError("invalid persisted rehearsal turn")
            self.restore(state)

        def save(self):
            state = self.state()
            for path in (index, thread_path(self.thread), thread_path(self.child)):
                temporary = storage / f".state-{uuid.uuid4()}.tmp"
                descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                try:
                    with os.fdopen(descriptor, "w") as stream:
                        json.dump(state, stream, sort_keys=True)
                        stream.write("\n")
                    temporary.replace(path)
                finally:
                    temporary.unlink(missing_ok=True)

        def observe(self, notification, generations=None):
            accepted = super().observe(notification, generations)
            if accepted and notification.get("method") in (
                    "turn/started", "turn/completed", "item/started", "item/completed"):
                self.save()
            return accepted

        def request(self, frame, codex_home):
            if frame.get("method") == "thread/resume":
                requested = frame.get("params", {}).get("threadId")
                if requested not in (self.thread, self.child):
                    path = thread_path(requested)
                    if path.exists() or path.is_symlink():
                        self.load(path)
            responses = super().request(frame, codex_home)
            self.save()
            return responses

    return RecordedScenario



def serve_stdio(peer, home, record, incoming, outgoing):
    """Answer account probes without a listener or conversation persistence."""
    scenario = peer.Scenario()
    for line in incoming:
        frame = json.loads(line)
        if frame.get("method") == "account/rateLimits/read" and frame.get("id") is not None:
            # Explicitly synthetic allowance, never read from a real account.
            responses = [{"id": frame["id"], "result": {
                "ordinaryUsageAllowed": True,
                "rateLimits": {"limitId": "codex", "planType": "free",
                               "primary": {"usedPercent": 0, "windowDurationMins": 300}}}}]
        else:
            responses = scenario.request(frame, str(home))
        record("probe-request", method=frame.get("method"), request_id=frame.get("id"),
               errors=[response["error"] for response in responses if "error" in response])
        for response in responses:
            if "_sleep" in response:
                time.sleep(response["_sleep"])
            else:
                outgoing.write(json.dumps(response) + "\n")
                outgoing.flush()


def main():
    if "--version" in sys.argv:
        print("codex-cli 0.162.0 (offline update rehearsal fixture)")
        return
    root = private_directory(Path(os.environ["CAGENTS_TOOL_ROWS_FIXTURE_ROOT"]))
    configure_peer(root)
    logs = root / "logs"
    logs.mkdir(mode=0o700, exist_ok=True)
    # The harness may create logs with its ordinary umask; ROOT itself is 0700.
    if logs.is_symlink() or not logs.is_dir() or logs.stat().st_uid != os.getuid():
        raise ValueError("fixture logs must be an owned directory beneath its private root")
    log = logs / "codex-provider.jsonl"
    log_lock = threading.Lock()

    def record(event, **fields):
        value = {"event": event, "pid": os.getpid(), "time": time.time(), **fields}
        with log_lock:
            descriptor = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600)
            with os.fdopen(descriptor, "a") as stream:
                stream.write(json.dumps(value, sort_keys=True) + "\n")

    record("launch", argv=sys.argv[1:], codex_home=os.environ.get("CODEX_HOME"),
           runtime=os.environ.get("XDG_RUNTIME_DIR"))
    try:
        home = Path(os.environ["CODEX_HOME"]).resolve(strict=True)
        if not home.is_dir() or home.stat().st_uid != os.getuid():
            raise ValueError("fixture CODEX_HOME must be an owned directory")
        runtime = private_directory(Path(os.environ["XDG_RUNTIME_DIR"]))
        if not home.is_relative_to(root):
            raise ValueError("fixture CODEX_HOME must be beneath its private root")
        source = Path(__file__).with_name("codex-tool-rows.py")
        spec = importlib.util.spec_from_file_location("rehearsal_codex_peer", source)
        peer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(peer)
        args = sys.argv[1:]
        if not any(arg == "--listen" or arg.startswith("--listen=") for arg in args):
            if "app-server" not in args:
                raise ValueError("fixture requires app-server or --version")
            serve_stdio(peer, home, record, sys.stdin, sys.stdout)
            return
        path = listener_path(args, runtime)

        class LoggedScenario(scenario_class(peer, home / "rehearsal-app-server", path)):
            def observe(self, notification, generations=None):
                accepted = super().observe(notification, generations)
                if accepted and notification.get("method") in ("turn/started", "turn/completed"):
                    params = notification["params"]
                    record("notification", method=notification["method"], socket=str(path),
                           thread_id=params["threadId"], turn_id=params["turn"]["id"],
                           status=params["turn"]["status"], parent=params["threadId"] == self.thread)
                return accepted

            def request(self, frame, codex_home):
                responses = super().request(frame, codex_home)
                params = frame.get("params") or {}
                record("request", method=frame.get("method"), request_id=frame.get("id"),
                       socket=str(path), parent=params.get("threadId") == self.thread,
                       thread_id=params.get("threadId"), turn_id=params.get("turnId"),
                       errors=[response["error"] for response in responses if "error" in response])
                return responses

        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        listener.listen()
        record("listening", socket=str(path))
        scenario, request_lock = LoggedScenario(), threading.Lock()
        while True:
            connection, _ = listener.accept()

            def worker(connection=connection):
                try:
                    peer.serve(connection, str(home), scenario, request_lock)
                except (EOFError, ConnectionError):
                    record("disconnected")
                except Exception as error:
                    record("connection-error", error=str(error), exception=type(error).__name__)
                    print(f"rehearsal codex: {error}", file=sys.stderr, flush=True)
                finally:
                    connection.close()

            threading.Thread(target=worker, daemon=True).start()
    except Exception as error:
        record("failure", error=str(error), exception=type(error).__name__)
        raise


if __name__ == "__main__":
    main()
